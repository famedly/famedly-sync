//! Common test helpers

#![cfg(test)]
#![allow(clippy::expect_used, dead_code)]

use std::{collections::HashSet, path::Path, time::Duration};

use famedly_sync::{Config, SkippedErrors, zitadel::Zitadel as SyncZitadel};
use futures::TryStreamExt;
use ldap3::{Ldap as LdapClient, LdapConnAsync, LdapConnSettings, Mod};
use tokio::sync::OnceCell;

/// Ldap client with helper functions to create tests users
pub struct Ldap {
	client: LdapClient,
}

impl Ldap {
	/// Initialize the LDAP client
	pub async fn new() -> Self {
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
	pub async fn create_user(
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

	/// Change user details
	pub async fn change_user<S: AsRef<[u8]> + Eq + core::hash::Hash + Send>(
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

	/// Delete a user
	pub async fn delete_user(&mut self, uid: &str) {
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

static CONFIG_WITH_LDAP: OnceCell<Config> = OnceCell::const_new();
static CONFIG_WITH_CSV: OnceCell<Config> = OnceCell::const_new();
static CONFIG_WITH_UKT: OnceCell<Config> = OnceCell::const_new();

/// Get the module's test environment config
pub async fn ldap_config() -> &'static Config {
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
pub async fn ukt_config() -> &'static Config {
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
pub async fn csv_config() -> &'static Config {
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

pub async fn cleanup_test_users(config: &Config) {
	let skipped_errors = SkippedErrors::new();
	let zitadel =
		SyncZitadel::new(config.zitadel.clone(), config.feature_flags.clone(), &skipped_errors)
			.await
			.expect("failed to set up Zitadel client");

	zitadel
		.list_users()
		.expect("failed to list users")
		.try_for_each_concurrent(Some(4), async |zitadel_user| {
			zitadel.delete_user(&zitadel_user.0).await
		})
		.await
		.unwrap();
}
