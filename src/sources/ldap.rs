//! LDAP source for syncing with Famedly's Zitadel.

use std::{
	fmt::Display,
	path::{Path, PathBuf},
};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use ldap_poller::{
	config::TLSConfig, ldap::EntryStatus, ldap3::SearchEntry, AttributeConfig, Cache, CacheMethod,
	ConnectionConfig, Ldap, SearchEntryExt, Searches,
};
use serde::Deserialize;
use tokio::sync::mpsc::Receiver;
use tokio_stream::{wrappers::ReceiverStream, StreamExt};
use url::Url;

use super::Source;
use crate::{
	user::{StringOrBytes, User},
	zitadel::{ChangedUser, SourceDiff, UserId},
};

/// LDAP sync source
pub struct LdapSource {
	/// LDAP configuration
	ldap_config: LdapSourceConfig,
	/// Dry run flag (prevents writing cache)
	is_dry_run: bool,
}

#[async_trait]
impl Source for LdapSource {
	fn get_name(&self) -> &'static str {
		"LDAP"
	}

	async fn get_diff(&self) -> Result<SourceDiff> {
		let cache = read_cache(&self.ldap_config.cache_path).await?;
		let (mut ldap_client, ldap_receiver) = Ldap::new(self.ldap_config.clone().into(), cache);

		let is_dry_run = self.is_dry_run;
		let cache_path = self.ldap_config.cache_path.clone();

		let sync_handle: tokio::task::JoinHandle<Result<_>> = tokio::spawn(async move {
			ldap_client.sync_once(None).await.context("failed to sync/fetch data from LDAP")?;

			if is_dry_run {
				tracing::warn!("Not writing ldap cache during a dry run");
			} else {
				let cache = ldap_client.persist_cache().await;
				tokio::fs::write(
					&cache_path,
					bincode::serialize(&cache).context("failed to serialize cache")?,
				)
				.await
				.context("failed to write cache")?;
			}

			tracing::info!("Finished syncing LDAP data");

			Ok(())
		});

		let (added, changed, removed) = self.get_user_changes(ldap_receiver).await?;

		sync_handle.await??;

		Ok(SourceDiff {
			new_users: added,
			changed_users: changed.into_iter().map(|(old, new)| ChangedUser { old, new }).collect(),
			deleted_user_ids: removed,
		})
	}
}

impl LdapSource {
	/// Create a new LDAP source
	pub fn new(ldap_config: LdapSourceConfig, is_dry_run: bool) -> Self {
		Self { ldap_config, is_dry_run }
	}

	/// Get user changes from an ldap receiver
	pub async fn get_user_changes(
		&self,
		ldap_receiver: Receiver<EntryStatus>,
	) -> Result<(Vec<User>, Vec<(User, User)>, Vec<UserId>)> {
		ReceiverStream::new(ldap_receiver)
			.fold(Ok((vec![], vec![], vec![])), |acc, entry_status| {
				let (mut added, mut changed, mut removed) = acc?;
				match entry_status {
					EntryStatus::New(entry) => {
						tracing::debug!("New entry: {:?}", entry);
						added.push(self.parse_user(entry)?);
					}
					EntryStatus::Changed { old, new } => {
						tracing::debug!("Changes found for {:?} -> {:?}", old, new);
						changed.push((self.parse_user(old)?, self.parse_user(new)?));
					}
					EntryStatus::Removed(entry) => {
						tracing::debug!("Deleted user {}", String::from_utf8_lossy(&entry));
						removed.push(UserId::Nick(String::from_utf8(entry.clone())?));
					}
				};
				Ok((added, changed, removed))
			})
			.await
	}

	/// Construct a user from an LDAP SearchEntry
	pub(crate) fn parse_user(&self, entry: SearchEntry) -> Result<User> {
		let disable_bitmask = {
			use std::ops::BitOr;
			self.ldap_config.attributes.disable_bitmasks.iter().fold(0, i32::bitor)
		};

		let status = read_search_entry(&entry, &self.ldap_config.attributes.status)?;
		let enabled = if disable_bitmask != 0 {
			disable_bitmask
				& match status {
					StringOrBytes::String(status) => status.parse::<i32>()?,
					StringOrBytes::Bytes(status) => {
						i32::from_be_bytes(status.try_into().map_err(|err: Vec<u8>| {
							let err_string = String::from_utf8_lossy(&err).to_string();
							anyhow!(err_string).context("failed to convert to i32 flag")
						})?)
					}
				} == 0
		} else if let StringOrBytes::String(status) = status {
			match &status[..] {
				"TRUE" => true,
				"FALSE" => false,
				_ => bail!("Cannot parse status without disable_bitmasks: {:?}", status),
			}
		} else {
			bail!("Binary status without disable_bitmasks");
		};

		let first_name = read_search_entry(&entry, &self.ldap_config.attributes.first_name)?;
		let last_name = read_search_entry(&entry, &self.ldap_config.attributes.last_name)?;
		let preferred_username =
			read_search_entry(&entry, &self.ldap_config.attributes.preferred_username)?;
		let email = read_search_entry(&entry, &self.ldap_config.attributes.email)?;
		let ldap_user_id = read_search_entry(&entry, &self.ldap_config.attributes.user_id)?;
		let phone = read_search_entry(&entry, &self.ldap_config.attributes.phone).ok();

		Ok(User {
			first_name,
			last_name,
			preferred_username,
			email,
			external_user_id: ldap_user_id,
			phone,
			enabled,
		})
	}
}

/// Read an attribute from the entry
fn read_search_entry(entry: &SearchEntry, attribute: &AttributeMapping) -> Result<StringOrBytes> {
	match attribute {
		AttributeMapping::OptionalBinary { name, is_binary: false }
		| AttributeMapping::NoBinaryOption(name) => {
			if let Some(attr) = entry.attr_first(name) {
				return Ok(StringOrBytes::String(attr.to_owned()));
			};
		}
		AttributeMapping::OptionalBinary { name, is_binary: true } => {
			if let Some(binary_attr) = entry.bin_attr_first(name) {
				return Ok(StringOrBytes::Bytes(binary_attr.to_vec()));
			};

			// If attributes encode as valid UTF-8, they will
			// not be in the bin_attr list
			if let Some(attr) = entry.attr_first(name) {
				return Ok(StringOrBytes::Bytes(attr.as_bytes().to_vec()));
			};
		}
	}

	bail!("missing `{}` values for `{}`", attribute, entry.dn)
}

/// Read the ldap sync cache
pub async fn read_cache(path: &Path) -> Result<Option<Cache>> {
	Ok(match tokio::fs::read(path).await {
		Ok(data) => Some(bincode::deserialize(&data).context("cache deserialization failed")?),
		Err(err) => {
			if err.kind() == std::io::ErrorKind::NotFound {
				tracing::info!("LDAP sync cache missing");
				None
			} else {
				bail!(err)
			}
		}
	})
}

/// LDAP-specific configuration
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LdapSourceConfig {
	/// The URL of the LDAP/AD server
	pub url: Url,
	/// The base DN for searching users
	pub base_dn: String,
	/// The DN to bind for authentication
	pub bind_dn: String,
	/// The password for the bind DN
	pub bind_password: String,
	/// Filter to apply when searching for users, e.g., (objectClass=person) DO
	/// NOT FILTER STATUS!
	pub user_filter: String,
	/// Timeout for LDAP operations in seconds
	pub timeout: u64,
	/// A mapping from the mostly free-form LDAP attributes to
	/// attribute names as used by famedly
	pub attributes: LdapAttributesMapping,
	/// Whether to update deleted entries
	pub check_for_deleted_entries: bool,
	/// Whether to ask LDAP for specific attributes or just specify *.
	/// Various implementations either do or don't send data in both
	/// cases, so this needs to be tested against the actual server.
	pub use_attribute_filter: bool,
	/// TLS-related configuration
	pub tls: Option<LdapTlsConfig>,
	/// Where to cache the last known LDAP state
	pub cache_path: PathBuf,
}

impl From<LdapSourceConfig> for ldap_poller::Config {
	fn from(cfg: LdapSourceConfig) -> ldap_poller::Config {
		let starttls = cfg.tls.as_ref().is_some_and(|tls| tls.danger_use_start_tls);
		let no_tls_verify = cfg.tls.as_ref().is_some_and(|tls| tls.danger_disable_tls_verify);
		let root_certificates_path =
			cfg.tls.as_ref().and_then(|tls| tls.server_certificate.clone());
		let client_key_path = cfg.tls.as_ref().and_then(|tls| tls.client_key.clone());
		let client_certificate_path =
			cfg.tls.as_ref().and_then(|tls| tls.client_certificate.clone());

		let tls = TLSConfig {
			starttls,
			no_tls_verify,
			root_certificates_path,
			client_key_path,
			client_certificate_path,
		};

		let attributes = cfg.attributes;
		ldap_poller::Config {
			url: cfg.url,
			connection: ConnectionConfig {
				timeout: cfg.timeout,
				operation_timeout: std::time::Duration::from_secs(cfg.timeout),
				tls,
			},
			search_user: cfg.bind_dn,
			search_password: cfg.bind_password,
			searches: Searches {
				user_base: cfg.base_dn,
				user_filter: cfg.user_filter,
				page_size: None,
			},
			attributes: AttributeConfig {
				pid: attributes.user_id.get_name(),
				updated: attributes.last_modified.map(AttributeMapping::get_name),
				additional: vec![],
				filter_attributes: cfg.use_attribute_filter,
				attrs_to_track: vec![
					attributes.status.get_name(),
					attributes.first_name.get_name(),
					attributes.last_name.get_name(),
					attributes.preferred_username.get_name(),
					attributes.email.get_name(),
					attributes.phone.get_name(),
				],
			},
			cache_method: CacheMethod::ModificationTime,
			check_for_deleted_entries: cfg.check_for_deleted_entries,
		}
	}
}

/// A mapping from the mostly free-form LDAP attributes to attribute
/// names as used by famedly
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LdapAttributesMapping {
	/// Attribute for the user's first name
	pub first_name: AttributeMapping,
	/// Attribute for the user's last name
	pub last_name: AttributeMapping,
	/// Attribute for the user's preferred username
	pub preferred_username: AttributeMapping,
	/// Attribute for the user's email address
	pub email: AttributeMapping,
	/// Attribute for the user's phone number
	pub phone: AttributeMapping,
	/// Attribute for the user's unique ID
	pub user_id: AttributeMapping,
	/// This attribute shows the account status (It expects an i32 like
	/// userAccountControl in AD)
	pub status: AttributeMapping,
	/// Marks an account as disabled (for example userAccountControl: bit flag
	/// ACCOUNTDISABLE would be 2)
	#[serde(default)]
	pub disable_bitmasks: Vec<i32>,
	/// Last modified
	pub last_modified: Option<AttributeMapping>,
}

/// How an attribute should be defined in config - it can either be a
/// raw string, *or* it can be a struct defining both an attribute
/// name and whether the attribute should be treated as binary.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum AttributeMapping {
	/// An attribute that's defined without specifying whether it is
	/// binary or not
	NoBinaryOption(String),
	/// An attribute that specifies whether it is binary or not
	OptionalBinary {
		/// The name of the attribute
		name: String,
		/// Whether the attribute is binary
		#[serde(default)]
		is_binary: bool,
	},
}

impl AttributeMapping {
	/// Get the attribute name
	#[must_use]
	pub fn get_name(self) -> String {
		match self {
			Self::NoBinaryOption(name) => name,
			Self::OptionalBinary { name, .. } => name,
		}
	}
}

impl Display for AttributeMapping {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.clone().get_name())
	}
}

/// The LDAP TLS configuration
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LdapTlsConfig {
	/// Path to the client key; if not specified, it will be assumed
	/// that the server is configured not to verify client
	/// certificates.
	pub client_key: Option<PathBuf>,
	/// Path to the client certificate; if not specified, it will be
	/// assumed that the server is configured not to verify client
	/// certificates.
	pub client_certificate: Option<PathBuf>,
	/// Path to the server certificate; if not specified, the host's
	/// CA will be used to verify the server.
	pub server_certificate: Option<PathBuf>,
	/// Whether to verify the server's certificates.
	///
	/// This should normally only be used in test environments, as
	/// disabling certificate validation defies the purpose of using
	/// TLS in the first place.
	#[serde(default)]
	pub danger_disable_tls_verify: bool,
	/// Enable StartTLS, i.e., use the non-TLS ldap port, but send a
	/// special message to upgrade the connection to TLS.
	///
	/// This is less secure than standard TLS, an `ldaps` URL should
	/// be preferred.
	#[serde(default)]
	pub danger_use_start_tls: bool,
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use indoc::indoc;
	use ldap3::SearchEntry;
	use ldap_poller::ldap::EntryStatus;
	use tokio::sync::mpsc;

	use crate::{sources::ldap::LdapSource, user::StringOrBytes, Config};

	const EXAMPLE_CONFIG: &str = indoc! {r#"
        zitadel:
          url: http://localhost:8080
          key_file: tests/environment/zitadel/service-user.json
          organization_id: 1
          project_id: 1
          idp_id: 1

        sources:
          ldap:
            url: ldap://localhost:1389
            base_dn: ou=testorg,dc=example,dc=org
            bind_dn: cn=admin,dc=example,dc=org
            bind_password: adminpassword
            user_filter: "(objectClass=shadowAccount)"
            timeout: 5
            check_for_deleted_entries: true
            use_attribute_filter: true
            attributes:
              first_name: "cn"
              last_name: "sn"
              preferred_username: "displayName"
              email: "mail"
              phone: "telephoneNumber"
              user_id: "uid"
              status:
                name: "shadowFlag"
                is_binary: false
              disable_bitmasks: [0x2, 0x10]
            tls:
              client_key: ./tests/environment/certs/client.key
              client_certificate: ./tests/environment/certs/client.crt
              server_certificate: ./tests/environment/certs/server.crt
              danger_disable_tls_verify: false
              danger_use_start_tls: false
            cache_path: ./test

        feature_flags: []
	"#};

	fn load_config() -> Config {
		serde_yaml::from_str(EXAMPLE_CONFIG).expect("invalid config")
	}

	fn new_user() -> HashMap<String, Vec<String>> {
		HashMap::from([
			("cn".to_owned(), vec!["Test".to_owned()]),
			("sn".to_owned(), vec!["User".to_owned()]),
			("displayName".to_owned(), vec!["testuser".to_owned()]),
			("mail".to_owned(), vec!["testuser@example.com".to_owned()]),
			("telephoneNumber".to_owned(), vec!["123456789".to_owned()]),
			("uid".to_owned(), vec!["testuser".to_owned()]),
			("shadowFlag".to_owned(), vec!["0".to_owned()]),
		])
	}

	#[test]
	fn test_attribute_filter_use() {
		let config = load_config();

		let ldap_config = config.sources.ldap.expect("Expected LDAP config");

		assert_eq!(
			Into::<ldap_poller::Config>::into(ldap_config).attributes.get_attr_filter(),
			vec!["uid", "shadowFlag", "cn", "sn", "displayName", "mail", "telephoneNumber"]
		);
	}

	#[test]
	fn test_no_attribute_filters() {
		let config = load_config();

		let mut ldap_config = config.sources.ldap.as_ref().expect("Expected LDAP config").clone();

		ldap_config.use_attribute_filter = false;

		assert_eq!(
			Into::<ldap_poller::Config>::into(ldap_config).attributes.get_attr_filter(),
			vec!["*"]
		);
	}

	#[tokio::test]
	async fn test_get_user_changes_new_and_changed() {
		let (tx, rx) = mpsc::channel(32);
		let config = load_config();
		let ldap_source =
			LdapSource { ldap_config: config.sources.ldap.unwrap(), is_dry_run: false };

		let mut user = new_user();

		// Simulate new user entry
		tx.send(EntryStatus::New(SearchEntry {
			dn: "uid=testuser,ou=testorg,dc=example,dc=org".to_owned(),
			attrs: user.clone(),
			bin_attrs: HashMap::new(),
		}))
		.await
		.unwrap();

		// Modify user attributes to simulate a change
		user.insert("mail".to_owned(), vec!["newemail@example.com".to_owned()]);
		user.insert("telephoneNumber".to_owned(), vec!["987654321".to_owned()]);

		// Simulate changed user entry
		tx.send(EntryStatus::Changed {
			old: SearchEntry {
				dn: "uid=testuser,ou=testorg,dc=example,dc=org".to_owned(),
				attrs: new_user(),
				bin_attrs: HashMap::new(),
			},
			new: SearchEntry {
				dn: "uid=testuser,ou=testorg,dc=example,dc=org".to_owned(),
				attrs: user.clone(),
				bin_attrs: HashMap::new(),
			},
		})
		.await
		.unwrap();

		// Close the sender side of the channel
		drop(tx);

		let result = ldap_source.get_user_changes(rx).await;

		assert!(result.is_ok(), "Failed to get user changes: {:?}", result);
		let (added, changed, removed) = result.unwrap();
		assert_eq!(added.len(), 1, "Unexpected number of added users");
		assert_eq!(changed.len(), 1, "Unexpected number of changed users");
		assert_eq!(removed.len(), 0, "Unexpected number of removed users");

		// Verify the changes
		let changed_user_entry = &changed[0].1;
		assert_eq!(
			changed_user_entry.email,
			StringOrBytes::String("newemail@example.com".to_owned())
		);
		assert_eq!(changed_user_entry.phone, Some(StringOrBytes::String("987654321".to_owned())));
	}

	#[tokio::test]
	async fn test_get_user_changes_removed() {
		let (tx, rx) = mpsc::channel(32);
		let config = load_config();
		let ldap_source =
			LdapSource { ldap_config: config.sources.ldap.unwrap(), is_dry_run: false };

		let user = new_user();

		// Simulate new user entry
		tx.send(EntryStatus::New(SearchEntry {
			dn: "uid=testuser,ou=testorg,dc=example,dc=org".to_owned(),
			attrs: user.clone(),
			bin_attrs: HashMap::new(),
		}))
		.await
		.unwrap();

		// Simulate removed user entry
		tx.send(EntryStatus::Removed("uid=testuser".as_bytes().to_vec())).await.unwrap();

		// Close the sender side of the channel
		drop(tx);

		let result = ldap_source.get_user_changes(rx).await;

		assert!(result.is_ok(), "Failed to get user changes: {:?}", result);
		let (added, changed, removed) = result.unwrap();
		assert_eq!(added.len(), 1, "Unexpected number of added users");
		assert_eq!(changed.len(), 0, "Unexpected number of changed users");
		assert_eq!(removed.len(), 1, "Unexpected number of removed users");
	}

	#[tokio::test]
	async fn test_parse_user() {
		let config = load_config();
		let ldap_source =
			LdapSource { ldap_config: config.sources.ldap.unwrap(), is_dry_run: false };

		let entry = SearchEntry {
			dn: "uid=testuser,ou=testorg,dc=example,dc=org".to_owned(),
			attrs: new_user(),
			bin_attrs: HashMap::new(),
		};

		let result = ldap_source.parse_user(entry);
		assert!(result.is_ok(), "Failed to parse user: {:?}", result);
		let user = result.unwrap();
		assert_eq!(user.first_name, StringOrBytes::String("Test".to_owned()));
		assert_eq!(user.last_name, StringOrBytes::String("User".to_owned()));
		assert_eq!(user.preferred_username, StringOrBytes::String("testuser".to_owned()));
		assert_eq!(user.email, StringOrBytes::String("testuser@example.com".to_owned()));
		assert_eq!(user.phone, Some(StringOrBytes::String("123456789".to_owned())));
		assert_eq!(user.preferred_username, StringOrBytes::String("testuser".to_owned()));
		assert_eq!(user.external_user_id, StringOrBytes::String("testuser".to_owned()));
		assert!(user.enabled);
	}

	#[tokio::test]
	async fn test_text_enabled() {
		let mut config = load_config();
		config.sources.ldap.as_mut().unwrap().attributes.disable_bitmasks =
			serde_yaml::from_str("[0]").expect("invalid config fragment");
		let ldap_source =
			LdapSource { ldap_config: config.sources.ldap.unwrap(), is_dry_run: false };

		for (attr, parsed) in [("TRUE", true), ("FALSE", false)] {
			let entry = SearchEntry {
				dn: "uid=testuser,ou=testorg,dc=example,dc=org".to_owned(),
				attrs: {
					let mut user = new_user();
					user.insert("shadowFlag".to_owned(), vec![attr.to_owned()]);
					user
				},
				bin_attrs: HashMap::new(),
			};

			let result = ldap_source.parse_user(entry);
			assert!(result.is_ok(), "Failed to parse user: {:?}", result);
			let user = result.unwrap();
			assert_eq!(user.enabled, parsed);
		}
	}
}
