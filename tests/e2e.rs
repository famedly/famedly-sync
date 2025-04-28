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
use zitadel_rust_client::v1::{
	Email, Gender, ImportHumanUserRequest, Phone, Profile, UserType, Zitadel,
	error::{Error as ZitadelError, TonicErrorCode},
};

mod common;

use common::{Ldap, cleanup_test_users, csv_config, ldap_config, ukt_config};

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

		perform_sync(config.clone()).await.map_err(|e| format!("Sync failed: {e}"))?;

		let user = zitadel
			.get_user_by_login_name(login_name)
			.await
			.map_err(|e| format!("Failed to get user: {e}"))?
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
			panic!("Test failed for ID '{uid}': {error}");
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
	perform_sync(config.clone()).await.expect("Initial sync failed");

	// Verify all users exist with correct data
	for user in TEST_USERS {
		let expected_hex_id = hex::encode(user.uid.as_bytes());

		let zitadel_user = zitadel
			.get_user_by_login_name(user.email)
			.await
			.unwrap_or_else(|_| panic!("Failed to get user {}", user.email))
			.unwrap_or_else(|| panic!("User {} not found", user.email));

		match zitadel_user.r#type {
			Some(UserType::Human(human)) => {
				// Verify ID encoding
				let profile =
					human.profile.unwrap_or_else(|| panic!("User {} lacks profile", user.email));
				assert_eq!(
					profile.nick_name,
					expected_hex_id,
					"Wrong ID encoding for user {}, got '{:?}', expected '{:?}'",
					user.email,
					String::from_utf8_lossy(&hex::decode(profile.nick_name.clone()).unwrap()),
					String::from_utf8_lossy(&hex::decode(expected_hex_id.clone()).unwrap())
				);

				// Verify phone number to ensure complete sync
				let phone =
					human.phone.unwrap_or_else(|| panic!("User {} lacks phone", user.email));
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
	perform_sync(config.clone()).await.expect("Update sync failed");

	// Verify updates were applied in correct order
	for user in TEST_USERS {
		let zitadel_user = zitadel
			.get_user_by_login_name(user.email)
			.await
			.unwrap_or_else(|_| panic!("Failed to get updated user {}", user.email))
			.unwrap_or_else(|| panic!("Updated user {} not found", user.email));

		match zitadel_user.r#type {
			Some(UserType::Human(human)) => {
				let profile = human
					.profile
					.unwrap_or_else(|| panic!("Updated user {} lacks profile", user.email));
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
	perform_sync(config.clone()).await.expect("Deletion sync failed");

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
	perform_sync(config.clone()).await.expect("syncing failed");

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
	perform_sync(config.clone()).await.expect("syncing failed");

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
				panic!("zitadel failed while searching for user: {error}")
			}
		}
	} else {
		panic!("disabled user was synced: {user:?}");
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

	perform_sync(config.clone()).await.expect("syncing failed");

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
	perform_sync(config.clone()).await.expect("syncing failed");

	ldap.change_user("change", vec![("telephoneNumber", HashSet::from(["+12015550123"]))]).await;

	perform_sync(config.clone()).await.expect("syncing failed");

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

	perform_sync(config.clone()).await.expect("syncing failed");
	let zitadel = open_zitadel_connection().await;
	let user = zitadel.get_user_by_login_name("disable@famedly.de").await;
	assert!(user.is_ok_and(|u| u.is_some()));

	ldap.change_user("disable", vec![("shadowFlag", HashSet::from(["514"]))]).await;
	perform_sync(config.clone()).await.expect("syncing failed");
	let user = zitadel.get_user_by_login_name("disable@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));

	ldap.change_user("disable", vec![("shadowFlag", HashSet::from(["512"]))]).await;
	perform_sync(config.clone()).await.expect("syncing failed");
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
	perform_sync(config.clone()).await.expect("syncing failed");

	ldap.change_user("email_change", vec![("mail", HashSet::from(["email_changed@famedly.de"]))])
		.await;

	perform_sync(config.clone()).await.expect("syncing failed");

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
	perform_sync(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user =
		zitadel.get_user_by_login_name("deleted@famedly.de").await.expect("failed to find user");
	assert!(user.is_some());

	ldap.delete_user("deleted").await;

	perform_sync(config.clone()).await.expect("syncing failed");

	let user = zitadel.get_user_by_login_name("deleted@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));

	assert!(zitadel.get_user_by_login_name("another_user@example.test").await.is_ok());
	assert!(zitadel.get_user_by_login_name("projectless_user@example.test").await.is_ok());
	assert!(zitadel.get_user_by_login_name("another_org_user@example.test").await.is_ok());
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_user_no_localpart_skipped() {
	let config = ldap_config().await.clone();

	// Prepare Zitadel client
	let zitadel = open_zitadel_connection().await;

	// Create user in Zitadel
	let user = ImportHumanUserRequest {
		user_name: "maxmustermann".to_owned(),
		profile: Some(Profile {
			first_name: "Test".to_owned(),
			last_name: "User".to_owned(),
			display_name: "User, Test".to_owned(),
			gender: Gender::Unspecified.into(),
			nick_name: "deadbeef".to_owned(),
			preferred_language: String::default(),
		}),
		email: Some(Email { email: "max@mustermann.de".to_owned(), is_email_verified: true }),
		phone: Some(Phone { phone: "+12345678901".to_owned(), is_phone_verified: true }),
		password: String::default(),
		hashed_password: None,
		password_change_required: false,
		request_passwordless_registration: false,
		otp_code: String::default(),
		idps: vec![],
	};

	zitadel
		.create_human_user(&config.zitadel.organization_id, user)
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
	perform_sync(config.clone()).await.expect("syncing failed");

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

	perform_sync(config.clone()).await.expect("syncing failed");

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

	perform_sync(config.clone()).await.expect("syncing failed");

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

	perform_sync(config.clone()).await.expect("syncing failed");

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

	perform_sync(config.clone()).await.expect("syncing failed");

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
	assert!(zitadel.get_user_by_login_name("dry_run@famedly.de").await.is_err_and(
		|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound),
	));

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

	assert!(
		matches!(user.r#type, Some(UserType::Human(user)) if user.phone.as_ref().expect("phone missing").phone == "+12015550123")
	);

	// Assert that disabling a user does not sync
	ldap.change_user("dry_run", vec![("shadowFlag", HashSet::from(["514"]))]).await;
	perform_sync(dry_run_config.clone()).await.expect("syncing failed");
	assert!(
		zitadel.get_user_by_login_name("dry_run@famedly.de").await.is_ok_and(|user| user.is_some())
	);

	// Assert that a user deletion does not sync
	ldap.delete_user("dry_run").await;
	perform_sync(dry_run_config.clone()).await.expect("syncing failed");
	assert!(
		zitadel.get_user_by_login_name("dry_run@famedly.de").await.is_ok_and(|user| user.is_some())
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
	perform_sync(config.clone()).await.expect("syncing failed");

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
	let user = zitadel
		.create_human_user(&config.zitadel.organization_id, user)
		.await
		.expect("failed to create user");

	zitadel
		.set_user_metadata(
			Some(&config.zitadel.organization_id),
			user.clone(),
			"localpart".to_owned(),
			"irrelevant",
		)
		.await
		.expect("Failed to set user localpart");

	zitadel
		.set_user_metadata(
			Some(&config.zitadel.organization_id),
			user.clone(),
			"preferred_username".to_owned(),
			"irrelevant",
		)
		.await
		.expect("Failed to set user preferred name");

	let user = zitadel
		.get_user_by_login_name("delete_me@famedly.de")
		.await
		.expect("could not query Zitadel users");
	assert!(user.is_some());
	let user = user.expect("could not find user");
	assert_eq!(user.user_name, "delete_me@famedly.de");

	perform_sync(config.clone()).await.expect("syncing failed");

	let user = zitadel.get_user_by_login_name("delete_me@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));
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
	assert_eq!(user.user_name, "john.doe@example.com");
	assert_eq!(user.id, "john.doe", "Unexpected Zitadel userId");

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

	let localpart = zitadel
		.get_user_metadata(Some(config.zitadel.organization_id.clone()), &user.id, "localpart")
		.await
		.expect("could not get user metadata");
	assert_eq!(localpart, Some(user.id.clone()), "Localpart metadata should match userId");

	let grants = zitadel
		.list_user_grants(&config.zitadel.organization_id, &user.id)
		.await
		.expect("failed to get user grants");

	let grant = grants.result.first().expect("no user grants found");
	assert!(grant.role_keys.clone().into_iter().any(|key| key == FAMEDLY_USER_ROLE));

	// Test user without localpart (should use UUID)
	let user = zitadel
		.get_user_by_login_name("jane.smith@example.com")
		.await
		.expect("could not query Zitadel users");

	let user = user.expect("could not find user");
	assert_eq!(user.user_name, "jane.smith@example.com");
	let uuid = Uuid::new_v5(&FAMEDLY_NAMESPACE, "jane.smith@example.com".as_bytes());
	assert_eq!(user.id, uuid.to_string(), "Unexpected Zitadel userId for user without localpart");

	let localpart = zitadel
		.get_user_metadata(Some(config.zitadel.organization_id.clone()), &user.id, "localpart")
		.await
		.expect("could not get user metadata");
	assert_eq!(localpart, Some(user.id), "Localpart metadata should match userId");

	// Re-import an existing user to update (as checked by unique email)
	let csv_content = indoc::indoc! {r#"
    email,first_name,last_name,phone,localpart
    john.doe@example.com,Changed_Name,Changed_Surname,+2222222222,new.localpart
  "#};
	let _file = temp_csv_file(&mut config, csv_content);
	perform_sync(config.clone()).await.expect("syncing failed");

	let user = zitadel
		.get_user_by_login_name("john.doe@example.com")
		.await
		.expect("could not query Zitadel users");

	let user = user.expect("could not find user");
	assert_eq!(user.user_name, "john.doe@example.com");
	assert_eq!(user.id, "john.doe", "Zitadel userId should not change");

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

	let localpart = zitadel
		.get_user_metadata(Some(config.zitadel.organization_id.clone()), &user.id, "localpart")
		.await
		.expect("could not get user metadata");
	assert_eq!(localpart, Some(user.id), "Localpart metadata should match userId");
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

	perform_sync(ldap_config.clone()).await.expect("syncing failed");

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

	let idps = zitadel.list_user_idps(user.id.clone()).await.expect("could not get user IDPs");

	assert!(!idps.is_empty(), "User should have IDP links");

	let idp = idps.first().expect("No IDP link found");
	assert_eq!(idp.idp_id, config.zitadel.idp_id, "IDP link should match configured IDP");
	assert_eq!(idp.provided_user_id, test_uid, "IDP provided_user_id should match plain LDAP uid");
	assert_eq!(idp.user_id, user.id, "IDP user_id should match Zitadel user id");
	assert_eq!(
		idp.provided_user_name, test_email,
		"IDP provided_user_name should match test_email"
	);
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_migrate_base64_id() {
	let config = ldap_config().await;
	cleanup_test_users(config).await;

	// The uid for this test must be such that encodes to such base64 string that
	// doesn't look like hex. Otherwise, we need to have a sample of users so the
	// script determines encoding heuristically. This is tested later in
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

	// The migration logic should decide to treat it as hex when looking at it on
	// its own, because we check for hex first (it's a subset of base64 and thus
	// more restrictive)
	let expected_id = ambiguous_id.clone();
	run_migration_test(config, email, user_name, ambiguous_id, expected_id).await;

	// When we create some base64-only encoded values in the database, the migration
	// logic should heuristically find out, that the DB has external IDs encoded
	// with base64 and thus treat the ambiguous ID as base64 even though it can be
	// both base64 and hex
	let base_64_user = ImportHumanUserRequest {
		user_name: "another_test".to_owned(),
		profile: Some(Profile {
			first_name: "Test".to_owned(),
			last_name: "User".to_owned(),
			display_name: "User, Test".to_owned(),
			gender: Gender::Unspecified.into(),
			nick_name: "Z9FmZQ==".to_owned(), // base64 encoded
			preferred_language: String::default(),
		}),
		email: Some(Email {
			email: "another_test@example.com".to_owned(),
			is_email_verified: true,
		}),
		phone: Some(Phone { phone: "+12345678901".to_owned(), is_phone_verified: true }),
		password: String::default(),
		hashed_password: None,
		password_change_required: false,
		request_passwordless_registration: false,
		otp_code: String::default(),
		idps: vec![],
	};

	let zitadel = open_zitadel_connection().await;
	let temp_user = zitadel
		.create_human_user(&config.zitadel.organization_id, base_64_user)
		.await
		.expect("Failed to create user");

	zitadel
		.set_user_metadata(
			Some(&config.zitadel.organization_id),
			temp_user.clone(),
			"localpart".to_owned(),
			"irrelevant",
		)
		.await
		.expect("Failed to set user localpart");

	zitadel
		.set_user_metadata(
			Some(&config.zitadel.organization_id),
			temp_user.clone(),
			"preferred_username".to_owned(),
			"irrelevant",
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

	zitadel.remove_user(temp_user).await.expect("Failed to delete user");
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

	match user.r#type {
		Some(UserType::Human(human)) => {
			let profile = human.profile.expect("User lacks profile after LDAP sync");
			let expected_hex_id = hex::encode(uid.as_bytes());
			assert_eq!(
				profile.nick_name, expected_hex_id,
				"External ID not in hex encoding after LDAP sync for user '{email}'"
			);
			assert_eq!(
				profile.first_name, "New First Name",
				"Fist name was not updated by LDAP sync for user '{email}'"
			);
		}
		_ => panic!("User lacks human details after LDAP sync for user '{email}'"),
	}
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

/// Open a connection to the configured Zitadel backend
async fn open_zitadel_connection() -> Zitadel {
	let zitadel_config = ldap_config().await.zitadel.clone();
	Zitadel::new(zitadel_config.url, zitadel_config.key_file)
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
	let user = ImportHumanUserRequest {
		user_name: user_name.to_owned(),
		profile: Some(Profile {
			first_name: "Test".to_owned(),
			last_name: "User".to_owned(),
			display_name: "User, Test".to_owned(),
			gender: Gender::Unspecified.into(),
			nick_name: initial_nick_name.clone(),
			preferred_language: String::default(),
		}),
		email: Some(Email { email: email.to_owned(), is_email_verified: true }),
		phone: Some(Phone { phone: "+12345678901".to_owned(), is_phone_verified: true }),
		password: String::default(),
		hashed_password: None,
		password_change_required: false,
		request_passwordless_registration: false,
		otp_code: String::default(),
		idps: vec![],
	};

	let user_id = zitadel
		.create_human_user(&config.zitadel.organization_id, user)
		.await
		.expect("Failed to create user");

	zitadel
		.set_user_metadata(
			Some(&config.zitadel.organization_id),
			user_id.clone(),
			"localpart".to_owned(),
			"irrelevant",
		)
		.await
		.expect("Failed to set user localpart");

	zitadel
		.set_user_metadata(
			Some(&config.zitadel.organization_id),
			user_id.clone(),
			"preferred_username".to_owned(),
			"irrelevant",
		)
		.await
		.expect("Failed to set user preferred name");

	zitadel
		.add_user_grant(
			Some(config.zitadel.organization_id.clone()),
			user_id,
			config.zitadel.project_id.clone(),
			None,
			vec![FAMEDLY_USER_ROLE.to_owned()],
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

	match user.r#type {
		Some(user_type) => {
			if let UserType::Human(human) = user_type {
				let profile = human.profile.expect("User lacks profile");
				assert_eq!(
					profile.nick_name, expected_nick_name,
					"Nickname encoding mismatch for user '{email}'"
				);
			} else {
				panic!("User is not of type Human for user '{email}'");
			}
		}
		None => panic!("User type is None for user '{email}'"),
	}
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
