//! E2E integration tests

#![cfg(test)]
/// E2E integration tests
use std::collections::HashSet;

use base64::{Engine as _, engine::general_purpose};
use famedly_sync::{
	AttributeMapping, Config, FeatureFlag,
	csv_test_helpers::temp_csv_file,
	perform_sync,
	ukt_test_helpers::{
		ENDPOINT_PATH, OAUTH2_PATH, get_mock_server_url, prepare_endpoint_mock, prepare_oauth2_mock,
	},
};
use test_log::test;
use url::Url;
use uuid::{Uuid, uuid};
use wiremock::MockServer;
use zitadel_rust_client::v2::Zitadel;

mod common;

use common::{Ldap, ZitadelExt, cleanup_test_users, csv_config, ldap_config, ukt_config};

/// The Famedly UUID namespace to use to generate v5 UUIDs.
const FAMEDLY_NAMESPACE: Uuid = uuid!("d9979cff-abee-4666-bc88-1ec45a843fb8");

/// The Zitadel project role to assign to users.
const FAMEDLY_USER_ROLE: &str = "User";

/// Sync a single user with the given `uid`/`email` and assert that their
/// Zitadel nickname is the hex-encoded `uid`.
async fn verify_user_encoding(
	ldap: &mut Ldap,
	zitadel: &Zitadel,
	config: &Config,
	uid: &str,
	email: &str,
) -> Result<(), String> {
	let login_name = email;
	let expected_hex_id = hex::encode(uid.as_bytes());

	ldap.create_user("Test", "User", "TU", login_name, None, uid, false).await;

	perform_sync(config.clone()).await.map_err(|e| format!("Sync failed: {e}"))?;

	let user = zitadel
		.get_user_by_login_name(login_name)
		.await
		.map_err(|e| format!("Failed to get user: {e}"))?
		.ok_or_else(|| "User not found".to_owned())?;

	let human = user.human().ok_or_else(|| "User lacks human details".to_owned())?;
	let profile = human.profile().ok_or_else(|| "User lacks profile".to_owned())?;
	let nick_name = profile.nick_name().map_or("", String::as_str);

	if nick_name != expected_hex_id {
		return Err(format!(
			"ID mismatch for '{uid}': expected '{expected_hex_id}', got '{nick_name}'"
		));
	}
	Ok(())
}

/// Test cases for verifying correct user ID encoding (uid, email).
const USER_ID_ENCODING_CASES: &[(&str, &str)] = &[
	// Basic cases
	("simple123", "simple123@example.com"),
	("MiXed123Case", "mixed123case@example.com"),
	// Special characters
	("u.s-e_r", "user@example.com"),
	("123", "123@example.com"),
	// Unicode
	("üsernamÉ", "username@example.com"),
	("ὈΔΥΣΣΕΎΣ", "odysseus@example.com"),
	("Потребител", "potrebitel@example.com"),
	// Long string
	("ThisIsAVeryLongUsernameThatShouldStillWork123456789", "long@example.com"),
];

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_user_id_encoding() {
	let config = ldap_config().await;
	let mut ldap = Ldap::new().await;
	let zitadel = open_zitadel_connection().await;

	for (uid, email) in USER_ID_ENCODING_CASES {
		if let Err(error) = verify_user_encoding(&mut ldap, &zitadel, config, uid, email).await {
			panic!("Test failed for ID '{uid}': {error}");
		}
	}
}

/// A user fixture for the ID sync-ordering test.
struct TestUser<'a> {
	/// Raw LDAP uid, used as the external ID.
	uid: &'a str,
	/// Email address / login name.
	email: &'a str,
	/// Phone number.
	phone: &'a str,
}

/// Users covering a range of scripts to exercise sync ordering by external ID.
const TEST_USERS: &[TestUser] = &[
	TestUser { uid: "üser", email: "youser@example.com", phone: "+6666666666" },
	TestUser { uid: "aaa", email: "aaa@example.com", phone: "+1111111111" },
	TestUser { uid: "777", email: "777@example.com", phone: "+5555555555" },
	TestUser { uid: "bbb", email: "bbb@example.com", phone: "+3333333333" },
	TestUser { uid: "🦀", email: "crab@example.com", phone: "+1000000001" },
	TestUser { uid: "한글", email: "korean@example.com", phone: "+1000000002" },
	TestUser { uid: "عربي", email: "arabic@example.com", phone: "+1000000005" },
];

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_user_id_sync_ordering() {
	// Setup
	let config = ldap_config().await;
	let mut ldap = Ldap::new().await;
	let zitadel = open_zitadel_connection().await;

	// Create all users in LDAP
	for user in TEST_USERS {
		ldap.create_user("Test", "User", "TU", user.email, Some(user.phone), user.uid, false).await;
	}

	// Initial sync
	perform_sync(config.clone()).await.expect("Initial sync failed");

	// Verify all users exist with correct data
	for user in TEST_USERS {
		let expected_hex_id = hex::encode(user.uid.as_bytes());

		let zitadel_user = zitadel
			.get_user_by_login_name(user.email)
			.await
			.unwrap_or_else(|_| panic!("Failed to get user {}", user.email))
			.unwrap_or_else(|| panic!("User {} not found", user.email));

		let human = zitadel_user
			.human()
			.unwrap_or_else(|| panic!("User {} lacks human details", user.email));
		// Verify ID encoding
		let profile =
			human.profile().unwrap_or_else(|| panic!("User {} lacks profile", user.email));
		let nick_name = profile.nick_name().map_or("", String::as_str);
		assert_eq!(
			nick_name,
			expected_hex_id,
			"Wrong ID encoding for user {}, got '{:?}', expected '{:?}'",
			user.email,
			String::from_utf8_lossy(&hex::decode(nick_name).unwrap()),
			String::from_utf8_lossy(&hex::decode(expected_hex_id.clone()).unwrap())
		);

		// Verify phone number to ensure complete sync
		let phone = human
			.phone()
			.and_then(|phone| phone.phone())
			.map_or_else(|| panic!("User {} lacks phone", user.email), String::as_str);
		assert_eq!(phone, user.phone, "Wrong phone for user {}", user.email);
	}

	// Now update all users with new data
	for user in TEST_USERS {
		ldap.change_user(
			user.uid,
			// Just change the last_name (sn) attribute to the user's uid with SN prefix
			vec![("sn", HashSet::from([format!("SN{}", user.uid).as_str()]))],
		)
		.await;
	}

	// Sync again
	perform_sync(config.clone()).await.expect("Update sync failed");

	// Verify updates were applied in correct order
	for user in TEST_USERS {
		let zitadel_user = zitadel
			.get_user_by_login_name(user.email)
			.await
			.unwrap_or_else(|_| panic!("Failed to get updated user {}", user.email))
			.unwrap_or_else(|| panic!("Updated user {} not found", user.email));

		let human = zitadel_user
			.human()
			.unwrap_or_else(|| panic!("Updated user {} lacks human details", user.email));
		let profile =
			human.profile().unwrap_or_else(|| panic!("Updated user {} lacks profile", user.email));
		let last_name = profile.family_name().cloned().unwrap_or_default();
		assert_eq!(
			last_name,
			format!("SN{}", user.uid),
			"Wrong updated last_name for user {}",
			user.email
		);
	}

	// Delete users
	for user in TEST_USERS.iter().rev() {
		ldap.delete_user(user.uid).await;
	}

	// Final sync
	perform_sync(config.clone()).await.expect("Deletion sync failed");

	// Verify all users were NOT deleted (because they are not disabled, just
	// missing from LDAP)
	for user in TEST_USERS {
		let result = zitadel.get_user_by_login_name(user.email).await.expect("failed to find user");
		assert!(result.is_some());
	}

	// Recreate users again
	for user in TEST_USERS {
		ldap.create_user("Test", "User", "TU", user.email, Some(user.phone), user.uid, false).await;
	}

	// Finally disable users in reverse order
	for user in TEST_USERS.iter().rev() {
		ldap.change_user(user.uid, vec![("shadowFlag", HashSet::from(["514"]))]).await;
	}

	// Final sync
	perform_sync(config.clone()).await.expect("Deletion sync failed");

	// Verify all users were deleted in correct order
	for user in TEST_USERS {
		let result = zitadel
			.get_user_by_login_name(user.email)
			.await
			.expect("failed to query Zitadel users");

		assert!(result.is_none(), "User {} still exists after deletion", user.email);
	}
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_simple_sync() {
	let mut ldap = Ldap::new().await;
	ldap.create_user(
		"Bob",
		"Tables",
		"Bobby",
		"simple@famedly.de",
		Some("+12015550123"),
		"simple",
		false,
	)
	.await;

	let config = ldap_config().await;
	perform_sync(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("simple@famedly.de")
		.await
		.expect("could not query Zitadel users");

	assert!(user.is_some());

	let user = user.expect("could not find user");

	assert_eq!(user.username().map(String::as_str), Some("simple@famedly.de"));

	let human = user.human().expect("user lacks details");
	let profile = human.profile().expect("user lacks a profile");
	let phone = human.phone().expect("user lacks a phone number");
	let email = human.email().expect("user lacks an email address");

	assert_eq!(profile.given_name().map(String::as_str), Some("Bob"));
	assert_eq!(profile.family_name().map(String::as_str), Some("Tables"));
	assert_eq!(profile.display_name().map(String::as_str), Some("Tables, Bob"));
	assert_eq!(phone.phone().map(String::as_str), Some("+12015550123"));
	assert_eq!(phone.is_verified(), Some(&true));
	assert_eq!(email.email().map(String::as_str), Some("simple@famedly.de"));
	assert_eq!(email.is_verified(), Some(&true));

	let user_id = user.user_id().expect("user lacks an ID").clone();

	let preferred_username = zitadel
		.get_metadata(&config.zitadel.organization_id, &user_id, "preferred_username")
		.await
		.expect("could not get user metadata");
	assert_eq!(preferred_username, Some("Bobby".to_owned()));

	let uuid = Uuid::new_v5(&FAMEDLY_NAMESPACE, "simple".as_bytes());

	let localpart = zitadel
		.get_metadata(&config.zitadel.organization_id, &user_id, "localpart")
		.await
		.expect("could not get user metadata");
	assert_eq!(localpart, Some(uuid.to_string()));

	let role_keys = zitadel
		.user_role_keys(&config.zitadel.organization_id, &config.zitadel.project_id, &user_id)
		.await
		.expect("failed to get user grants");
	assert!(role_keys.iter().any(|key| key == FAMEDLY_USER_ROLE));
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_sync_disabled_user() {
	let mut ldap = Ldap::new().await;
	ldap.create_user(
		"Bob",
		"Tables",
		"Bobby",
		"disabled_user@famedly.de",
		Some("+12015550124"),
		"disabled_user",
		true,
	)
	.await;

	let config = ldap_config().await;
	perform_sync(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("disabled_user@famedly.de")
		.await
		.expect("could not query Zitadel users");

	assert!(user.is_none(), "disabled user was synced: {user:?}");
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_sso() {
	let mut config = ldap_config().await.clone();
	config.feature_flags.push(FeatureFlag::SsoLogin);

	let mut ldap = Ldap::new().await;
	ldap.create_user(
		"Bob",
		"Tables",
		"Bobby2",
		"sso@famedly.de",
		Some("+12015550124"),
		"sso",
		false,
	)
	.await;

	perform_sync(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("sso@famedly.de")
		.await
		.expect("could not query Zitadel users")
		.expect("could not find user");

	let user_id = user.user_id().expect("user lacks an ID");
	let idps = zitadel.user_idp_links(user_id).await.expect("could not get user idps");

	assert!(!idps.is_empty());
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_sync_change() {
	let mut ldap = Ldap::new().await;
	ldap.create_user(
		"Bob",
		"Tables",
		"Bobby2",
		"change@famedly.de",
		Some("+12015550124"),
		"change",
		false,
	)
	.await;

	let config = ldap_config().await;
	perform_sync(config.clone()).await.expect("syncing failed");

	ldap.change_user("change", vec![("telephoneNumber", HashSet::from(["+12015550123"]))]).await;

	perform_sync(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("change@famedly.de")
		.await
		.expect("could not query Zitadel users")
		.expect("missing Zitadel user");

	let human = user.human().expect("human user became a machine user?");
	assert_eq!(
		human.phone().and_then(|phone| phone.phone()).map(String::as_str),
		Some("+12015550123")
	);
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_sync_disable_and_reenable() {
	let mut ldap = Ldap::new().await;
	ldap.create_user(
		"Bob",
		"Tables",
		"Bobby2",
		"disable@famedly.de",
		Some("+12015550124"),
		"disable",
		false,
	)
	.await;

	let config = ldap_config().await;

	perform_sync(config.clone()).await.expect("syncing failed");
	let zitadel = open_zitadel_connection().await;
	let user = zitadel.get_user_by_login_name("disable@famedly.de").await.expect("query failed");
	assert!(user.is_some());

	ldap.change_user("disable", vec![("shadowFlag", HashSet::from(["514"]))]).await;
	perform_sync(config.clone()).await.expect("syncing failed");
	let user = zitadel.get_user_by_login_name("disable@famedly.de").await.expect("query failed");
	assert!(user.is_none());

	ldap.change_user("disable", vec![("shadowFlag", HashSet::from(["512"]))]).await;
	perform_sync(config.clone()).await.expect("syncing failed");
	let zitadel = open_zitadel_connection().await;
	let user = zitadel.get_user_by_login_name("disable@famedly.de").await.expect("query failed");
	assert!(user.is_some());
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_sync_email_change() {
	let mut ldap = Ldap::new().await;
	ldap.create_user(
		"Bob",
		"Tables",
		"Bobby2",
		"email_change@famedly.de",
		Some("+12015550124"),
		"email_change",
		false,
	)
	.await;

	let config = ldap_config().await;
	perform_sync(config.clone()).await.expect("syncing failed");

	ldap.change_user("email_change", vec![("mail", HashSet::from(["email_changed@famedly.de"]))])
		.await;

	perform_sync(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user =
		zitadel.get_user_by_login_name("email_changed@famedly.de").await.expect("query failed");

	assert!(user.is_some());
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_sync_safe_deletion() {
	let mut ldap = Ldap::new().await;
	ldap.create_user(
		"bob",
		"Tables",
		"Bobby3",
		"to_be_deleted@famedly.de",
		Some("+12015550124"),
		"to_be_deleted",
		false,
	)
	.await;

	ldap.create_user(
		"edward",
		"Riggs",
		"Edward",
		"to_be_disabled@famedly.de",
		Some("+12015550125"),
		"to_be_disabled",
		false,
	)
	.await;

	let config = ldap_config().await;
	perform_sync(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("to_be_deleted@famedly.de")
		.await
		.expect("failed to find user");
	assert!(user.is_some());
	let user = zitadel
		.get_user_by_login_name("to_be_disabled@famedly.de")
		.await
		.expect("failed to find user");
	assert!(user.is_some());

	ldap.delete_user("to_be_deleted").await;
	ldap.change_user("to_be_disabled", vec![("shadowFlag", HashSet::from(["514"]))]).await;

	perform_sync(config.clone()).await.expect("syncing failed");

	// Deleted LDAP users should persist in Zitadel for safety reasons
	let user = zitadel
		.get_user_by_login_name("to_be_deleted@famedly.de")
		.await
		.expect("failed to find user");
	assert!(user.is_some());

	// Disabled LDAP users should be deleted from Zitadel
	let user =
		zitadel.get_user_by_login_name("to_be_disabled@famedly.de").await.expect("query failed");
	assert!(user.is_none());

	assert!(
		zitadel
			.get_user_by_login_name("another_user@example.test")
			.await
			.expect("query failed")
			.is_some()
	);
	assert!(
		zitadel
			.get_user_by_login_name("projectless_user@example.test")
			.await
			.expect("query failed")
			.is_some()
	);
	assert!(
		zitadel
			.get_user_by_login_name("another_org_user@example.test")
			.await
			.expect("query failed")
			.is_some()
	);
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_user_no_localpart_skipped() {
	let config = ldap_config().await.clone();

	// Prepare Zitadel client
	let zitadel = open_zitadel_connection().await;

	// Create user in Zitadel
	zitadel
		.create_test_human_user(
			&config.zitadel.organization_id,
			"maxmustermann",
			"Test",
			"User",
			"User, Test",
			"deadbeef",
			"max@mustermann.de",
			"+12345678901",
		)
		.await
		.expect("Failed to create user");

	// Explicitly do not set a localpart for this user

	perform_sync(config.clone()).await.expect("syncing failed");

	zitadel
		.get_user_by_login_name("maxmustermann")
		.await
		.expect("user query failed")
		.expect("user should not have been deleted");
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_ldaps() {
	let mut config = ldap_config().await.clone();
	config
		.sources
		.ldap
		.as_mut()
		.map(|ldap_config| {
			ldap_config.url = Url::parse("ldaps://localhost:1636").expect("invalid ldaps url");
		})
		.expect("ldap must be configured for this test");

	let mut ldap = Ldap::new().await;
	ldap.create_user(
		"Bob",
		"Tables",
		"Bobby",
		"tls@famedly.de",
		Some("+12015550123"),
		"tls",
		false,
	)
	.await;

	perform_sync(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("tls@famedly.de")
		.await
		.expect("could not query Zitadel users");

	assert!(user.is_some());
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_ldaps_no_ident() {
	let mut config = ldap_config().await.clone();
	config
		.sources
		.ldap
		.as_mut()
		.map(|ldap_config| {
			ldap_config.url = Url::parse("ldaps://localhost:1636").expect("invalid ldaps url");
			if let Some(tls_config) = ldap_config.tls.as_mut() {
				tls_config.client_certificate = None;
				tls_config.client_key = None;
			}
		})
		.expect("ldap must be configured for this test");

	let mut ldap = Ldap::new().await;
	ldap.create_user(
		"Bob",
		"Tables",
		"Bobby",
		"servertls@famedly.de",
		Some("+12015550123"),
		"servertls",
		false,
	)
	.await;

	perform_sync(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("servertls@famedly.de")
		.await
		.expect("could not query Zitadel users");

	assert!(user.is_some());
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_ldaps_invalid_ident() {
	let mut config = ldap_config().await.clone();
	config
		.sources
		.ldap
		.as_mut()
		.map(|ldap_config| {
			ldap_config.url = Url::parse("ldaps://localhost:1636").expect("invalid ldaps url");
			if let Some(tls_config) = ldap_config.tls.as_mut() {
				tls_config.client_key = None;
			}
		})
		.expect("ldap must be configured for this test");

	let result = perform_sync(config.clone()).await;

	assert!(result.is_err());
	assert!(result.unwrap_err().chain().any(|source| {
		source.to_string().contains("Both client key *and* certificate must be specified")
	}));
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_ldaps_starttls() {
	let mut config = ldap_config().await.clone();
	config
		.sources
		.ldap
		.as_mut()
		.expect("ldap must be configured")
		.tls
		.as_mut()
		.expect("tls must be configured")
		.danger_use_start_tls = true;

	let mut ldap = Ldap::new().await;
	ldap.create_user(
		"Bob",
		"Tables",
		"Bobby",
		"starttls@famedly.de",
		Some("+12015550123"),
		"starttls2",
		false,
	)
	.await;

	perform_sync(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("starttls@famedly.de")
		.await
		.expect("could not query Zitadel users");

	assert!(user.is_some());
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_no_phone() {
	let mut ldap = Ldap::new().await;
	ldap.create_user("Bob", "Tables", "Bobby", "no_phone@famedly.de", None, "no_phone", false)
		.await;

	let config = ldap_config().await;
	perform_sync(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("no_phone@famedly.de")
		.await
		.expect("could not query Zitadel users");

	let user = user.expect("could not find user");

	let human = user.human().expect("user lacks details");
	// A missing phone number may be represented either as an absent phone
	// object or as an empty phone string, depending on the Zitadel response.
	let phone = human.phone().and_then(|phone| phone.phone()).map_or("", String::as_str);
	assert_eq!(phone, "");
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_sync_invalid_phone() {
	let mut ldap = Ldap::new().await;
	ldap.create_user(
		"John",
		"Good Phone",
		"Johnny1",
		"good_gone_bad_phone@famedly.de",
		Some("+12015550123"),
		"good_gone_bad_phone",
		false,
	)
	.await;

	ldap.create_user(
		"John",
		"Bad Phone",
		"Johnny2",
		"bad_phone_all_along@famedly.de",
		Some("abc"),
		"bad_phone_all_along",
		false,
	)
	.await;

	let config = ldap_config().await;
	perform_sync(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;

	let user = zitadel
		.get_user_by_login_name("good_gone_bad_phone@famedly.de")
		.await
		.expect("could not query Zitadel users");
	assert!(user.is_some());
	let user = user.expect("could not find user");
	let human = user.human().expect("user lacks details");
	assert_eq!(
		human.phone().and_then(|phone| phone.phone()).map(String::as_str),
		Some("+12015550123")
	);
	let user = zitadel
		.get_user_by_login_name("bad_phone_all_along@famedly.de")
		.await
		.expect("could not query Zitadel users");
	assert!(user.is_some());
	let user = user.expect("could not find user");
	let human = user.human().expect("user lacks details");
	let phone = human.phone().and_then(|phone| phone.phone()).map_or("", String::as_str);
	assert_eq!(phone, "");

	ldap.change_user("good_gone_bad_phone", vec![("telephoneNumber", HashSet::from(["abc"]))])
		.await;

	perform_sync(config.clone()).await.expect("syncing failed");

	let user = zitadel
		.get_user_by_login_name("good_gone_bad_phone@famedly.de")
		.await
		.expect("could not query Zitadel users");
	assert!(user.is_some());
	let user = user.expect("could not find user");
	let human = user.human().expect("user lacks details");
	let phone = human.phone().and_then(|phone| phone.phone()).map_or("", String::as_str);
	assert_eq!(phone, "");
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_binary_uid() {
	let mut config = ldap_config().await.clone();

	// Attribute uid (user_id) is configured as binary

	config
		.sources
		.ldap
		.as_mut()
		.expect("ldap must be configured for this test")
		.attributes
		.user_id = AttributeMapping::OptionalBinary {
		name: "userSMIMECertificate".to_owned(),
		is_binary: true,
	};

	let mut ldap = Ldap::new().await;

	// Create test user with binary ID
	let uid = "binary_user";
	let binary_uid = uid.as_bytes();
	ldap.create_user(
		"Binary",
		"User",
		"BinaryTest",
		"binary_id@famedly.de",
		Some("+12345678901"),
		uid, // Regular uid for DN
		false,
	)
	.await;

	// Set binary ID
	ldap.change_user(
		uid,
		vec![("userSMIMECertificate".as_bytes(), HashSet::from([uid.as_bytes()]))],
	)
	.await;

	perform_sync(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("binary_id@famedly.de")
		.await
		.expect("could not query Zitadel users")
		.expect("user not found");

	let human = user.human().expect("user lacks human details");
	let profile = human.profile().expect("user lacks profile");
	// The ID should be hex encoded in Zitadel
	assert_eq!(profile.nick_name().map(String::as_str), Some(hex::encode(binary_uid).as_str()));

	// Test update to a different binary ID that is valid UTF-8

	let new_binary_id = "updated_binary_user".as_bytes();
	ldap.change_user(
		uid,
		vec![("userSMIMECertificate".as_bytes(), HashSet::from([new_binary_id]))],
	)
	.await;

	perform_sync(config.clone()).await.expect("syncing failed");

	let user = zitadel
		.get_user_by_login_name("binary_id@famedly.de")
		.await
		.expect("could not query Zitadel users")
		.expect("user not found after update");

	let human = user.human().expect("user lost human details after update");
	let profile = human.profile().expect("user lacks profile");
	tracing::info!("profile: {profile:#?}");
	// Verify ID was updated
	assert_eq!(profile.nick_name().map(String::as_str), Some(hex::encode(new_binary_id).as_str()));

	// Test update to binary ID that is NOT valid UTF-8

	let invalid_binary_id = [0xA1, 0xA2];
	ldap.change_user(
		uid,
		vec![("userSMIMECertificate".as_bytes(), HashSet::from([invalid_binary_id.as_slice()]))],
	)
	.await;

	perform_sync(config.clone()).await.expect("syncing failed");

	let user = zitadel
		.get_user_by_login_name("binary_id@famedly.de")
		.await
		.expect("could not query Zitadel users")
		.expect("user not found after update");

	let human = user.human().expect("user lost human details after update");
	let profile = human.profile().expect("user lacks profile");
	// Verify ID was updated
	assert_eq!(
		profile.nick_name().map(String::as_str),
		Some(hex::encode(invalid_binary_id).as_str())
	);
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_dry_run() {
	let mut dry_run_config = ldap_config().await.clone();
	let config = ldap_config().await;
	dry_run_config.feature_flags.push(FeatureFlag::DryRun);

	let mut ldap = Ldap::new().await;
	ldap.create_user(
		"Bob",
		"Tables",
		"Bobby",
		"dry_run@famedly.de",
		Some("+12015550123"),
		"dry_run",
		false,
	)
	.await;

	let zitadel = open_zitadel_connection().await;

	// Assert the user does not sync, because this is a dry run
	perform_sync(dry_run_config.clone()).await.expect("syncing failed");
	assert!(
		zitadel.get_user_by_login_name("dry_run@famedly.de").await.expect("query failed").is_none()
	);

	// Actually sync the user so we can test other changes=
	perform_sync(config.clone()).await.expect("syncing failed");

	// Assert that a change in phone number does not sync
	ldap.change_user("dry_run", vec![("telephoneNumber", HashSet::from(["+12015550124"]))]).await;
	perform_sync(dry_run_config.clone()).await.expect("syncing failed");
	let user = zitadel
		.get_user_by_login_name("dry_run@famedly.de")
		.await
		.expect("could not query Zitadel users")
		.expect("could not find user");

	let human = user.human().expect("human user became a machine user?");
	assert_eq!(
		human.phone().and_then(|phone| phone.phone()).map(String::as_str),
		Some("+12015550123")
	);

	// Assert that disabling a user does not sync
	ldap.change_user("dry_run", vec![("shadowFlag", HashSet::from(["514"]))]).await;
	perform_sync(dry_run_config.clone()).await.expect("syncing failed");
	assert!(
		zitadel.get_user_by_login_name("dry_run@famedly.de").await.expect("query failed").is_some()
	);

	// Assert that a user deletion does not sync
	ldap.delete_user("dry_run").await;
	perform_sync(dry_run_config.clone()).await.expect("syncing failed");
	assert!(
		zitadel.get_user_by_login_name("dry_run@famedly.de").await.expect("query failed").is_some()
	);
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_sync_deactivated_only() {
	let mut ldap = Ldap::new().await;
	ldap.create_user(
		"Bob",
		"Tables",
		"Bobby2",
		"disable_disable_only@famedly.de",
		Some("+12015550124"),
		"disable_disable_only",
		false,
	)
	.await;

	ldap.create_user(
		"Bob",
		"Tables",
		"Bobby2",
		"changed_disable_only@famedly.de",
		Some("+12015550124"),
		"changed_disable_only",
		false,
	)
	.await;

	ldap.create_user(
		"Bob",
		"Tables",
		"Bobby2",
		"deleted_disable_only@famedly.de",
		Some("+12015550124"),
		"deleted_disable_only",
		false,
	)
	.await;

	ldap.create_user(
		"Bob",
		"Tables",
		"Bobby2",
		"reenabled_disable_only@famedly.de",
		Some("+12015550124"),
		"reenabled_disable_only",
		false,
	)
	.await;

	ldap.change_user("reenabled_disable_only", vec![("shadowFlag", HashSet::from(["514"]))]).await;

	let mut config = ldap_config().await.clone();
	perform_sync(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel.get_user_by_login_name("disable_disable_only@famedly.de").await;
	assert!(user.is_ok_and(|u| u.is_some()));
	let user = zitadel.get_user_by_login_name("changed_disable_only@famedly.de").await;
	assert!(user.is_ok_and(|u| u.is_some()));
	let user = zitadel.get_user_by_login_name("deleted_disable_only@famedly.de").await;
	assert!(user.is_ok_and(|u| u.is_some()));
	let user = zitadel.get_user_by_login_name("reenabled_disable_only@famedly.de").await;
	assert!(matches!(user, Ok(None)));

	config.feature_flags.push(FeatureFlag::DeactivateOnly);

	ldap.create_user(
		"Bob",
		"Tables",
		"Bobby2",
		"created_disable_only@famedly.de",
		Some("+12015550124"),
		"created_disable_only",
		false,
	)
	.await;

	ldap.change_user("disable_disable_only", vec![("shadowFlag", HashSet::from(["514"]))]).await;
	ldap.change_user(
		"changed_disable_only",
		vec![("telephoneNumber", HashSet::from(["+12015550123"]))],
	)
	.await;
	ldap.delete_user("deleted_disable_only").await;
	ldap.change_user("reenabled_disable_only", vec![("shadowFlag", HashSet::from(["512"]))]).await;
	perform_sync(config.clone()).await.expect("syncing failed");

	let user = zitadel.get_user_by_login_name("disable_disable_only@famedly.de").await;
	assert!(matches!(user, Ok(None)));
	let user = zitadel.get_user_by_login_name("created_disable_only@famedly.de").await;
	assert!(matches!(user, Ok(None)));
	let user = zitadel.get_user_by_login_name("deleted_disable_only@famedly.de").await;
	assert!(user.is_ok_and(|u| u.is_some()));
	let user = zitadel.get_user_by_login_name("reenabled_disable_only@famedly.de").await;
	assert!(matches!(user, Ok(None)));

	let user = zitadel
		.get_user_by_login_name("changed_disable_only@famedly.de")
		.await
		.expect("could not query Zitadel users")
		.expect("missing Zitadel user");

	let human = user.human().expect("human user became a machine user?");
	assert_eq!(
		human.phone().and_then(|phone| phone.phone()).map(String::as_str),
		Some("+12015550124")
	);
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_ukt_sync() {
	let mock_server = MockServer::start().await;

	prepare_oauth2_mock(&mock_server).await;
	prepare_endpoint_mock(&mock_server, "delete_me@famedly.de").await;

	let mut config = ukt_config().await.clone();

	config
		.sources
		.ukt
		.as_mut()
		.map(|ukt| {
			ukt.oauth2_url = get_mock_server_url(&mock_server, OAUTH2_PATH)
				.expect("Failed to get mock server URL");
			ukt.endpoint_url = get_mock_server_url(&mock_server, ENDPOINT_PATH)
				.expect("Failed to get mock server URL");
		})
		.expect("UKT configuration is missing");

	let zitadel = open_zitadel_connection().await;
	let user_id = zitadel
		.create_test_human_user(
			&config.zitadel.organization_id,
			"delete_me@famedly.de",
			"First",
			"Last",
			"First Last",
			"nickname",
			"delete_me@famedly.de",
			"+12015551111",
		)
		.await
		.expect("failed to create user");

	zitadel
		.set_user_metadata(
			&user_id,
			"localpart",
			"irrelevant",
			Some(config.zitadel.organization_id.clone()),
		)
		.await
		.expect("Failed to set user localpart");

	zitadel
		.set_user_metadata(
			&user_id,
			"preferred_username",
			"irrelevant",
			Some(config.zitadel.organization_id.clone()),
		)
		.await
		.expect("Failed to set user preferred name");

	let user = zitadel
		.get_user_by_login_name("delete_me@famedly.de")
		.await
		.expect("could not query Zitadel users");
	assert!(user.is_some());
	let user = user.expect("could not find user");
	assert_eq!(user.username().map(String::as_str), Some("delete_me@famedly.de"));

	perform_sync(config.clone()).await.expect("syncing failed");

	let user = zitadel.get_user_by_login_name("delete_me@famedly.de").await;
	assert!(matches!(user, Ok(None)));
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_csv_sync() {
	let mut config = csv_config().await.clone();

	perform_sync(config.clone()).await.expect("syncing failed");

	// Test user with localpart
	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("john.doe@example.com")
		.await
		.expect("could not query Zitadel users");
	let user = user.expect("could not find user");
	assert_eq!(user.username().map(String::as_str), Some("john.doe@example.com"));

	let human = user.human().expect("user lacks details");
	let profile = human.profile().expect("user lacks a profile");
	let phone = human.phone().expect("user lacks a phone number");
	let email = human.email().expect("user lacks an email address");

	assert_eq!(profile.given_name().map(String::as_str), Some("John"));
	assert_eq!(profile.family_name().map(String::as_str), Some("Doe"));
	assert_eq!(profile.display_name().map(String::as_str), Some("Doe, John"));
	assert_eq!(phone.phone().map(String::as_str), Some("+1111111111"));
	assert_eq!(phone.is_verified(), Some(&true));
	assert_eq!(email.email().map(String::as_str), Some("john.doe@example.com"));
	assert_eq!(email.is_verified(), Some(&true));

	let user_id = user.user_id().expect("user lacks an ID").clone();

	let preferred_username = zitadel
		.get_metadata(&config.zitadel.organization_id, &user_id, "preferred_username")
		.await
		.expect("could not get user metadata");
	assert_eq!(preferred_username, Some("john.doe@example.com".to_owned()));

	let localpart = zitadel
		.get_metadata(&config.zitadel.organization_id, &user_id, "localpart")
		.await
		.expect("could not get user metadata")
		.expect("missing localpart");
	assert_eq!(localpart, "john.doe", "Unexpected Zitadel userId for user without localpart");

	let role_keys = zitadel
		.user_role_keys(&config.zitadel.organization_id, &config.zitadel.project_id, &user_id)
		.await
		.expect("failed to get user grants");
	assert!(role_keys.iter().any(|key| key == FAMEDLY_USER_ROLE));

	// Test user without localpart (should use UUID)
	let user = zitadel
		.get_user_by_login_name("jane.smith@example.com")
		.await
		.expect("could not query Zitadel users");

	let user = user.expect("could not find user");
	assert_eq!(user.username().map(String::as_str), Some("jane.smith@example.com"));

	let uuid = Uuid::new_v5(&FAMEDLY_NAMESPACE, "jane.smith@example.com".as_bytes());
	let localpart = zitadel
		.get_metadata(
			&config.zitadel.organization_id,
			user.user_id().expect("user lacks an ID"),
			"localpart",
		)
		.await
		.expect("could not get user metadata");
	assert_eq!(localpart, Some(uuid.to_string()), "Localpart metadata should match userId");

	// Re-import an existing user to update (as checked by unique email)
	let csv_content = indoc::indoc! {r#"
    email,first_name,last_name,phone,localpart
    john.doe@example.com,Changed_Name,Changed_Surname,+2222222222,new.localpart
  "#};
	let _file = temp_csv_file(&mut config, csv_content);

	let user_id = zitadel
		.get_user_by_login_name("john.doe@example.com")
		.await
		.expect("could not query Zitadel users")
		.expect("Must be able to get the user pre-sync")
		.user_id()
		.expect("user lacks an ID")
		.clone();

	perform_sync(config.clone()).await.expect("syncing failed");

	let user = zitadel
		.get_user_by_login_name("john.doe@example.com")
		.await
		.expect("could not query Zitadel users");

	let user = user.expect("could not find user");
	assert_eq!(user.username().map(String::as_str), Some("john.doe@example.com"));
	assert_eq!(user.user_id(), Some(&user_id), "Zitadel userId should not change");

	let human = user.human().expect("user lacks details");
	let profile = human.profile().expect("user lacks a profile");
	let phone = human.phone().expect("user lacks a phone number");
	let email = human.email().expect("user lacks an email address");

	assert_eq!(profile.given_name().map(String::as_str), Some("Changed_Name"));
	assert_eq!(profile.family_name().map(String::as_str), Some("Changed_Surname"));
	assert_eq!(profile.display_name().map(String::as_str), Some("Changed_Surname, Changed_Name"));
	assert_eq!(phone.phone().map(String::as_str), Some("+2222222222"));
	assert_eq!(phone.is_verified(), Some(&true));
	assert_eq!(email.email().map(String::as_str), Some("john.doe@example.com"));
	assert_eq!(email.is_verified(), Some(&true));

	let localpart = zitadel
		.get_metadata(
			&config.zitadel.organization_id,
			user.user_id().expect("user lacks an ID"),
			"localpart",
		)
		.await
		.expect("could not get user metadata");
	assert_eq!(localpart, Some("john.doe".to_owned()));
}

/// Seed the LDAP fixtures used by [`test_e2e_ldap_with_ukt_sync`].
async fn seed_ldap_with_ukt_users(ldap: &mut Ldap) {
	for (last_name, uid) in [
		("To Be There", "to_be_there"),
		("Not To Be There", "not_to_be_there"),
		("Persist Deletion", "persist_deletion"),
		("Not To Be There Later", "not_to_be_there_later"),
		("To Be Changed", "to_be_changed"),
	] {
		ldap.create_user(
			"John",
			last_name,
			"Johnny",
			&format!("{uid}@famedly.de"),
			Some("+12015551111"),
			uid,
			false,
		)
		.await;
	}
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_ldap_with_ukt_sync() {
	let mock_server = MockServer::start().await;
	prepare_oauth2_mock(&mock_server).await;
	prepare_endpoint_mock(&mock_server, "not_to_be_there@famedly.de").await;

	// LDAP SYNC

	let mut ldap = Ldap::new().await;
	seed_ldap_with_ukt_users(&mut ldap).await;

	let ldap_config = ldap_config().await;
	perform_sync(ldap_config.clone()).await.expect("syncing failed");

	// UKT SYNC

	let mut ukt_config = ukt_config().await.clone();
	ukt_config
		.sources
		.ukt
		.as_mut()
		.map(|ukt| {
			ukt.oauth2_url = get_mock_server_url(&mock_server, OAUTH2_PATH)
				.expect("Failed to get mock server URL");
			ukt.endpoint_url = get_mock_server_url(&mock_server, ENDPOINT_PATH)
				.expect("Failed to get mock server URL");
		})
		.expect("UKT configuration is missing");

	perform_sync(ukt_config).await.expect("syncing failed");

	// VERIFY RESULTS OF SYNC

	let zitadel = open_zitadel_connection().await;

	// Should be deleted based on email
	let user = zitadel.get_user_by_login_name("not_to_be_there@famedly.de").await;
	assert!(matches!(user, Ok(None)));

	let user = zitadel
		.get_user_by_login_name("to_be_there@famedly.de")
		.await
		.expect("could not query Zitadel users");
	assert!(user.is_some());

	let user = zitadel
		.get_user_by_login_name("not_to_be_there_later@famedly.de")
		.await
		.expect("could not query Zitadel users");
	assert!(user.is_some());

	let user = zitadel
		.get_user_by_login_name("to_be_changed@famedly.de")
		.await
		.expect("could not query Zitadel users");
	assert!(user.is_some());
	let user = user.expect("could not find user");
	let human = user.human().expect("human user became a machine user?");
	assert_eq!(
		human.phone().and_then(|phone| phone.phone()).map(String::as_str),
		Some("+12015551111")
	);

	// UPDATES IN LDAP

	ldap.change_user("to_be_changed", vec![("telephoneNumber", HashSet::from(["+12015550123"]))])
		.await;
	ldap.delete_user("persist_deletion").await; // Should not be deleted
	ldap.change_user("not_to_be_there_later", vec![("shadowFlag", HashSet::from(["514"]))]).await; // Should be deleted

	perform_sync(ldap_config.clone()).await.expect("syncing failed");

	// VERIFY SECOND LDAP SYNC

	let user = zitadel
		.get_user_by_login_name("to_be_changed@famedly.de")
		.await
		.expect("could not query Zitadel users");
	assert!(user.is_some());
	let user = user.expect("could not find user");
	let human = user.human().expect("human user became a machine user?");
	assert_eq!(
		human.phone().and_then(|phone| phone.phone()).map(String::as_str),
		Some("+12015550123")
	);

	// Should not be deleted because it's just missing from LDAP but wasn't
	// disabled
	let user = zitadel
		.get_user_by_login_name("persist_deletion@famedly.de")
		.await
		.expect("could not query Zitadel users");
	assert!(user.is_some());

	// Should be deleted because it's disabled
	let user = zitadel.get_user_by_login_name("not_to_be_there_later@famedly.de").await;
	assert!(matches!(user, Ok(None)));
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_sso_linking() {
	let mut config = ldap_config().await.clone();
	config.feature_flags.push(FeatureFlag::SsoLogin);

	let mut ldap = Ldap::new().await;
	let test_email = "sso_link_test@famedly.de";
	let test_uid = "sso_link_test";
	ldap.create_user(
		"SSO",
		"LinkTest",
		"SSO Link",
		test_email,
		Some("+12015550199"),
		test_uid,
		false,
	)
	.await;

	perform_sync(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name(test_email)
		.await
		.expect("could not query Zitadel users")
		.expect("could not find user");

	let user_id = user.user_id().expect("user lacks an ID");
	let idps = zitadel.user_idp_links(user_id).await.expect("could not get user IDPs");

	assert!(!idps.is_empty(), "User should have IDP links");

	let idp = idps.first().expect("No IDP link found");
	assert_eq!(
		idp.idp_id().map(String::as_str),
		Some(config.zitadel.idp_id.as_ref().unwrap().as_str()),
		"IDP link should match configured IDP"
	);
	assert_eq!(
		idp.user_id().map(String::as_str),
		Some(test_uid),
		"IDP provided user id should match plain LDAP uid"
	);
	assert_eq!(
		idp.user_name().map(String::as_str),
		Some(test_email),
		"IDP provided user name should match test_email"
	);
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_migrate_base64_id() {
	let config = ldap_config().await;
	cleanup_test_users(config).await;

	// The uid for this test must be such that encodes to such base64 string
	// that doesn't look like hex. Otherwise, we need to have a sample of users
	// so the script determines encoding heuristically. This is tested later in
	// test_e2e_migrate_ambiguous_id
	let uid = "base64_test";
	let email = "migrate_test@famedly.de";
	let user_name = "migrate_user";

	// Base64-encoded External ID
	let base64_id = general_purpose::STANDARD.encode(uid);

	run_migration_test(config, email, user_name, base64_id.clone(), hex::encode(uid.as_bytes()))
		.await;
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_migrate_plain_id() {
	let config = ldap_config().await;
	cleanup_test_users(config).await;

	let uid = "plain_test";
	let email = "plain_test@famedly.de";
	let user_name = "plain_user";

	// Plain unencoded External ID
	let plain_id = uid.to_owned();

	run_migration_test(config, email, user_name, plain_id.clone(), hex::encode(uid.as_bytes()))
		.await;
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_migrate_hex_id() {
	let config = ldap_config().await;
	cleanup_test_users(config).await;

	let uid = "hex_test";
	let email = "hex_test@famedly.de";
	let user_name = "hex_user";

	// Already hex-encoded External ID
	let hex_id = hex::encode(uid.as_bytes());

	run_migration_test(config, email, user_name, hex_id.clone(), hex_id.clone()).await;
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_migrate_empty_id() {
	let config = ldap_config().await;

	let email = "empty_id@famedly.de";
	let user_name = "empty_user";

	// Empty External ID
	let empty_id = "".to_owned();

	run_migration_test(config, email, user_name, empty_id.clone(), empty_id.clone()).await;
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_migrate_ambiguous_id_as_base64() {
	let config = ldap_config().await;
	cleanup_test_users(config).await;

	let email = "ambiguous_id@famedly.de";
	let user_name = "ambiguous_user_one";

	// "cafe" is hex (ca fe) and also appears as valid base64
	// (all alphanumeric and length % 4 == 0)
	let ambiguous_id = "cafe".to_owned();

	// The migration logic should decide to treat it as hex when looking at it
	// on its own, because we check for hex first (it's a subset of base64 and
	// thus more restrictive)
	let expected_id = ambiguous_id.clone();
	run_migration_test(config, email, user_name, ambiguous_id, expected_id).await;

	// When we create some base64-only encoded values in the database, the
	// migration logic should heuristically find out, that the DB has external
	// IDs encoded with base64 and thus treat the ambiguous ID as base64 even
	// though it can be both base64 and hex
	let zitadel = open_zitadel_connection().await;
	let temp_user = zitadel
		.create_test_human_user(
			&config.zitadel.organization_id,
			"another_test",
			"Test",
			"User",
			"User, Test",
			"Z9FmZQ==", // base64 encoded
			"another_test@example.com",
			"+12345678901",
		)
		.await
		.expect("Failed to create user");

	zitadel
		.set_user_metadata(
			&temp_user,
			"localpart",
			"irrelevant",
			Some(config.zitadel.organization_id.clone()),
		)
		.await
		.expect("Failed to set user localpart");

	zitadel
		.set_user_metadata(
			&temp_user,
			"preferred_username",
			"irrelevant",
			Some(config.zitadel.organization_id.clone()),
		)
		.await
		.expect("Failed to set user preferred name");

	let user_name = "ambiguous_user_two";

	// "beefcafe" appears both as a valid hex and base64
	let ambiguous_id = "beefcafe".to_owned();

	let decoded =
		general_purpose::STANDARD.decode(&ambiguous_id).expect("Test ID should be valid base64");
	let expected_id = hex::encode(decoded);

	run_migration_test(config, email, user_name, ambiguous_id, expected_id).await;

	zitadel.delete_user(&temp_user).await.expect("Failed to delete user");
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_migrate_then_ldap_sync() {
	let config = ldap_config().await;
	cleanup_test_users(config).await;

	let uid = "migrate_sync_test_ldap";
	let email = "migrate_sync_ldap@famedly.de";
	let user_name = "migrate_sync_user_ldap";

	// Base64-encoded ID
	let base64_id = general_purpose::STANDARD.encode(uid);

	run_migration_test(config, email, user_name, base64_id.clone(), hex::encode(uid.as_bytes()))
		.await;

	// LDAP with updated First Name
	let config = ldap_config().await;
	let mut ldap = Ldap::new().await;
	ldap.create_user(
		"New First Name",
		"User",
		"User, Test", // !NOTE: Display name from LDAP isn't picked up by the sync
		email,
		Some("+12345678901"),
		uid,
		false,
	)
	.await;

	perform_sync(config.clone()).await.expect("LDAP sync failed");

	// Verify both External ID encoding and updated First Name
	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name(user_name)
		.await
		.expect("Failed to get user after LDAP sync")
		.expect("User not found after LDAP sync");

	let human = user
		.human()
		.unwrap_or_else(|| panic!("User lacks human details after LDAP sync for user '{email}'"));
	let profile = human.profile().expect("User lacks profile after LDAP sync");
	let expected_hex_id = hex::encode(uid.as_bytes());
	assert_eq!(
		profile.nick_name().map(String::as_str),
		Some(expected_hex_id.as_str()),
		"External ID not in hex encoding after LDAP sync for user '{email}'"
	);
	assert_eq!(
		profile.given_name().map(String::as_str),
		Some("New First Name"),
		"Fist name was not updated by LDAP sync for user '{email}'"
	);
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_migrate_dry_run() {
	let mut dry_run_config = ldap_config().await.clone();
	dry_run_config.feature_flags.push(FeatureFlag::DryRun);

	let uid = "plain_test_dry_run";
	let email = "plain_test_dry_run@famedly.de";
	let user_name = "plain_user_dry_run";
	let plain_id = uid.to_owned();

	run_migration_test(&dry_run_config, email, user_name, plain_id.clone(), plain_id).await;
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_sync_user_already_exists() {
	let config = ldap_config().await;
	let mut ldap = Ldap::new().await;
	let zitadel = open_zitadel_connection().await;

	let email = "user_exists_test@famedly.de";
	let initial_external_id = "user_v1";
	let recreated_external_id = "user_v2";
	let phone = "+12015550199";

	ldap.create_user("Test", "User", "User, Test", email, Some(phone), initial_external_id, false)
		.await;

	// Initial sync - should create user normally
	perform_sync(config.clone()).await.expect("Initial sync failed");

	// Verify user was created with initial external ID
	let user = zitadel
		.get_user_by_login_name(email)
		.await
		.expect("Failed to get user after initial sync")
		.expect("User not found after initial sync");

	let initial_zitadel_id = user.user_id().expect("user lacks an ID").clone();
	let human = user.human().expect("User is not human type");
	let profile = human.profile().expect("User lacks profile");
	assert_eq!(
		profile.nick_name().map(String::as_str),
		Some(hex::encode(initial_external_id.as_bytes()).as_str()),
		"Initial external ID not encoded correctly"
	);

	// Now change the external ID in LDAP by deleting and re-creating a new user
	// with the same email
	ldap.delete_user(initial_external_id).await;
	ldap.create_user(
		"Test",
		"User",
		"User, Test",
		email,
		Some(phone),
		recreated_external_id,
		false,
	)
	.await;

	// Second sync - should trigger "User already exists" error handling
	perform_sync(config.clone())
		.await
		.expect("Second sync failed - the 'Found existing user' flow should work");

	// Verify user was updated with new external ID
	let recreated_user = zitadel
		.get_user_by_login_name(email)
		.await
		.expect("Failed to get user after update sync")
		.expect("User not found after update sync");

	// Should be the same user (same Zitadel ID)
	assert_eq!(
		recreated_user.user_id(),
		Some(&initial_zitadel_id),
		"User ID changed - should be same user updated"
	);

	let human = recreated_user.human().expect("Updated user is not human type");
	let profile = human.profile().expect("Updated user lacks profile");
	assert_eq!(
		profile.nick_name().map(String::as_str),
		Some(hex::encode(recreated_external_id.as_bytes()).as_str()),
		"External ID was not updated correctly"
	);

	// Verify other fields are still correct
	assert_eq!(
		profile.given_name().map(String::as_str),
		Some("Test"),
		"First name should be preserved"
	);
	assert_eq!(
		profile.family_name().map(String::as_str),
		Some("User"),
		"Last name should be preserved"
	);

	assert_eq!(
		human.phone().and_then(|phone| phone.phone()).map(String::as_str),
		Some(phone),
		"Phone should be preserved"
	);
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_sync_user_already_exists_error_case() {
	let config = ldap_config().await;
	let mut ldap = Ldap::new().await;
	let zitadel = open_zitadel_connection().await;

	// Create a user directly in Zitadel (bypassing sync metadata)
	// This simulates a user that exists in Zitadel but doesn't have metadata
	// so will error out when the sync tools tries to find the user by email
	let email = "user_exists_error_case@famedly.de";
	let external_id = "user_error_case";
	let phone = "+12015550222";

	// Create user directly in Zitadel without localpart metadata
	let user_id = zitadel
		.create_test_human_user(
			&config.zitadel.organization_id,
			email,
			"Direct",
			"User",
			"User, Direct",
			&hex::encode("different_external_id".as_bytes()),
			email,
			phone,
		)
		.await
		.expect("Failed to create user directly in Zitadel");

	zitadel
		.add_user_grant(
			Some(config.zitadel.organization_id.clone()),
			&user_id,
			config.zitadel.project_id.clone(),
			None,
			Some(vec![FAMEDLY_USER_ROLE.to_owned()]),
		)
		.await
		.expect("Failed to create user grant");

	// Now create a user in LDAP with the same email but different external ID
	ldap.create_user("Test", "User", "User, Test", email, Some(phone), external_id, false).await;

	// This sync should trigger the "User already exists" error
	// But since the existing user lacks metadata (localpart),
	// it will fail to find it in get_users_by_email due to the Skippable error
	let result = perform_sync(config.clone()).await;

	match result {
		Ok(skipped_errors) => {
			// If the sync completed, verify that errors were skipped
			assert!(
				skipped_errors.assert_no_errors().is_err(),
				"Expected skipped errors but none occurred"
			);
		}
		Err(err) => {
			panic!("Expected sync to complete with skipped errors, but got fatal error: {err:?}");
		}
	}
}

/// Open a connection to the configured Zitadel backend
async fn open_zitadel_connection() -> Zitadel {
	let zitadel_config = ldap_config().await.zitadel.clone();
	Zitadel::new(zitadel_config.url, zitadel_config.key_file, None)
		.await
		.expect("failed to set up Zitadel client")
}

/// Helper function to create a user, run migration, and verify the encoding.
async fn run_migration_test(
	config: &Config,
	email: &str,
	user_name: &str,
	initial_nick_name: String,
	expected_nick_name: String,
) {
	// Prepare Zitadel client
	let zitadel = open_zitadel_connection().await;

	// Create user in Zitadel
	let user_id = zitadel
		.create_test_human_user(
			&config.zitadel.organization_id,
			user_name,
			"Test",
			"User",
			"User, Test",
			&initial_nick_name,
			email,
			"+12345678901",
		)
		.await
		.expect("Failed to create user");

	zitadel
		.set_user_metadata(
			&user_id,
			"localpart",
			"irrelevant",
			Some(config.zitadel.organization_id.clone()),
		)
		.await
		.expect("Failed to set user localpart");

	zitadel
		.set_user_metadata(
			&user_id,
			"preferred_username",
			"irrelevant",
			Some(config.zitadel.organization_id.clone()),
		)
		.await
		.expect("Failed to set user preferred name");

	zitadel
		.add_user_grant(
			Some(config.zitadel.organization_id.clone()),
			&user_id,
			config.zitadel.project_id.clone(),
			None,
			Some(vec![FAMEDLY_USER_ROLE.to_owned()]),
		)
		.await
		.expect("Failed to create user grant");

	// Run migration
	run_migration_binary(config.feature_flags.contains(&FeatureFlag::DryRun));

	// Verify External ID after migration
	let user = zitadel
		.get_user_by_login_name(user_name)
		.await
		.expect("Failed to get user")
		.expect("User not found");

	let human =
		user.human().unwrap_or_else(|| panic!("User is not of type Human for user '{email}'"));
	let profile = human.profile().expect("User lacks profile");
	assert_eq!(
		profile.nick_name().map(String::as_str),
		Some(expected_nick_name.as_str()),
		"Nickname encoding mismatch for user '{email}'"
	);
}

/// Helper function to run the migration binary.
fn run_migration_binary(is_dry_run: bool) {
	let temp_dir = tempfile::tempdir().unwrap();

	// Copy service-user.json to temp location
	let mut key_file_path = temp_dir.path().to_path_buf();
	key_file_path.push("zitadel");
	std::fs::create_dir_all(&key_file_path).unwrap();
	key_file_path.push("service-user.json");

	std::fs::copy("tests/environment/zitadel/service-user.json", &key_file_path).unwrap();

	// Read and modify config
	let mut config_path = std::env::current_dir().unwrap();
	config_path.push("tests/environment/config.yaml");
	let mut config_content = std::fs::read_to_string(&config_path).unwrap();

	// Update key_file path to be relative to temp config
	config_content = config_content.replace(
		"key_file: tests/environment/zitadel/service-user.json",
		&format!("key_file: {}", key_file_path.to_str().unwrap()),
	);

	// Add dry run flag if needed
	if is_dry_run {
		config_content = config_content.replace("feature_flags:", "feature_flags:\n  - dry_run");
	}

	// Write config to temp dir
	let config_file = temp_dir.path().join("config.yaml");
	std::fs::write(&config_file, &config_content).unwrap();

	// Run migration with temp config
	let status = std::process::Command::new(env!("CARGO_BIN_EXE_migrate"))
		.env("FAMEDLY_SYNC_CONFIG", config_file.to_str().unwrap())
		.status()
		.expect("Failed to execute migration binary");
	assert!(status.success(), "Migration binary exited with status: {status}");
}

/// Paging and attribute filtering must preserve every reconciled user.
#[test(tokio::test)]
async fn test_e2e_ldap_paging_and_attribute_filter_equivalence() {
	let mut ldap = Ldap::new().await;
	for n in 0..5 {
		ldap.create_user(
			"Paging",
			"Fixture",
			"Paging Fixture",
			&format!("paging-{n}@example.test"),
			None,
			&format!("paging-{n}"),
			false,
		)
		.await;
	}
	let mut config = ldap_config().await.clone();
	for (page_size, use_attribute_filter) in [(2, true), (0, false)] {
		let source = config.sources.ldap.as_mut().expect("LDAP config");
		source.page_size = page_size;
		source.use_attribute_filter = use_attribute_filter;
		perform_sync(config.clone())
			.await
			.expect("sync")
			.assert_no_errors()
			.expect("no skipped errors");
		let zitadel = open_zitadel_connection().await;
		for n in 0..5 {
			assert!(
				zitadel
					.get_user_by_login_name(&format!("paging-{n}@example.test"))
					.await
					.expect("lookup")
					.is_some()
			);
		}
	}
}
