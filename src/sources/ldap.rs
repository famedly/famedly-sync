//! LDAP source for syncing with Famedly's Zitadel.

use std::{fmt::Display, path::PathBuf};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::prelude::{Engine, BASE64_STANDARD};
use ldap_poller::{
	config::TLSConfig, ldap::EntryStatus, ldap3::SearchEntry, AttributeConfig, CacheMethod,
	ConnectionConfig, Ldap, SearchEntryExt, Searches,
};
use serde::Deserialize;
use tokio::sync::mpsc::Receiver;
use tokio_stream::{wrappers::ReceiverStream, StreamExt};
use url::Url;

use super::Source;
use crate::user::User;

/// LDAP sync source
pub struct LdapSource {
	/// LDAP configuration
	ldap_config: LdapSourceConfig,
}

#[async_trait]
impl Source for LdapSource {
	fn get_name(&self) -> &'static str {
		"LDAP"
	}

	async fn get_sorted_users(&self) -> Result<Vec<User>> {
		let (mut ldap_client, ldap_receiver) = Ldap::new(self.ldap_config.clone().into(), None);

		let sync_handle: tokio::task::JoinHandle<Result<_>> = tokio::spawn(async move {
			ldap_client.sync_once(None).await.context("failed to sync/fetch data from LDAP")?;
			tracing::info!("Finished syncing LDAP data");
			Ok(())
		});

		let mut added = self.get_user_changes(ldap_receiver).await?;
		sync_handle.await??;

		// TODO: Find out if we can use the AD extension for receiving sorted data
		added.sort_by(|a, b| a.external_user_id.cmp(&b.external_user_id));

		Ok(added)
	}
}

impl LdapSource {
	/// Create a new LDAP source
	pub fn new(ldap_config: LdapSourceConfig) -> Self {
		Self { ldap_config }
	}

	/// Get user changes from an ldap receiver
	pub async fn get_user_changes(
		&self,
		ldap_receiver: Receiver<EntryStatus>,
	) -> Result<Vec<User>> {
		ReceiverStream::new(ldap_receiver)
			.fold(Ok(vec![]), |acc, entry_status| {
				let mut added = acc?;
				if let EntryStatus::New(entry) = entry_status {
					tracing::debug!("New entry: {:?}", entry);
					added.push(self.parse_user(entry)?);
				};
				Ok(added)
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

		let ldap_user_id = match read_search_entry(&entry, &self.ldap_config.attributes.user_id)? {
			// TODO(tlater): Use an encoding that preserves alphabetic order
			StringOrBytes::Bytes(byte_id) => BASE64_STANDARD.encode(byte_id),
			StringOrBytes::String(string_id) => BASE64_STANDARD.encode(string_id),
		};

		let first_name =
			read_string_entry(&entry, &self.ldap_config.attributes.first_name, &ldap_user_id)?;
		let last_name =
			read_string_entry(&entry, &self.ldap_config.attributes.last_name, &ldap_user_id)?;
		let preferred_username = read_string_entry(
			&entry,
			&self.ldap_config.attributes.preferred_username,
			&ldap_user_id,
		)?;
		let email = read_string_entry(&entry, &self.ldap_config.attributes.email, &ldap_user_id)?;
		let phone =
			read_string_entry(&entry, &self.ldap_config.attributes.phone, &ldap_user_id).ok();

		Ok(User {
			first_name,
			last_name,
			preferred_username: Some(preferred_username),
			email,
			external_user_id: ldap_user_id,
			phone,
			enabled,
		})
	}
}

/// Read an an attribute, but assert that it is a string
fn read_string_entry(
	entry: &SearchEntry,
	attribute: &AttributeMapping,
	id: &str,
) -> Result<String> {
	match read_search_entry(entry, attribute)? {
		StringOrBytes::String(entry) => Ok(entry),
		StringOrBytes::Bytes(_) => {
			Err(anyhow!("Unacceptable binary value for {} of user `{}`", attribute, id))
		}
	}
}

/// Read an attribute from the entry
fn read_search_entry(entry: &SearchEntry, attribute: &AttributeMapping) -> Result<StringOrBytes> {
	match attribute {
		AttributeMapping::OptionalBinary { name, is_binary: false }
		| AttributeMapping::NoBinaryOption(name) => {
			entry.attr_first(name).map(|entry| StringOrBytes::String(entry.to_owned()))
		}
		AttributeMapping::OptionalBinary { name, is_binary: true } => entry
			.bin_attr_first(name)
			// If an entry encodes as UTF-8, it will still only be
			// available from the `.attr_first` function, even if ldap
			// presents it with the `::` delimiter.
			//
			// Hence the configuration, we just treat it as binary
			// data if this is requested.
			.or_else(|| entry.attr_first(name).map(str::as_bytes))
			.map(|entry| StringOrBytes::Bytes(entry.to_vec())),
	}
	.ok_or(anyhow!("missing `{}` values for `{}`", attribute, entry.dn))
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
			cache_method: CacheMethod::Disabled,
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

/// A structure that can either be a string or bytes
#[derive(Clone, Debug)]
enum StringOrBytes {
	/// A string
	String(String),
	/// A byte string
	Bytes(Vec<u8>),
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use base64::prelude::{Engine, BASE64_STANDARD};
	use indoc::indoc;
	use ldap3::SearchEntry;
	use ldap_poller::ldap::EntryStatus;
	use tokio::sync::mpsc;

	use crate::{sources::ldap::LdapSource, Config};

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
		let ldap_source = LdapSource { ldap_config: config.sources.ldap.unwrap() };

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
		let added = result.unwrap();
		assert_eq!(added.len(), 1, "Unexpected number of added users");
	}

	#[tokio::test]
	async fn test_get_user_changes_removed() {
		let (tx, rx) = mpsc::channel(32);
		let config = load_config();
		let ldap_source = LdapSource { ldap_config: config.sources.ldap.unwrap() };

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
		let added = result.unwrap();
		assert_eq!(added.len(), 1, "Unexpected number of added users");
	}

	#[tokio::test]
	async fn test_parse_user() {
		let config = load_config();
		let ldap_source = LdapSource { ldap_config: config.sources.ldap.unwrap() };

		let entry = SearchEntry {
			dn: "uid=testuser,ou=testorg,dc=example,dc=org".to_owned(),
			attrs: new_user(),
			bin_attrs: HashMap::new(),
		};

		let result = ldap_source.parse_user(entry);
		assert!(result.is_ok(), "Failed to parse user: {:?}", result);
		let user = result.unwrap();
		assert_eq!(user.first_name, "Test");
		assert_eq!(user.last_name, "User");
		assert_eq!(user.preferred_username, Some("testuser".to_owned()));
		assert_eq!(user.email, "testuser@example.com");
		assert_eq!(user.phone, Some("123456789".to_owned()));
		assert_eq!(user.preferred_username, Some("testuser".to_owned()));
		assert_eq!(user.external_user_id, BASE64_STANDARD.encode("testuser"));
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
