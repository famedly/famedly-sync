#![allow(clippy::expect_used)]

use std::{collections::HashSet, path::Path, time::Duration};

use base64::prelude::{Engine, BASE64_STANDARD};
use ldap3::{Ldap as LdapClient, LdapConnAsync, LdapConnSettings, Mod};
use ldap_sync::{sync_ldap_users_to_zitadel, AttributeMapping, Config, FeatureFlag};
use tempfile::TempDir;
use test_log::test;
use tokio::sync::OnceCell;
use url::Url;
use uuid::{uuid, Uuid};
use zitadel_rust_client::{
	error::{Error as ZitadelError, TonicErrorCode},
	UserType, Zitadel,
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

	sync_ldap_users_to_zitadel(config().await.clone()).await.expect("syncing failed");

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
			Some(config().await.famedly.organization_id.clone()),
			&user.id,
			"preferred_username",
		)
		.await
		.expect("could not get user metadata");
	assert_eq!(preferred_username, Some("Bobby".to_owned()));

	let uuid = Uuid::new_v5(&uuid!("d9979cff-abee-4666-bc88-1ec45a843fb8"), "simple".as_bytes());

	let localpart = zitadel
		.get_user_metadata(
			Some(config().await.famedly.organization_id.clone()),
			&user.id,
			"localpart",
		)
		.await
		.expect("could not get user metadata");
	assert_eq!(localpart, Some(uuid.to_string()));

	let grants = zitadel
		.list_user_grants(&config().await.famedly.organization_id, &user.id)
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

	sync_ldap_users_to_zitadel(config().await.clone()).await.expect("syncing failed");

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

	sync_ldap_users_to_zitadel(config).await.expect("syncing failed");

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

	sync_ldap_users_to_zitadel(config().await.clone()).await.expect("syncing failed");

	ldap.change_user("change", vec![("telephoneNumber", HashSet::from(["+12015550123"]))]).await;

	sync_ldap_users_to_zitadel(config().await.clone()).await.expect("syncing failed");

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

	sync_ldap_users_to_zitadel(config().await.clone()).await.expect("syncing failed");
	let zitadel = open_zitadel_connection().await;
	let user = zitadel.get_user_by_login_name("disable@famedly.de").await;
	assert!(user.is_ok_and(|u| u.is_some()));

	ldap.change_user("disable", vec![("shadowFlag", HashSet::from(["514"]))]).await;
	sync_ldap_users_to_zitadel(config().await.clone()).await.expect("syncing failed");
	let user = zitadel.get_user_by_login_name("disable@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));

	ldap.change_user("disable", vec![("shadowFlag", HashSet::from(["512"]))]).await;
	sync_ldap_users_to_zitadel(config().await.clone()).await.expect("syncing failed");
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

	sync_ldap_users_to_zitadel(config().await.clone()).await.expect("syncing failed");

	ldap.change_user("email_change", vec![("mail", HashSet::from(["email_changed@famedly.de"]))])
		.await;

	sync_ldap_users_to_zitadel(config().await.clone()).await.expect("syncing failed");

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

	sync_ldap_users_to_zitadel(config().await.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user =
		zitadel.get_user_by_login_name("deleted@famedly.de").await.expect("failed to find user");
	assert!(user.is_some());

	ldap.delete_user("deleted").await;

	sync_ldap_users_to_zitadel(config().await.clone()).await.expect("syncing failed");

	let user = zitadel.get_user_by_login_name("deleted@famedly.de").await;
	assert!(user.is_err_and(|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound)));
}

#[test(tokio::test)]
#[test_log(default_log_filter = "debug")]
async fn test_e2e_ldaps() {
	let mut config = config().await.clone();
	config.ldap.url = Url::parse("ldaps://localhost:1636").expect("invalid ldaps url");

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

	sync_ldap_users_to_zitadel(config).await.expect("syncing failed");

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
	config.ldap.tls.as_mut().expect("tls must be configured").danger_use_start_tls = true;

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

	sync_ldap_users_to_zitadel(config).await.expect("syncing failed");

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

	sync_ldap_users_to_zitadel(config().await.clone()).await.expect("syncing failed");

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
async fn test_e2e_binary_attr() {
	let mut config = config().await.clone();

	// OpenLDAP checks if types match, so we need to use an attribute
	// that can actually be binary.
	config.ldap.attributes.preferred_username = AttributeMapping::OptionalBinary {
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

	sync_ldap_users_to_zitadel(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("binary@famedly.de")
		.await
		.expect("could not query Zitadel users");

	assert!(user.is_some());

	if let Some(user) = user {
		let preferred_username = zitadel
			.get_user_metadata(
				Some(config.famedly.organization_id.clone()),
				&user.id,
				"preferred_username",
			)
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
	config.ldap.attributes.preferred_username = AttributeMapping::OptionalBinary {
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

	sync_ldap_users_to_zitadel(config.clone()).await.expect("syncing failed");

	let zitadel = open_zitadel_connection().await;
	let user = zitadel
		.get_user_by_login_name("binaryutf8@famedly.de")
		.await
		.expect("could not query Zitadel users");

	assert!(user.is_some());

	if let Some(user) = user {
		let preferred_username = zitadel
			.get_user_metadata(
				Some(config.famedly.organization_id.clone()),
				&user.id,
				"preferred_username",
			)
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
	sync_ldap_users_to_zitadel(dry_run_config.clone()).await.expect("syncing failed");
	assert!(zitadel.get_user_by_login_name("dry_run@famedly.de").await.is_err_and(
		|error| matches!(error, ZitadelError::TonicResponseError(status) if status.code() == TonicErrorCode::NotFound),
	));

	// Actually sync the user so we can test other changes
	sync_ldap_users_to_zitadel(config().await.clone()).await.expect("syncing failed");

	// Assert that a change in phone number does not sync
	ldap.change_user("dry_run", vec![("telephoneNumber", HashSet::from(["+12015550124"]))]).await;
	sync_ldap_users_to_zitadel(dry_run_config.clone()).await.expect("syncing failed");
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
	sync_ldap_users_to_zitadel(dry_run_config.clone()).await.expect("syncing failed");
	assert!(zitadel
		.get_user_by_login_name("dry_run@famedly.de")
		.await
		.is_ok_and(|user| user.is_some()));

	// Assert that a user deletion does not sync
	ldap.delete_user("dry_run").await;
	sync_ldap_users_to_zitadel(dry_run_config).await.expect("syncing failed");
	assert!(zitadel
		.get_user_by_login_name("dry_run@famedly.de")
		.await
		.is_ok_and(|user| user.is_some()));
}

struct Ldap {
	client: LdapClient,
}

impl Ldap {
	async fn new() -> Self {
		let config = config().await;
		let mut settings = LdapConnSettings::new();

		settings = settings.set_conn_timeout(Duration::from_secs(config.ldap.timeout));
		settings = settings.set_starttls(false);

		let (conn, mut ldap) = LdapConnAsync::from_url_with_settings(settings, &config.ldap.url)
			.await
			.expect("could not connect to ldap");

		ldap3::drive!(conn);

		ldap.simple_bind(&config.ldap.bind_dn, &config.ldap.bind_password)
			.await
			.expect("could not authenticate to ldap");

		Self { client: ldap }
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
		};

		self.client
			.add(&format!("uid={},{}", uid, config().await.ldap.base_dn.as_str()), attrs)
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

		self.client
			.modify(&format!("uid={},{}", uid, config().await.ldap.base_dn.as_str()), mods)
			.await
			.expect("failed to modify user")
			.success()
			.expect("failed to modify user");
	}

	async fn delete_user(&mut self, uid: &str) {
		self.client
			.delete(&format!("uid={},{}", uid, config().await.ldap.base_dn.as_str()))
			.await
			.expect("failed to delete user")
			.success()
			.expect("failed to delete user");
	}
}

/// Open a connection to the configured Zitadel backend
async fn open_zitadel_connection() -> Zitadel {
	let famedly = config().await.famedly.clone();
	Zitadel::new(famedly.url, famedly.key_file).await.expect("failed to set up Zitadel client")
}

/// Get the module's test environment config
async fn config() -> &'static Config {
	CONFIG
		.get_or_init(|| async {
			let mut config = Config::from_file(Path::new("tests/environment/config.yaml"))
				.await
				.expect("failed to parse test env file");

			let tempdir = TEMPDIR
				.get_or_init(|| async { TempDir::new().expect("failed to initialize cache dir") })
				.await;

			config.cache_path = tempdir.path().join("cache.bin");

			config
		})
		.await
}
