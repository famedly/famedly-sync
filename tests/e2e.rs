#![allow(clippy::expect_used)]

use std::{collections::HashSet, path::Path, time::Duration};

use base64::prelude::{Engine, BASE64_STANDARD};
use ldap3::{Ldap as LdapClient, LdapConnAsync, LdapConnSettings, Mod};
use ldap_sync::{
	test_helpers::{
		get_mock_server_url, prepare_endpoint_mock, prepare_oauth2_mock, ENDPOINT_PATH, OATH2_PATH,
	},
	AttributeMapping, Config, FeatureFlag,
};
use tempfile::TempDir;
use test_log::test;
use tokio::sync::OnceCell;
use url::Url;
use uuid::{uuid, Uuid};
use wiremock::MockServer;
use zitadel_rust_client::{
	error::{Error as ZitadelError, TonicErrorCode},
	Email, Gender, ImportHumanUserRequest, Phone, Profile, UserType, Zitadel,
};

static CONFIG: OnceCell<Config> = OnceCell::const_new();
static TEMPDIR: OnceCell<TempDir> = OnceCell::const_new();

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

	let config = config().await;
	config.perform_sync().await.expect("syncing failed");

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

	let uuid = Uuid::new_v5(&uuid!("d9979cff-abee-4666-bc88-1ec45a843fb8"), "simple".as_bytes());

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
	assert!(grant.role_keys.clone().into_iter().any(|key| key == "User"));
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

	let config = config().await;
	config.perform_sync().await.expect("syncing failed");

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
	let mut config = config().await.clone();
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

	config.perform_sync().await.expect("syncing failed");

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

	let config = config().await;
	config.perform_sync().await.expect("syncing failed");

	ldap.change_user("change", vec![("telephoneNumber", HashSet::from(["+12015550123"]))]).await;

	config.perform_sync().await.expect("syncing failed");

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

	let config = config().await;

	config.perform_sync().await.expect("syncing failed");
	let zitadel = open_zitadel_connection().await;
	let user = zitadel.get_user_by_login_name("disable@famedly.de").await;
	assert!(user.is_ok_and(|u| u.is_some()));

	ldap.change_user("disable", vec![("shadowFlag", HashSet::from(["514"]))]).await;
	config.perform_sync().await.expect("syncing failed");
	let user = zitadel.get_user_by_login_name("disable@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));

	ldap.change_user("disable", vec![("shadowFlag", HashSet::from(["512"]))]).await;
	config.perform_sync().await.expect("syncing failed");
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

	let config = config().await;
	config.perform_sync().await.expect("syncing failed");

	ldap.change_user("email_change", vec![("mail", HashSet::from(["email_changed@famedly.de"]))])
		.await;

	config.perform_sync().await.expect("syncing failed");

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

	let config = config().await;
	config.perform_sync().await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user =
		zitadel.get_user_by_login_name("deleted@famedly.de").await.expect("failed to find user");
	assert!(user.is_some());

	ldap.delete_user("deleted").await;

	config.perform_sync().await.expect("syncing failed");

	let user = zitadel.get_user_by_login_name("deleted@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_ldaps() {
	let mut config = config().await.clone();
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

	config.perform_sync().await.expect("syncing failed");

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
	let mut config = config().await.clone();
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
		"starttls",
		false,
	)
	.await;

	config.perform_sync().await.expect("syncing failed");

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

	let config = config().await;
	config.perform_sync().await.expect("syncing failed");

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

	let config = config().await;
	config.perform_sync().await.expect("syncing failed");

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

	config.perform_sync().await.expect("syncing failed");

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
async fn test_e2e_binary_attr() {
	let mut config = config().await.clone();

	// OpenLDAP checks if types match, so we need to use an attribute
	// that can actually be binary.
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
		"Bob",
		"Tables",
		"Bobby",
		"binary@famedly.de",
		Some("+12015550123"),
		"binary",
		false,
	)
	.await;
	ldap.change_user(
		"binary",
		vec![(
			"userSMIMECertificate".as_bytes(),
			// It's important that this is invalid UTF-8
			HashSet::from([[0xA0, 0xA1].as_slice()]),
		)],
	)
	.await;

	let org_id = config.zitadel.organization_id.clone();

	config.perform_sync().await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("binary@famedly.de")
		.await
		.expect("could not query Zitadel users");

	assert!(user.is_some());

	if let Some(user) = user {
		let preferred_username = zitadel
			.get_user_metadata(Some(org_id), &user.id, "preferred_username")
			.await
			.expect("could not get user metadata");

		assert_eq!(
			preferred_username
				.map(|u| BASE64_STANDARD.decode(u).expect("failed to decode binary attr")),
			Some([0xA0, 0xA1].to_vec())
		);
	}
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_binary_attr_valid_utf8() {
	let mut config = config().await.clone();

	// OpenLDAP checks if types match, so we need to use an attribute
	// that can actually be binary.
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
		"Bob",
		"Tables",
		"Bobby",
		"binaryutf8@famedly.de",
		Some("+12015550123"),
		"binaryutf8",
		false,
	)
	.await;
	ldap.change_user(
		"binaryutf8",
		vec![("userSMIMECertificate".as_bytes(), HashSet::from(["validutf8".as_bytes()]))],
	)
	.await;

	let org_id = config.zitadel.organization_id.clone();

	config.perform_sync().await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("binaryutf8@famedly.de")
		.await
		.expect("could not query Zitadel users");

	assert!(user.is_some());

	if let Some(user) = user {
		let preferred_username = zitadel
			.get_user_metadata(Some(org_id), &user.id, "preferred_username")
			.await
			.expect("could not get user metadata");

		assert_eq!(
			preferred_username
				.map(|u| BASE64_STANDARD.decode(u).expect("failed to decode binary attr")),
			Some("validutf8".as_bytes().to_vec())
		);
	}
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_dry_run() {
	let mut dry_run_config = config().await.clone();
	let config = config().await;
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
	dry_run_config.perform_sync().await.expect("syncing failed");
	assert!(zitadel.get_user_by_login_name("dry_run@famedly.de").await.is_err_and(
		|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound),
	));

	// Actually sync the user so we can test other changes=
	config.perform_sync().await.expect("syncing failed");

	// Assert that a change in phone number does not sync
	ldap.change_user("dry_run", vec![("telephoneNumber", HashSet::from(["+12015550124"]))]).await;
	dry_run_config.perform_sync().await.expect("syncing failed");
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
	dry_run_config.perform_sync().await.expect("syncing failed");
	assert!(zitadel
		.get_user_by_login_name("dry_run@famedly.de")
		.await
		.is_ok_and(|user| user.is_some()));

	// Assert that a user deletion does not sync
	ldap.delete_user("dry_run").await;
	dry_run_config.perform_sync().await.expect("syncing failed");
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

	let mut config = config().await.clone();
	config.perform_sync().await.expect("syncing failed");

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
	config.perform_sync().await.expect("syncing failed");

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

	let mut config = config().await.clone();

	config
		.sources
		.ukt
		.as_mut()
		.map(|ukt| {
			ukt.oauth2_url = get_mock_server_url(&mock_server, OATH2_PATH)
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

	config.perform_sync().await.expect("syncing failed");

	let user = zitadel.get_user_by_login_name("delete_me@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_full_sync_with_ldap_and_ukt() {
	let mock_server = MockServer::start().await;
	prepare_oauth2_mock(&mock_server).await;
	prepare_endpoint_mock(&mock_server, "not_to_be_there@famedly.de").await;

	let mut config = config().await.clone();
	config
		.sources
		.ukt
		.as_mut()
		.map(|ukt| {
			ukt.oauth2_url = get_mock_server_url(&mock_server, OATH2_PATH)
				.expect("Failed to get mock server URL");
			ukt.endpoint_url = get_mock_server_url(&mock_server, ENDPOINT_PATH)
				.expect("Failed to get mock server URL");
		})
		.expect("UKT configuration is missing");

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

	config.perform_sync().await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;

	let user = zitadel.get_user_by_login_name("not_to_be_there@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));

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

	ldap.change_user("to_be_changed", vec![("telephoneNumber", HashSet::from(["+12015550123"]))])
		.await;
	ldap.delete_user("not_to_be_there_later").await;

	config.perform_sync().await.expect("syncing failed");

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
		let config = config().await.clone();
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

		let base_dn = config()
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

		let base_dn = config()
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
		let base_dn = config()
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
	let zitadel_config = config().await.zitadel.clone();
	Zitadel::new(zitadel_config.url, zitadel_config.key_file)
		.await
		.expect("failed to set up Zitadel client")
}

/// Get the module's test environment config
async fn config() -> &'static Config {
	CONFIG
		.get_or_init(|| async {
			let mut config = Config::new(Path::new("tests/environment/config.yaml"))
				.expect("failed to parse test env file");

			let tempdir = TEMPDIR
				.get_or_init(|| async { TempDir::new().expect("failed to initialize cache dir") })
				.await;

			config
				.sources
				.ldap
				.as_mut()
				.expect("ldap must be configured for this test")
				.cache_path = tempdir.path().join("cache.bin");

			config
		})
		.await
}
