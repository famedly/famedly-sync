#![cfg(test)]
#![allow(clippy::expect_fun_call)]
/// E2E integration tests
use std::{collections::HashSet, path::Path, time::Duration};

use famedly_sync::{
	csv_test_helpers::temp_csv_file,
	perform_sync,
	ukt_test_helpers::{
		get_mock_server_url, prepare_endpoint_mock, prepare_oauth2_mock, ENDPOINT_PATH, OAUTH2_PATH,
	},
	AttributeMapping, Config, FeatureFlag,
};
use ldap3::{Ldap as LdapClient, LdapConnAsync, LdapConnSettings, Mod};
use test_log::test;
use tokio::sync::OnceCell;
use url::Url;
use uuid::{uuid, Uuid};
use wiremock::MockServer;
use zitadel_rust_client::v1::{
	error::{Error as ZitadelError, TonicErrorCode},
	Email, Gender, ImportHumanUserRequest, Phone, Profile, UserType, Zitadel,
};

static CONFIG_WITH_LDAP: OnceCell<Config> = OnceCell::const_new();
static CONFIG_WITH_CSV: OnceCell<Config> = OnceCell::const_new();
static CONFIG_WITH_UKT: OnceCell<Config> = OnceCell::const_new();

/// The Famedly UUID namespace to use to generate v5 UUIDs.
const FAMEDLY_NAMESPACE: Uuid = uuid!("d9979cff-abee-4666-bc88-1ec45a843fb8");

/// The Zitadel project role to assign to users.
const FAMEDLY_USER_ROLE: &str = "User";

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_user_id_encoding() {
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

		perform_sync(config).await.map_err(|e| format!("Sync failed: {}", e))?;

		let user = zitadel
			.get_user_by_login_name(login_name)
			.await
			.map_err(|e| format!("Failed to get user: {}", e))?
			.ok_or_else(|| "User not found".to_owned())?;

		match user.r#type {
			Some(UserType::Human(user)) => {
				let profile = user.profile.ok_or_else(|| "User lacks profile".to_owned())?;

				if profile.nick_name != expected_hex_id {
					return Err(format!(
						"ID mismatch for '{}': expected '{}', got '{}'",
						uid, expected_hex_id, profile.nick_name
					));
				}
				Ok(())
			}
			_ => Err("User lacks human details".to_owned()),
		}
	}

	/// Test cases for verifying correct user ID encoding
	/// (uid, email)
	const TEST_CASES: &[(&str, &str)] = &[
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

	// Run all test cases
	let config = ldap_config().await;
	let mut ldap = Ldap::new().await;
	let zitadel = open_zitadel_connection().await;

	for (uid, email) in TEST_CASES {
		if let Err(error) = verify_user_encoding(&mut ldap, &zitadel, config, uid, email).await {
			panic!("Test failed for ID '{}': {}", uid, error);
		}
	}
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_user_id_sync_ordering() {
	struct TestUser<'a> {
		uid: &'a str,
		email: &'a str,
		phone: &'a str,
	}

	const TEST_USERS: &[TestUser] = &[
		TestUser { uid: "üser", email: "youser@example.com", phone: "+6666666666" },
		TestUser { uid: "aaa", email: "aaa@example.com", phone: "+1111111111" },
		TestUser { uid: "777", email: "777@example.com", phone: "+5555555555" },
		TestUser { uid: "bbb", email: "bbb@example.com", phone: "+3333333333" },
		TestUser { uid: "🦀", email: "crab@example.com", phone: "+1000000001" },
		TestUser { uid: "한글", email: "korean@example.com", phone: "+1000000002" },
		TestUser { uid: "عربي", email: "arabic@example.com", phone: "+1000000005" },
	];

	// Setup
	let config = ldap_config().await;
	let mut ldap = Ldap::new().await;
	let zitadel = open_zitadel_connection().await;

	// Create all users in LDAP
	for user in TEST_USERS {
		ldap.create_user("Test", "User", "TU", user.email, Some(user.phone), user.uid, false).await;
	}

	// Initial sync
	perform_sync(config).await.expect("Initial sync failed");

	// Verify all users exist with correct data
	for user in TEST_USERS {
		let expected_hex_id = hex::encode(user.uid.as_bytes());

		let zitadel_user = zitadel
			.get_user_by_login_name(user.email)
			.await
			.expect(&format!("Failed to get user {}", user.email))
			.expect(&format!("User {} not found", user.email));

		match zitadel_user.r#type {
			Some(UserType::Human(human)) => {
				// Verify ID encoding
				let profile = human.profile.expect(&format!("User {} lacks profile", user.email));
				assert_eq!(
					profile.nick_name,
					expected_hex_id,
					"Wrong ID encoding for user {}, got '{:?}', expected '{:?}'",
					user.email,
					String::from_utf8_lossy(&hex::decode(profile.nick_name.clone()).unwrap()),
					String::from_utf8_lossy(&hex::decode(expected_hex_id.clone()).unwrap())
				);

				// Verify phone number to ensure complete sync
				let phone = human.phone.expect(&format!("User {} lacks phone", user.email));
				assert_eq!(phone.phone, user.phone, "Wrong phone for user {}", user.email);
			}
			_ => panic!("User {} lacks human details", user.email),
		}
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
	perform_sync(config).await.expect("Update sync failed");

	// Verify updates were applied in correct order
	for user in TEST_USERS {
		let zitadel_user = zitadel
			.get_user_by_login_name(user.email)
			.await
			.expect(&format!("Failed to get updated user {}", user.email))
			.expect(&format!("Updated user {} not found", user.email));

		match zitadel_user.r#type {
			Some(UserType::Human(human)) => {
				let profile =
					human.profile.expect(&format!("Updated user {} lacks profile", user.email));
				let last_name = profile.last_name;
				assert_eq!(
					last_name,
					format!("SN{}", user.uid),
					"Wrong updated last_name for user {}",
					user.email
				);
			}
			_ => panic!("Updated user {} lacks human details", user.email),
		}
	}

	// Finally delete users in reverse order
	for user in TEST_USERS.iter().rev() {
		ldap.delete_user(user.uid).await;
	}

	// Final sync
	perform_sync(config).await.expect("Deletion sync failed");

	// Verify all users were deleted in correct order
	for user in TEST_USERS {
		let result = zitadel.get_user_by_login_name(user.email).await;

		assert!(
			matches!(
				result,
				Err(ZitadelError::TonicResponseError(status))
				if status.code() == TonicErrorCode::NotFound
			),
			"User {} still exists after deletion",
			user.email
		);
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
	perform_sync(config).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("simple@famedly.de")
		.await
		.expect("could not query Zitadel users");

	assert!(user.is_some());

	let user = user.expect("could not find user");

	assert_eq!(user.user_name, "simple@famedly.de");

	if let Some(UserType::Human(user)) = user.r#type {
		let profile = user.profile.expect("user lacks a profile");
		let phone = user.phone.expect("user lacks a phone number)");
		let email = user.email.expect("user lacks an email address");

		assert_eq!(profile.first_name, "Bob");
		assert_eq!(profile.last_name, "Tables");
		assert_eq!(profile.display_name, "Tables, Bob");
		assert_eq!(phone.phone, "+12015550123");
		assert!(phone.is_phone_verified);
		assert_eq!(email.email, "simple@famedly.de");
		assert!(email.is_email_verified);
	} else {
		panic!("user lacks details");
	}

	let preferred_username = zitadel
		.get_user_metadata(
			Some(config.zitadel.organization_id.clone()),
			&user.id,
			"preferred_username",
		)
		.await
		.expect("could not get user metadata");
	assert_eq!(preferred_username, Some("Bobby".to_owned()));

	let uuid = Uuid::new_v5(&FAMEDLY_NAMESPACE, "simple".as_bytes());

	let localpart = zitadel
		.get_user_metadata(Some(config.zitadel.organization_id.clone()), &user.id, "localpart")
		.await
		.expect("could not get user metadata");
	assert_eq!(localpart, Some(uuid.to_string()));

	let grants = zitadel
		.list_user_grants(&config.zitadel.organization_id, &user.id)
		.await
		.expect("failed to get user grants");

	let grant = grants.result.first().expect("no user grants found");
	assert!(grant.role_keys.clone().into_iter().any(|key| key == FAMEDLY_USER_ROLE));
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
	perform_sync(config).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel.get_user_by_login_name("disabled_user@famedly.de").await;

	if let Err(error) = user {
		match error {
			ZitadelError::TonicResponseError(status)
				if status.code() == TonicErrorCode::NotFound =>
			{
				return;
			}
			_ => {
				panic!("zitadel failed while searching for user: {}", error)
			}
		}
	} else {
		panic!("disabled user was synced: {:?}", user);
	}
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

	perform_sync(&config).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("sso@famedly.de")
		.await
		.expect("could not query Zitadel users")
		.expect("could not find user");

	let idps = zitadel.list_user_idps(user.id).await.expect("could not get user idps");

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
	perform_sync(config).await.expect("syncing failed");

	ldap.change_user("change", vec![("telephoneNumber", HashSet::from(["+12015550123"]))]).await;

	perform_sync(config).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("change@famedly.de")
		.await
		.expect("could not query Zitadel users")
		.expect("missing Zitadel user");

	match user.r#type {
		Some(UserType::Human(user)) => {
			assert_eq!(user.phone.expect("phone missing").phone, "+12015550123");
		}

		_ => panic!("human user became a machine user?"),
	}
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

	perform_sync(config).await.expect("syncing failed");
	let zitadel = open_zitadel_connection().await;
	let user = zitadel.get_user_by_login_name("disable@famedly.de").await;
	assert!(user.is_ok_and(|u| u.is_some()));

	ldap.change_user("disable", vec![("shadowFlag", HashSet::from(["514"]))]).await;
	perform_sync(config).await.expect("syncing failed");
	let user = zitadel.get_user_by_login_name("disable@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));

	ldap.change_user("disable", vec![("shadowFlag", HashSet::from(["512"]))]).await;
	perform_sync(config).await.expect("syncing failed");
	let zitadel = open_zitadel_connection().await;
	let user = zitadel.get_user_by_login_name("disable@famedly.de").await;
	assert!(user.is_ok_and(|u| u.is_some()));
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
	perform_sync(config).await.expect("syncing failed");

	ldap.change_user("email_change", vec![("mail", HashSet::from(["email_changed@famedly.de"]))])
		.await;

	perform_sync(config).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel.get_user_by_login_name("email_changed@famedly.de").await;

	assert!(user.is_ok());
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_sync_deletion() {
	let mut ldap = Ldap::new().await;
	ldap.create_user(
		"bob",
		"Tables",
		"Bobby3",
		"deleted@famedly.de",
		Some("+12015550124"),
		"deleted",
		false,
	)
	.await;

	let config = ldap_config().await;
	perform_sync(config).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user =
		zitadel.get_user_by_login_name("deleted@famedly.de").await.expect("failed to find user");
	assert!(user.is_some());

	ldap.delete_user("deleted").await;

	perform_sync(config).await.expect("syncing failed");

	let user = zitadel.get_user_by_login_name("deleted@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));
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

	perform_sync(&config).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("tls@famedly.de")
		.await
		.expect("could not query Zitadel users");

	assert!(user.is_some());
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

	perform_sync(&config).await.expect("syncing failed");

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
	perform_sync(config).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("no_phone@famedly.de")
		.await
		.expect("could not query Zitadel users");

	let user = user.expect("could not find user");

	if let Some(UserType::Human(user)) = user.r#type {
		// Yes, I know, the codegen for the zitadel crate is
		// pretty crazy. A missing phone number is represented as
		// Some(Phone { phone: "", is_phone_Verified: _ })
		assert_eq!(user.phone.expect("user lacks a phone number object").phone, "");
	} else {
		panic!("user lacks details");
	};
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
	perform_sync(config).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;

	let user = zitadel
		.get_user_by_login_name("good_gone_bad_phone@famedly.de")
		.await
		.expect("could not query Zitadel users");
	assert!(user.is_some());
	let user = user.expect("could not find user");
	match user.r#type {
		Some(UserType::Human(user)) => {
			assert_eq!(
				user.phone.expect("phone field should always be present").phone,
				"+12015550123"
			);
		}
		_ => panic!("user lacks details"),
	}
	let user = zitadel
		.get_user_by_login_name("bad_phone_all_along@famedly.de")
		.await
		.expect("could not query Zitadel users");
	assert!(user.is_some());
	let user = user.expect("could not find user");
	match user.r#type {
		Some(UserType::Human(user)) => {
			assert_eq!(user.phone.expect("phone field should always be present").phone, "");
		}
		_ => panic!("user lacks details"),
	}

	ldap.change_user("good_gone_bad_phone", vec![("telephoneNumber", HashSet::from(["abc"]))])
		.await;

	perform_sync(config).await.expect("syncing failed");

	let user = zitadel
		.get_user_by_login_name("good_gone_bad_phone@famedly.de")
		.await
		.expect("could not query Zitadel users");
	assert!(user.is_some());
	let user = user.expect("could not find user");
	match user.r#type {
		Some(UserType::Human(user)) => {
			assert_eq!(user.phone.expect("phone field should always be present").phone, "");
		}
		_ => panic!("user lacks details"),
	}
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

	perform_sync(&config).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("binary_id@famedly.de")
		.await
		.expect("could not query Zitadel users")
		.expect("user not found");

	match user.r#type {
		Some(UserType::Human(user)) => {
			let profile = user.profile.expect("user lacks profile");
			// The ID should be hex encoded in Zitadel
			assert_eq!(profile.nick_name, hex::encode(binary_uid));
		}
		_ => panic!("user lacks human details"),
	}

	// Test update to a different binary ID that is valid UTF-8

	let new_binary_id = "updated_binary_user".as_bytes();
	ldap.change_user(
		uid,
		vec![("userSMIMECertificate".as_bytes(), HashSet::from([new_binary_id]))],
	)
	.await;

	perform_sync(&config).await.expect("syncing failed");

	let user = zitadel
		.get_user_by_login_name("binary_id@famedly.de")
		.await
		.expect("could not query Zitadel users")
		.expect("user not found after update");

	match user.r#type {
		Some(UserType::Human(user)) => {
			let profile = user.profile.expect("user lacks profile");
			tracing::info!("profile: {:#?}", profile);
			// Verify ID was updated
			assert_eq!(profile.nick_name, hex::encode(new_binary_id));
		}
		_ => panic!("user lost human details after update"),
	}

	// Test update to binary ID that is NOT valid UTF-8

	let invalid_binary_id = [0xA1, 0xA2];
	ldap.change_user(
		uid,
		vec![("userSMIMECertificate".as_bytes(), HashSet::from([invalid_binary_id.as_slice()]))],
	)
	.await;

	perform_sync(&config).await.expect("syncing failed");

	let user = zitadel
		.get_user_by_login_name("binary_id@famedly.de")
		.await
		.expect("could not query Zitadel users")
		.expect("user not found after update");

	match user.r#type {
		Some(UserType::Human(user)) => {
			let profile = user.profile.expect("user lacks profile");
			// Verify ID was updated
			assert_eq!(profile.nick_name, hex::encode(invalid_binary_id));
		}
		_ => panic!("user lost human details after update"),
	}
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_binary_preferred_username() {
	let mut config = ldap_config().await.clone();

	// Attribute preferred_username is configured as binary but shouldn't be

	config
		.sources
		.ldap
		.as_mut()
		.expect("ldap must be configured for this test")
		.attributes
		.preferred_username = AttributeMapping::OptionalBinary {
		name: "userSMIMECertificate".to_owned(),
		is_binary: true,
	};

	let mut ldap = Ldap::new().await;
	ldap.create_user(
		"BobFail",
		"TablesFail",
		"BobbyFail",
		"binary_fail@famedly.de",
		Some("+12015550123"),
		"binary_fail",
		false,
	)
	.await;

	// Test with a valid UTF-8 binary attribute

	ldap.change_user(
		"binary_fail",
		vec![("userSMIMECertificate".as_bytes(), HashSet::from(["new_binary_fail".as_bytes()]))],
	)
	.await;

	let result = tokio::spawn({
		let config = config.clone();
		async move { perform_sync(&config).await }
	})
	.await;

	match result {
		Ok(sync_result) => {
			assert!(sync_result.is_err());
			let error = sync_result.unwrap_err();
			assert!(error.to_string().contains("Failed to query users from LDAP"));
			if let Some(cause) = error.source() {
				assert!(cause.to_string().contains("Binary values are not accepted"));
				assert!(cause.to_string().contains("attribute `userSMIMECertificate`"));
			} else {
				panic!("Expected error to have a cause");
			}
		}
		Err(join_error) if join_error.is_panic() => {
			panic!("perform_sync panicked unexpectedly: {}", join_error);
		}
		Err(e) => {
			panic!("unexpected error: {}", e);
		}
	}

	// Test with an invalid UTF-8 binary attribute

	ldap.change_user(
		"binary_fail",
		vec![("userSMIMECertificate".as_bytes(), HashSet::from([[0xA0, 0xA1].as_slice()]))],
	)
	.await;

	let result = tokio::spawn({
		let config = config.clone();
		async move { perform_sync(&config).await }
	})
	.await;

	match result {
		Ok(sync_result) => {
			assert!(sync_result.is_err());
			let error = sync_result.unwrap_err();
			assert!(error.to_string().contains("Failed to query users from LDAP"));
			if let Some(cause) = error.source() {
				assert!(cause.to_string().contains("Binary values are not accepted"));
				assert!(cause.to_string().contains("attribute `userSMIMECertificate`"));
			} else {
				panic!("Expected error to have a cause");
			}
		}
		Err(join_error) if join_error.is_panic() => {
			panic!("perform_sync panicked unexpectedly: {}", join_error);
		}
		Err(e) => {
			panic!("unexpected error: {}", e);
		}
	}
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
	perform_sync(&dry_run_config).await.expect("syncing failed");
	assert!(zitadel.get_user_by_login_name("dry_run@famedly.de").await.is_err_and(
		|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound),
	));

	// Actually sync the user so we can test other changes=
	perform_sync(config).await.expect("syncing failed");

	// Assert that a change in phone number does not sync
	ldap.change_user("dry_run", vec![("telephoneNumber", HashSet::from(["+12015550124"]))]).await;
	perform_sync(&dry_run_config).await.expect("syncing failed");
	let user = zitadel
		.get_user_by_login_name("dry_run@famedly.de")
		.await
		.expect("could not query Zitadel users")
		.expect("could not find user");

	assert!(
		matches!(user.r#type, Some(UserType::Human(user)) if user.phone.as_ref().expect("phone missing").phone == "+12015550123")
	);

	// Assert that disabling a user does not sync
	ldap.change_user("dry_run", vec![("shadowFlag", HashSet::from(["514"]))]).await;
	perform_sync(&dry_run_config).await.expect("syncing failed");
	assert!(zitadel
		.get_user_by_login_name("dry_run@famedly.de")
		.await
		.is_ok_and(|user| user.is_some()));

	// Assert that a user deletion does not sync
	ldap.delete_user("dry_run").await;
	perform_sync(&dry_run_config).await.expect("syncing failed");
	assert!(zitadel
		.get_user_by_login_name("dry_run@famedly.de")
		.await
		.is_ok_and(|user| user.is_some()));
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
	perform_sync(&config).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel.get_user_by_login_name("disable_disable_only@famedly.de").await;
	assert!(user.is_ok_and(|u| u.is_some()));
	let user = zitadel.get_user_by_login_name("changed_disable_only@famedly.de").await;
	assert!(user.is_ok_and(|u| u.is_some()));
	let user = zitadel.get_user_by_login_name("deleted_disable_only@famedly.de").await;
	assert!(user.is_ok_and(|u| u.is_some()));
	let user = zitadel.get_user_by_login_name("reenabled_disable_only@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));

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
	perform_sync(&config).await.expect("syncing failed");

	let user = zitadel.get_user_by_login_name("disable_disable_only@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));
	let user = zitadel.get_user_by_login_name("created_disable_only@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));
	let user = zitadel.get_user_by_login_name("deleted_disable_only@famedly.de").await;
	assert!(user.is_ok_and(|u| u.is_some()));
	let user = zitadel.get_user_by_login_name("reenabled_disable_only@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));

	let user = zitadel
		.get_user_by_login_name("changed_disable_only@famedly.de")
		.await
		.expect("could not query Zitadel users")
		.expect("missing Zitadel user");

	match user.r#type {
		Some(UserType::Human(user)) => {
			assert_eq!(user.phone.expect("phone missing").phone, "+12015550124");
		}

		_ => panic!("human user became a machine user?"),
	}
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

	let user = ImportHumanUserRequest {
		user_name: "delete_me@famedly.de".to_owned(),
		profile: Some(Profile {
			first_name: "First".to_owned(),
			last_name: "Last".to_owned(),
			display_name: "First Last".to_owned(),
			gender: Gender::Unspecified.into(),
			nick_name: "nickname".to_owned(),
			preferred_language: String::default(),
		}),
		email: Some(Email { email: "delete_me@famedly.de".to_owned(), is_email_verified: true }),
		phone: Some(Phone { phone: "+12015551111".to_owned(), is_phone_verified: true }),
		password: String::default(),
		hashed_password: None,
		password_change_required: false,
		request_passwordless_registration: false,
		otp_code: String::default(),
		idps: vec![],
	};

	let zitadel = open_zitadel_connection().await;
	zitadel
		.create_human_user(&config.zitadel.organization_id, user)
		.await
		.expect("failed to create user");

	let user = zitadel
		.get_user_by_login_name("delete_me@famedly.de")
		.await
		.expect("could not query Zitadel users");
	assert!(user.is_some());
	let user = user.expect("could not find user");
	assert_eq!(user.user_name, "delete_me@famedly.de");

	perform_sync(&config).await.expect("syncing failed");

	let user = zitadel.get_user_by_login_name("delete_me@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));
}

// Currently fails because CSV uses non-hex encoded IDs, need to think
// about how to fit this into the overall workflow
#[ignore]
#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_csv_sync() {
	let mut config = csv_config().await.clone();

	perform_sync(&config).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("john.doe@example.com")
		.await
		.expect("could not query Zitadel users");

	assert!(user.is_some());

	let user = user.expect("could not find user");

	assert_eq!(user.user_name, "john.doe@example.com");

	if let Some(UserType::Human(user)) = user.r#type {
		let profile = user.profile.expect("user lacks a profile");
		let phone = user.phone.expect("user lacks a phone number");
		let email = user.email.expect("user lacks an email address");

		assert_eq!(profile.first_name, "John");
		assert_eq!(profile.last_name, "Doe");
		assert_eq!(profile.display_name, "Doe, John");
		assert_eq!(phone.phone, "+1111111111");
		assert!(phone.is_phone_verified);
		assert_eq!(email.email, "john.doe@example.com");
		assert!(email.is_email_verified);
	} else {
		panic!("user lacks details");
	}

	let preferred_username = zitadel
		.get_user_metadata(
			Some(config.zitadel.organization_id.clone()),
			&user.id,
			"preferred_username",
		)
		.await
		.expect("could not get user metadata");
	assert_eq!(preferred_username, Some("john.doe@example.com".to_owned()));

	let uuid = Uuid::new_v5(&FAMEDLY_NAMESPACE, "john.doe@example.com".as_bytes());

	let localpart = zitadel
		.get_user_metadata(Some(config.zitadel.organization_id.clone()), &user.id, "localpart")
		.await
		.expect("could not get user metadata");
	assert_eq!(localpart, Some(uuid.to_string()));

	let grants = zitadel
		.list_user_grants(&config.zitadel.organization_id, &user.id)
		.await
		.expect("failed to get user grants");

	let grant = grants.result.first().expect("no user grants found");
	assert!(grant.role_keys.clone().into_iter().any(|key| key == FAMEDLY_USER_ROLE));

	// Re-import an existing user to update (as checked by unique email)
	let csv_content = indoc::indoc! {r#"
    email,first_name,last_name,phone
    john.doe@example.com,Changed_Name,Changed_Surname,+2222222222
  "#};
	let _file = temp_csv_file(&mut config, csv_content);
	perform_sync(&config).await.expect("syncing failed");

	let user = zitadel
		.get_user_by_login_name("john.doe@example.com")
		.await
		.expect("could not query Zitadel users");
	assert!(user.is_some());
	let user = user.expect("could not find user");
	assert_eq!(user.user_name, "john.doe@example.com");
	if let Some(UserType::Human(user)) = user.r#type {
		let profile = user.profile.expect("user lacks a profile");
		let phone = user.phone.expect("user lacks a phone number");
		let email = user.email.expect("user lacks an email address");

		assert_eq!(profile.first_name, "Changed_Name");
		assert_eq!(profile.last_name, "Changed_Surname");
		assert_eq!(profile.display_name, "Changed_Surname, Changed_Name");
		assert_eq!(phone.phone, "+2222222222");
		assert!(phone.is_phone_verified);
		assert_eq!(email.email, "john.doe@example.com");
		assert!(email.is_email_verified);
	} else {
		panic!("user lacks details");
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
	ldap.create_user(
		"John",
		"To Be There",
		"Johnny",
		"to_be_there@famedly.de",
		Some("+12015551111"),
		"to_be_there",
		false,
	)
	.await;

	ldap.create_user(
		"John",
		"Not To Be There",
		"Johnny",
		"not_to_be_there@famedly.de",
		Some("+12015551111"),
		"not_to_be_there",
		false,
	)
	.await;

	ldap.create_user(
		"John",
		"Not To Be There Later",
		"Johnny",
		"not_to_be_there_later@famedly.de",
		Some("+12015551111"),
		"not_to_be_there_later",
		false,
	)
	.await;

	ldap.create_user(
		"John",
		"To Be Changed",
		"Johnny",
		"to_be_changed@famedly.de",
		Some("+12015551111"),
		"to_be_changed",
		false,
	)
	.await;

	let ldap_config = ldap_config().await.clone();
	perform_sync(&ldap_config).await.expect("syncing failed");

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

	perform_sync(&ukt_config).await.expect("syncing failed");

	// VERIFY RESULTS OF SYNC

	let zitadel = open_zitadel_connection().await;

	let user = zitadel.get_user_by_login_name("not_to_be_there@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error,
	ZitadelError::TonicResponseError(status) if status.code() ==
	TonicErrorCode::NotFound)));

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
	match user.r#type {
		Some(UserType::Human(user)) => {
			assert_eq!(user.phone.expect("phone missing").phone, "+12015551111");
		}
		_ => panic!("human user became a machine user?"),
	}

	// UPDATES IN LDAP

	ldap.change_user("to_be_changed", vec![("telephoneNumber", HashSet::from(["+12015550123"]))])
		.await;
	ldap.delete_user("not_to_be_there_later").await;

	perform_sync(&ldap_config).await.expect("syncing failed");

	// VERIFY SECOND LDAP SYNC

	let user = zitadel
		.get_user_by_login_name("to_be_changed@famedly.de")
		.await
		.expect("could not query Zitadel users");
	assert!(user.is_some());
	let user = user.expect("could not find user");
	match user.r#type {
		Some(UserType::Human(user)) => {
			assert_eq!(user.phone.expect("phone missing").phone, "+12015550123");
		}
		_ => panic!("human user became a machine user?"),
	}
	let user = zitadel.get_user_by_login_name("not_to_be_there_later@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));
}

struct Ldap {
	client: LdapClient,
}

impl Ldap {
	async fn new() -> Self {
		let config = ldap_config().await.clone();
		let mut settings = LdapConnSettings::new();

		if let Some(ref ldap_config) = config.sources.ldap {
			settings = settings.set_conn_timeout(Duration::from_secs(ldap_config.timeout));
			settings = settings.set_starttls(false);

			let (conn, mut ldap) =
				LdapConnAsync::from_url_with_settings(settings, &ldap_config.url)
					.await
					.expect("could not connect to ldap");

			ldap3::drive!(conn);

			ldap.simple_bind(&ldap_config.bind_dn, &ldap_config.bind_password)
				.await
				.expect("could not authenticate to ldap");

			Self { client: ldap }
		} else {
			panic!("ldap must be configured for this test");
		}
	}

	/// Create a test user
	#[allow(clippy::too_many_arguments)]
	async fn create_user(
		&mut self,
		cn: &str,
		sn: &str,
		display_name: &str,
		mail: &str,
		telephone_number: Option<&str>,
		uid: &str,
		shadow_inactive: bool,
	) {
		tracing::info!("Adding test user to LDAP: `{mail}``");

		let user_account_control_value =
			if shadow_inactive { 514_i32.to_string() } else { 512_i32.to_string() };

		let mut attrs = vec![
			("objectClass", HashSet::from(["inetOrgPerson", "shadowAccount"])),
			("cn", HashSet::from([cn])),
			("sn", HashSet::from([sn])),
			("displayName", HashSet::from([display_name])),
			("mail", HashSet::from([mail])),
			("uid", HashSet::from([uid])),
			("shadowFlag", HashSet::from([user_account_control_value.as_str()])),
		];

		if let Some(phone) = telephone_number {
			attrs.push(("telephoneNumber", HashSet::from([phone])));
		}

		let base_dn = ldap_config()
			.await
			.sources
			.ldap
			.as_ref()
			.expect("ldap must be configured for this test")
			.base_dn
			.as_str();

		self.client
			.add(&format!("uid={},{}", uid, base_dn), attrs)
			.await
			.expect("failed to create debug user")
			.success()
			.expect("failed to create debug user");

		tracing::info!("Successfully added test user");
	}

	async fn change_user<S: AsRef<[u8]> + Eq + core::hash::Hash + Send>(
		&mut self,
		uid: &str,
		changes: Vec<(S, HashSet<S>)>,
	) {
		let mods = changes
			.into_iter()
			.map(|(attribute, changes)| Mod::Replace(attribute, changes))
			.collect();

		let base_dn = ldap_config()
			.await
			.sources
			.ldap
			.as_ref()
			.expect("ldap must be configured for this test")
			.base_dn
			.as_str();

		self.client
			.modify(&format!("uid={},{}", uid, base_dn), mods)
			.await
			.expect("failed to modify user")
			.success()
			.expect("failed to modify user");
	}

	async fn delete_user(&mut self, uid: &str) {
		let base_dn = ldap_config()
			.await
			.sources
			.ldap
			.as_ref()
			.expect("ldap must be configured for this test")
			.base_dn
			.as_str();

		self.client
			.delete(&format!("uid={},{}", uid, base_dn))
			.await
			.expect("failed to delete user")
			.success()
			.expect("failed to delete user");
	}
}

/// Open a connection to the configured Zitadel backend
async fn open_zitadel_connection() -> Zitadel {
	let zitadel_config = ldap_config().await.zitadel.clone();
	Zitadel::new(zitadel_config.url, zitadel_config.key_file)
		.await
		.expect("failed to set up Zitadel client")
}

/// Get the module's test environment config
async fn ldap_config() -> &'static Config {
	CONFIG_WITH_LDAP
		.get_or_init(|| async {
			let mut config = Config::new(Path::new("tests/environment/config.yaml"))
				.expect("failed to parse test env file");

			config.sources.ldap = serde_yaml::from_slice(
				&std::fs::read(Path::new("tests/environment/ldap-config.template.yaml"))
					.expect("failed to read ldap config file"),
			)
			.expect("failed to parse ldap config");

			config
		})
		.await
}

/// Get the module's test environment config
async fn ukt_config() -> &'static Config {
	CONFIG_WITH_UKT
		.get_or_init(|| async {
			let mut config = Config::new(Path::new("tests/environment/config.yaml"))
				.expect("failed to parse test env file");

			config.sources.ukt = serde_yaml::from_slice(
				&std::fs::read(Path::new("tests/environment/ukt-config.template.yaml"))
					.expect("failed to read ukt config file"),
			)
			.expect("failed to parse ukt config");

			config
		})
		.await
}

/// Get the module's test environment config
async fn csv_config() -> &'static Config {
	CONFIG_WITH_CSV
		.get_or_init(|| async {
			let mut config = Config::new(Path::new("tests/environment/config.yaml"))
				.expect("failed to parse test env file");

			config.sources.csv = serde_yaml::from_slice(
				&std::fs::read(Path::new("tests/environment/csv-config.template.yaml"))
					.expect("failed to read csv config file"),
			)
			.expect("failed to parse csv config");

			config
		})
		.await
}
