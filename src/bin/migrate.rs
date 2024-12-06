//! This binary is used to migrate user IDs from base64 to hex encoding.
use std::{path::Path, str::FromStr};

use anyhow::{Context, Result};
use famedly_sync::{
	get_next_zitadel_user,
	user::{ExternalIdEncoding, User as SyncUser},
	zitadel::Zitadel as SyncZitadel,
	Config,
};
use tracing::level_filters::LevelFilter;

#[tokio::main]
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
	tracing::debug!("Note: External IDs are stored in the nick_name field of the user's profile in Zitadel, often referred to as uid.");

	// Zitadel
	let mut zitadel = SyncZitadel::new(&config).await?;

	// Detect external ID encoding based on a sample of users
	let users_sample = zitadel.get_users_sample().await?;
	let encoding = detect_database_encoding(users_sample);

	// Get a stream of all users
	let mut stream = zitadel.list_users()?;

	// Process each user
	while let Some((user, zitadel_id)) = get_next_zitadel_user(&mut stream, &mut zitadel).await? {
		tracing::info!(?user, "Starting migration for user");

		// Convert uid (=external ID, =nick_name) in Zitadel
		let updated_user = user.create_user_with_converted_external_id(encoding)?;
		tracing::debug!(?updated_user, "User updated");

		zitadel.update_user(&zitadel_id, &user, &updated_user).await?;

		tracing::info!(?user, ?updated_user, "User migrated");
	}

	tracing::info!("Migration completed.");
	Ok(())
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

	// Use thresholds to determine encoding
	let hex_ratio = f64::from(hex_count) / f64::from(total);
	let base64_ratio = f64::from(base64_count) / f64::from(total);

	if hex_ratio > 0.8 {
		ExternalIdEncoding::Hex
	} else if base64_ratio > 0.8 {
		ExternalIdEncoding::Base64
	} else {
		ExternalIdEncoding::Plain
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{ExternalIdEncoding, SyncUser};

	fn create_test_user(external_user_id: &str) -> SyncUser {
		SyncUser::new(
			"first name".to_owned(),
			"last name".to_owned(),
			"email@example.com".to_owned(),
			None,
			true,
			None,
			external_user_id.to_owned(),
		)
	}

	fn run_detection_test(user_ids: Vec<&str>, expected_encoding: ExternalIdEncoding) {
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
		original_id: &str,
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
		let user_ids = vec!["deadbeef", "cafebabe", "0123456789abcdef"];
		run_detection_test(user_ids, ExternalIdEncoding::Hex);
	}

	#[tokio::test]
	async fn test_all_base64() {
		// All users look like base64: "Y2FmZQ==", "Zm9v", "YmFy"
		// "Y2FmZQ==" decodes to "cafe"
		// "Zm9v" decodes to "foo"
		// "YmFy" decodes to "bar"
		// All are valid base64 and length % 4 == 0
		let user_ids = vec!["Y2FmZQ==", "Zm9v", "YmFy"];
		run_detection_test(user_ids, ExternalIdEncoding::Base64);
	}

	#[tokio::test]
	async fn test_mixed_ambiguous() {
		// Some look hex, all look base64
		let user_ids = vec!["cafebabe", "deadbeef", "beefcafe", "Y2FmZQ==", "Zm9v", "YmFy"];
		run_detection_test(user_ids, ExternalIdEncoding::Base64);
	}

	#[tokio::test]
	async fn test_edge_length_cases() {
		// "cafe" is ambiguous (valid hex and base64)
		// "cafeb" length is 5, not divisible by 2 or 4, so neither hex nor base64
		// "abc" length is 3, not divisible by 4, and 'c' is hex valid but odd length ->
		// not hex.
		let user_ids = vec!["cafe", "cafeb", "abc"];
		// "cafe" might count for both hex and base64, but "cafeb" and "abc" won't count
		// for either. Out of 3, maybe 1 counts as hex/base64 and 2 are plain. Ratios:
		// hex = 1/3 ≈ 0.33, base64 = 1/3 ≈ 0.33, both < 0.8.
		run_detection_test(user_ids, ExternalIdEncoding::Plain);
	}

	#[tokio::test]
	async fn test_invalid_characters() {
		// "zzz" is not hex. It's also not base64-safe (though 'z' is alphanumeric,
		// length=3 %4!=0) "+++" is not hex and length=3 not multiple of 4 for base64.
		let user_ids = vec!["zzz", "+++"];
		run_detection_test(user_ids, ExternalIdEncoding::Plain);
	}

	#[tokio::test]
	async fn test_near_threshold_hex() {
		// We want a scenario where hex ratio just hits 0.8.
		// Suppose we have 5 users total, 4 of which are hex. 4/5 = 0.8
		// If 4 pass as hex, and maybe 1 is something else.
		let user_ids = vec!["deadbeef", "cafebabe", "beefcafe", "0123456789abcdef", "plain_id"];
		// The 4 hex IDs will count, "plain_id" won't count for either.
		// hex_ratio = 4/5=0.8. The code uses `>` 0.8 not `>=`, so 0.8 is NOT greater
		// than 0.8. This test checks that boundary condition. Expected = Plain since
		// not strictly greater.
		run_detection_test(user_ids, ExternalIdEncoding::Plain);
	}

	#[tokio::test]
	async fn test_near_threshold_base64() {
		// Similar scenario for base64
		// 5 users, 4 are valid base64. 4/5=0.8 exactly.
		let user_ids = vec!["Y2FmZQ==", "Zm9v", "YmFy", "YQ==", "plain_id"];
		// Again hits exactly 0.8, not greater, expect Plain
		run_detection_test(user_ids, ExternalIdEncoding::Plain);
	}

	#[tokio::test]
	async fn test_empty_ids() {
		// Empty IDs should be skipped. Only one non-empty user which is hex.
		// hex_count=1, total=1 => ratio=1.0 > 0.8 => Hex
		let user_ids = vec!["", "", "cafebabe"];
		run_detection_test(user_ids, ExternalIdEncoding::Hex);
	}

	//
	// Conversion Tests
	//

	#[tokio::test]
	async fn test_conversion_hex_to_hex() {
		let original_id = "deadbeef";
		// Expected hex, no changes should be made.
		run_conversion_test(original_id, ExternalIdEncoding::Hex, "deadbeef");
	}

	#[tokio::test]
	async fn test_conversion_base64_to_hex() {
		let original_id = "Y2FmZQ=="; // "cafe"

		// Expected base64, we decode base64 => "cafe" and then hex encode the bytes of
		// "cafe". "cafe" as ASCII: 0x63 0x61 0x66 0x65 in hex is "63616665"
		run_conversion_test(original_id, ExternalIdEncoding::Base64, "63616665");
	}

	#[tokio::test]
	async fn test_conversion_plain_to_hex() {
		let original_id = "plain_id";
		// Expected plain without encoding, so just hex-encode the ASCII.
		// 'p' = 0x70, 'l' = 0x6c, 'a' = 0x61, 'i' = 0x69, 'n' = 0x6e, '_'=0x5f,
		// 'i'=0x69, 'd'=0x64 => "706c61696e5f6964"
		run_conversion_test(original_id, ExternalIdEncoding::Plain, "706c61696e5f6964");
	}
}
