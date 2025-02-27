//! This binary is used to migrate user IDs from base64 to hex encoding.
use std::{path::Path, str::FromStr};

use anyhow_ext::{Context, Result};
use famedly_sync::{
	Config, SkippedErrors,
	user::{ExternalIdEncoding, User as SyncUser},
	zitadel::Zitadel as SyncZitadel,
};
use futures::TryStreamExt;
use tracing::level_filters::LevelFilter;

#[tokio::main]
#[anyhow_trace::anyhow_trace]
async fn main() -> Result<()> {
	// Config
	let config_path =
		std::env::var("FAMEDLY_SYNC_CONFIG").unwrap_or_else(|_| "./config.yaml".to_owned());
	let config = Config::new(Path::new(&config_path))?;

	// Tracing
	let subscriber = tracing_subscriber::FmtSubscriber::builder()
		.with_max_level(
			config
				.log_level
				.as_ref()
				.map_or(Ok(LevelFilter::INFO), |s| LevelFilter::from_str(s))?,
		)
		.finish();
	tracing::subscriber::set_global_default(subscriber)
		.context("Setting default tracing subscriber failed")?;

	tracing::info!("Starting migration");
	tracing::debug!("Old external IDs will be base64 decoded and re-encoded as hex");
	tracing::debug!(
		"Note: External IDs are stored in the nick_name field of the user's profile in Zitadel, often referred to as uid."
	);

	let skipped_errors = SkippedErrors::new();

	// Zitadel
	let zitadel = SyncZitadel::new(config.zitadel, config.feature_flags, &skipped_errors).await?;

	// Detect external ID encoding based on a sample of users
	let users_sample = zitadel.get_users_sample().await?;
	let encoding = detect_database_encoding(users_sample);

	// Get a stream of all users and process each user
	zitadel
		.list_users()?
		.try_for_each_concurrent(Some(4), async |(zitadel_id, user)| {
			tracing::info!(?user, "Starting migration for user");

			// Convert uid (=external ID, =nick_name) in Zitadel
			let updated_user = user.create_user_with_converted_external_id(encoding)?;
			tracing::debug!(?updated_user, "User updated");

			zitadel.update_user(&zitadel_id, &user, &updated_user).await?;

			tracing::info!(?user, ?updated_user, "User migrated");
			Ok(())
		})
		.await?;

	tracing::info!("Migration completed.");
	skipped_errors.assert_no_errors()
}

/// Detects the most likely encoding scheme used across all user IDs
fn detect_database_encoding(users: Vec<SyncUser>) -> ExternalIdEncoding {
	// Count various encoding signatures
	let mut hex_count = 0;
	let mut base64_count = 0;
	let mut total = 0;

	for user in users {
		let nick_name = user.get_external_id();

		if nick_name.is_empty() {
			continue;
		}
		total += 1;

		// Check hex first (more restrictive)
		if nick_name.chars().all(|c| c.is_ascii_hexdigit()) && nick_name.len() % 2 == 0 {
			hex_count += 1;
		}

		// Check base64 signature
		if nick_name.len() % 4 == 0
			&& nick_name
				.chars()
				.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
		{
			base64_count += 1;
		}
	}

	// Return early if no valid samples
	if total == 0 {
		return ExternalIdEncoding::Ambiguous;
	}

	// Use thresholds to determine encoding
	let hex_ratio = f64::from(hex_count) / f64::from(total);
	let base64_ratio = f64::from(base64_count) / f64::from(total);

	// Require a strong majority (90%) for a format to be considered dominant
	// Also detect when both formats have significant presence
	match (hex_ratio, base64_ratio) {
		(h, _) if h > 0.9 => ExternalIdEncoding::Hex,
		(_, b) if b > 0.9 => ExternalIdEncoding::Base64,
		(h, b) if h > 0.2 && b > 0.2 => ExternalIdEncoding::Ambiguous, // Both formats present
		_ => ExternalIdEncoding::Ambiguous,                            // No clear dominant format
	}
}

#[cfg(test)]
mod tests {
	use base64::prelude::*;
	use famedly_sync::user;

	use super::*;
	use crate::{ExternalIdEncoding, SyncUser};

	enum UserId {
		Hex(String),
		Base64(String),
		Plain(String),
	}

	impl UserId {
		fn to_owned(&self) -> String {
			match self {
				UserId::Hex(id) => id.to_owned(),
				UserId::Base64(id) => id.to_owned(),
				UserId::Plain(id) => id.to_owned(),
			}
		}

		fn get_localpart(&self) -> String {
			match self {
				Self::Hex(id) => user::compute_famedly_uuid(
					&hex::decode(id).expect("Must be valid hex-encoded string"),
				),
				UserId::Base64(id) => user::compute_famedly_uuid(
					&BASE64_STANDARD.decode(id).expect("Must be valid base64-encoded string"),
				),
				UserId::Plain(id) => user::compute_famedly_uuid(id.as_bytes()),
			}
		}
	}

	fn create_test_user(external_user_id: UserId) -> SyncUser {
		SyncUser::new(
			"first name".to_owned(),
			"last name".to_owned(),
			"email@example.com".to_owned(),
			None,
			true,
			"Example User".to_owned(),
			external_user_id.to_owned(),
			external_user_id.get_localpart(),
		)
	}

	fn run_detection_test(user_ids: Vec<UserId>, expected_encoding: ExternalIdEncoding) {
		let users: Vec<SyncUser> = user_ids
			.into_iter()
			.map(create_test_user) // Assuming SyncUser::new(&str) exists
			.collect();

		let detected = detect_database_encoding(users);
		assert_eq!(
			detected, expected_encoding,
			"Expected {:?} but got {:?}",
			expected_encoding, detected
		);
	}

	fn run_conversion_test(
		original_id: UserId,
		expected_encoding: ExternalIdEncoding,
		expected_result: &str,
	) {
		let user = create_test_user(original_id);
		let migrated_user = user
			.create_user_with_converted_external_id(expected_encoding)
			.expect("Should successfully convert user");
		assert_eq!(
			migrated_user.get_external_id(),
			expected_result,
			"Unexpected conversion result"
		);
	}

	#[tokio::test]
	async fn test_all_hex() {
		// All users look like hex: "deadbeef", "cafebabe", "0123456789abcdef"
		let user_ids = vec![
			UserId::Hex("deadbeef".to_owned()),
			UserId::Hex("cafebabe".to_owned()),
			UserId::Hex("0123456789abcdef".to_owned()),
		];
		run_detection_test(user_ids, ExternalIdEncoding::Hex);
	}

	#[tokio::test]
	async fn test_all_base64() {
		// All users look like base64: "Y2FmZQ==", "Zm9v", "YmFy"
		// "Y2FmZQ==" decodes to "cafe"
		// "Zm9v" decodes to "foo"
		// "YmFy" decodes to "bar"
		// All are valid base64 and length % 4 == 0
		let user_ids = vec![
			UserId::Base64("Y2FmZQ==".to_owned()),
			UserId::Base64("Zm9v".to_owned()),
			UserId::Base64("YmFy".to_owned()),
		];
		run_detection_test(user_ids, ExternalIdEncoding::Base64);
	}

	#[tokio::test]
	async fn test_mixed_ambiguous() {
		// Some look hex, all look base64
		let user_ids = vec![
			UserId::Hex("cafebabe".to_owned()),
			UserId::Hex("deadbeef".to_owned()),
			UserId::Hex("beefcafe".to_owned()),
			UserId::Base64("Y2FmZQ==".to_owned()),
			UserId::Base64("Zm9v".to_owned()),
			UserId::Base64("YmFy".to_owned()),
		];
		run_detection_test(user_ids, ExternalIdEncoding::Base64);
	}

	#[tokio::test]
	async fn test_edge_length_cases() {
		// "cafe" is ambiguous (valid hex and base64)
		// "cafeb" length is 5, not divisible by 2 or 4, so neither hex nor base64
		// "abc" length is 3, not divisible by 4, and 'c' is hex valid but odd length ->
		// not hex.
		let user_ids = vec![
			UserId::Hex("cafe".to_owned()),
			UserId::Plain("cafeb".to_owned()),
			UserId::Plain("abc".to_owned()),
		];
		// "cafe" might count for both hex and base64, but "cafeb" and "abc" won't count
		// for either. Out of 3, maybe 1 counts as hex/base64 and 2 are plain. Ratios:
		// hex = 1/3 ≈ 0.33, base64 = 1/3 ≈ 0.33, both < 0.8.
		run_detection_test(user_ids, ExternalIdEncoding::Ambiguous);
	}

	#[tokio::test]
	async fn test_invalid_characters() {
		// "zzz" is not hex. It's also not base64-safe (though 'z' is alphanumeric,
		// length=3 %4!=0) "+++" is not hex and length=3 not multiple of 4 for base64.
		let user_ids = vec![UserId::Plain("zzz".to_owned()), UserId::Plain("+++".to_owned())];
		run_detection_test(user_ids, ExternalIdEncoding::Ambiguous);
	}

	#[tokio::test]
	async fn test_both_formats_significant() {
		// 10 total users:
		// - 3 hex (30%)
		// - 4 base64 (40%)
		// - 3 plain (30%)
		let user_ids = vec![
			// Hex format users (30%)
			UserId::Hex("deadbeef".to_owned()),
			UserId::Hex("cafebabe".to_owned()),
			UserId::Hex("12345678".to_owned()),
			// Base64 format users (40%)
			UserId::Base64("Y2FmZQ==".to_owned()), // "cafe"
			UserId::Base64("Zm9vYmFy".to_owned()), // "foobar"
			UserId::Base64("aGVsbG8=".to_owned()), // "hello"
			UserId::Base64("d29ybGQ=".to_owned()), // "world"
			// Plain format users (30%)
			UserId::Plain("plain_1".to_owned()),
			UserId::Plain("plain_2".to_owned()),
			UserId::Plain("plain_3".to_owned()),
		];

		// Both hex (30%) and base64 (40%) > 20% threshold
		// Neither > 90% threshold
		// Should detect as Ambiguous
		run_detection_test(user_ids, ExternalIdEncoding::Ambiguous);
	}

	#[tokio::test]
	async fn test_near_threshold_hex() {
		// Testing near 90% threshold for hex
		// 9 hex users and 1 plain = 90% exactly
		let user_ids = vec![
			UserId::Hex("deadbeef".to_owned()),
			UserId::Hex("cafebabe".to_owned()),
			UserId::Hex("beefcafe".to_owned()),
			UserId::Hex("12345678".to_owned()),
			UserId::Hex("87654321".to_owned()),
			UserId::Hex("abcdef12".to_owned()),
			UserId::Hex("34567890".to_owned()),
			UserId::Hex("98765432".to_owned()),
			UserId::Hex("fedcba98".to_owned()),
			UserId::Plain("plain_id".to_owned()),
		];
		// hex_ratio = 9/10 = 0.9
		// Code requires > 0.9, not >=, so this should be Ambiguous
		run_detection_test(user_ids, ExternalIdEncoding::Ambiguous);
	}

	#[tokio::test]
	async fn test_near_threshold_base64() {
		// Testing near 90% threshold for base64
		// 9 base64 users and 1 plain = 90% exactly
		let user_ids = vec![
			UserId::Base64("Y2FmZQ==".to_owned()), // cafe
			UserId::Base64("Zm9vYmFy".to_owned()), // foobar
			UserId::Base64("aGVsbG8=".to_owned()), // hello
			UserId::Base64("d29ybGQ=".to_owned()), // world
			UserId::Base64("dGVzdA==".to_owned()), // test
			UserId::Base64("YWJjZA==".to_owned()), // abcd
			UserId::Base64("eHl6Nzg=".to_owned()), // xyz78
			UserId::Base64("cXdlcnQ=".to_owned()), // qwert
			UserId::Base64("MTIzNDU=".to_owned()), // 12345
			UserId::Plain("plain_id".to_owned()),
		];
		// base64_ratio = 9/10 = 0.9
		// Code requires > 0.9, not >=, so this should be Ambiguous
		run_detection_test(user_ids, ExternalIdEncoding::Ambiguous);
	}

	#[tokio::test]
	async fn test_empty_ids() {
		// Empty IDs should be skipped. Only one non-empty user which is hex.
		// hex_count=1, total=1 => ratio=1.0 > 0.8 => Hex
		let user_ids = vec![
			UserId::Plain("".to_owned()),
			UserId::Plain("".to_owned()),
			UserId::Hex("cafebabe".to_owned()),
		];
		run_detection_test(user_ids, ExternalIdEncoding::Hex);
	}

	//
	// Conversion Tests
	//

	#[tokio::test]
	async fn test_conversion_hex_to_hex() {
		let original_id = UserId::Hex("deadbeef".to_owned());
		// Expected hex, no changes should be made.
		run_conversion_test(original_id, ExternalIdEncoding::Hex, "deadbeef");
	}

	#[tokio::test]
	async fn test_conversion_base64_to_hex() {
		let original_id = UserId::Base64("Y2FmZQ==".to_owned()); // "cafe"

		// Expected base64, we decode base64 => "cafe" and then hex encode the bytes of
		// "cafe". "cafe" as ASCII: 0x63 0x61 0x66 0x65 in hex is "63616665"
		run_conversion_test(original_id, ExternalIdEncoding::Base64, "63616665");
	}

	#[tokio::test]
	async fn test_conversion_plain_to_hex() {
		let original_id = UserId::Plain("plain_id".to_owned());
		// Expected plain without encoding, so just hex-encode the ASCII.
		// 'p' = 0x70, 'l' = 0x6c, 'a' = 0x61, 'i' = 0x69, 'n' = 0x6e, '_'=0x5f,
		// 'i'=0x69, 'd'=0x64 => "706c61696e5f6964"
		run_conversion_test(original_id, ExternalIdEncoding::Plain, "706c61696e5f6964");
	}

	#[tokio::test]
	async fn test_localpart_preservation() {
		// Test that migration preserves localpart values
		let original_user = SyncUser::new(
			"first name".to_owned(),
			"last name".to_owned(),
			"email@example.com".to_owned(),
			None,
			true,
			"Example User".to_owned(),
			"Y2FmZQ==".to_owned(),       // base64 encoded external ID
			"test.localpart".to_owned(), // localpart should be preserved
		);

		let migrated_user = original_user
			.create_user_with_converted_external_id(ExternalIdEncoding::Base64)
			.expect("Should successfully convert user");

		// External ID should be converted from base64 to hex
		assert_eq!(migrated_user.get_external_id(), hex::encode("cafe"));
		// Localpart should remain unchanged
		assert_eq!(migrated_user.get_localpart(), "test.localpart");
	}
}
