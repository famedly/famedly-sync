//! Common test helpers

#![cfg(test)]
#![allow(clippy::expect_used, dead_code)]

use std::{collections::HashSet, path::Path, pin::pin, time::Duration};

use anyhow::{Context as _, Result};
use famedly_sync::{Config, SkippedErrors, zitadel::Zitadel as SyncZitadel};
use futures::{StreamExt, TryStreamExt};
use ldap3::{Ldap as LdapClient, LdapConnAsync, LdapConnSettings, Mod};
use tokio::sync::OnceCell;
use zitadel_rust_client::v2::{
	Zitadel,
	management::{V1UserGrantProjectIdQuery, V1UserGrantQuery, V1UserGrantUserIdQuery},
	pagination::PaginationParams,
	users::{
		AddHumanUserRequest, IdpLink, LoginNameQuery, Organization, SearchQuery, SetHumanEmail,
		SetHumanPhone, SetHumanProfile, User,
	},
};

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
			.add(&format!("uid={uid},{base_dn}"), attrs)
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
			.modify(&format!("uid={uid},{base_dn}"), mods)
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
			.delete(&format!("uid={uid},{base_dn}"))
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

/// Test-only ergonomic helpers on top of the raw Zitadel v2 client.
///
/// These exist so the e2e tests can read and seed Zitadel state through the
/// same v2 HTTP API that production uses, instead of the legacy v1/gRPC client.
#[allow(async_fn_in_trait)]
pub trait ZitadelExt {
	/// Look up a single user by one of their login names.
	///
	/// Returns `Ok(None)` when no such user exists (the v2 API simply yields an
	/// empty result set, unlike the v1 client which raised a `NotFound` error).
	async fn get_user_by_login_name(&self, login_name: &str) -> Result<Option<User>>;

	/// Fetch and decode a single metadata value for a user, if present.
	async fn get_metadata(&self, org_id: &str, user_id: &str, key: &str) -> Result<Option<String>>;

	/// Collect all role keys granted to a user on the given project.
	async fn user_role_keys(
		&self,
		org_id: &str,
		project_id: &str,
		user_id: &str,
	) -> Result<Vec<String>>;

	/// Collect all identity provider links of a user.
	async fn user_idp_links(&self, user_id: &str) -> Result<Vec<IdpLink>>;

	/// Create a verified human user directly in Zitadel and return its ID.
	///
	/// Intended for seeding test fixtures that bypass the sync logic.
	#[allow(clippy::too_many_arguments)]
	async fn create_test_human_user(
		&self,
		org_id: &str,
		username: &str,
		first_name: &str,
		last_name: &str,
		display_name: &str,
		nick_name: &str,
		email: &str,
		phone: &str,
	) -> Result<String>;
}

impl ZitadelExt for Zitadel {
	async fn get_user_by_login_name(&self, login_name: &str) -> Result<Option<User>> {
		let mut stream = pin!(self.list_users(
			None,
			Some(PaginationParams::DEFAULT.with_asc(true)),
			None,
			Some(vec![
				SearchQuery::new()
					.with_login_name_query(LoginNameQuery::new(login_name.to_owned())),
			]),
		)?);

		stream.next().await.transpose()
	}

	async fn get_metadata(&self, org_id: &str, user_id: &str, key: &str) -> Result<Option<String>> {
		Ok(self.get_user_metadata(user_id, key, Some(org_id.to_owned())).await?.metadata().value())
	}

	async fn user_role_keys(
		&self,
		org_id: &str,
		project_id: &str,
		user_id: &str,
	) -> Result<Vec<String>> {
		let grants: Vec<_> = self
			.search_user_grants(
				Some(org_id.to_owned()),
				None,
				Some(vec![
					V1UserGrantQuery::ProjectId {
						project_id_query: V1UserGrantProjectIdQuery::new()
							.with_project_id(project_id.to_owned()),
					},
					V1UserGrantQuery::UserId {
						user_id_query: V1UserGrantUserIdQuery::new()
							.with_user_id(user_id.to_owned()),
					},
				]),
			)?
			.try_collect()
			.await?;

		Ok(grants.into_iter().filter_map(|grant| grant.role_keys().cloned()).flatten().collect())
	}

	async fn user_idp_links(&self, user_id: &str) -> Result<Vec<IdpLink>> {
		self.list_idp_links(user_id, None, None)?.try_collect().await
	}

	async fn create_test_human_user(
		&self,
		org_id: &str,
		username: &str,
		first_name: &str,
		last_name: &str,
		display_name: &str,
		nick_name: &str,
		email: &str,
		phone: &str,
	) -> Result<String> {
		let request = AddHumanUserRequest::new(
			SetHumanProfile::new(first_name.to_owned(), last_name.to_owned())
				.with_display_name(display_name.to_owned())
				.with_nick_name(nick_name.to_owned()),
			SetHumanEmail::new(email.to_owned()).with_is_verified(true),
		)
		.with_username(username.to_owned())
		.with_organization(Organization::new().with_org_id(org_id.to_owned()))
		.with_phone(SetHumanPhone::new().with_phone(phone.to_owned()).with_is_verified(true));

		self.create_human_user(request)
			.await?
			.user_id()
			.context("created user is missing an ID")
			.cloned()
	}
}
