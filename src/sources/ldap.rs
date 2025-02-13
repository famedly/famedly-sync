//! LDAP source for syncing with Famedly's Zitadel.

use std::{fmt::Display, path::PathBuf, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use itertools::Itertools;
use ldap3::{LdapConnAsync, LdapConnSettings, Scope, SearchEntry};
use native_tls::{Certificate, Identity, TlsConnector};
use serde::Deserialize;
use url::Url;

use super::Source;
use crate::user::{self, User};

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
		let (conn, mut ldap) = LdapConnAsync::from_url_with_settings(
			self.ldap_config.clone().try_into()?,
			&self.ldap_config.url,
		)
		.await?;

		let connection_result = ldap3::drive!(conn);

		ldap.with_timeout(Duration::from_secs(self.ldap_config.timeout))
			.simple_bind(&self.ldap_config.bind_dn, &self.ldap_config.bind_password)
			.await?
			.non_error()?;

		// We *could* use the streaming search instead, as that
		// *could* let up on memory pressure, however we end up
		// sorting the list in-memory later anyway.
		//
		// TODO: Use streaming search when we have a way to receive
		// pre-sorted results.
		let (search_results, _stats) = ldap
			.search(
				&self.ldap_config.base_dn,
				Scope::Subtree,
				&self.ldap_config.user_filter,
				self.ldap_config.clone().get_attribute_list(),
			)
			.await?
			.non_error()?;

		let mut users: Vec<User> = search_results
			.into_iter()
			.map(SearchEntry::construct)
			.map(|entry| self.parse_user(entry))
			.try_collect()?;

		// Check if there were any connection errors before proceeding
		// with an expensive sort
		ldap.unbind().await?;
		connection_result.await.context("Connection to ldap server failed")?;

		// There are LDAP extensions that permit sorting, however they
		// seem to be largely best-effort, and the server may just
		// return unsorted results if it doesn't feel like it or the
		// user is not permitted to sort (yeah...).
		//
		// Since having sorted lists is *really* important to the sync
		// algorithm, we shouldn't try to rely on this without a good
		// amount of testing.
		//
		// TODO: Find out if we can use the AD extension for receiving sorted data
		users.sort_by(|a, b| a.external_user_id.cmp(&b.external_user_id));

		Ok(users)
	}
}

impl LdapSource {
	/// Create a new LDAP source
	pub fn new(ldap_config: LdapSourceConfig) -> Self {
		Self { ldap_config }
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
					StringOrBytes::String(status) => {
						status.parse::<i32>().context("failed to parse status attribute")?
					}
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

		let (ldap_user_id, localpart) =
			match read_search_entry(&entry, &self.ldap_config.attributes.user_id)? {
				// Use hex encoding instead of base64 for consistent alphabetical order
				StringOrBytes::Bytes(byte_id) => {
					(hex::encode(&byte_id), user::compute_famedly_uuid(&byte_id))
				}
				StringOrBytes::String(string_id) => (
					hex::encode(string_id.as_bytes()),
					user::compute_famedly_uuid(string_id.as_bytes()),
				),
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
			preferred_username,
			email,
			external_user_id: ldap_user_id,
			phone,
			enabled,
			localpart,
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
		StringOrBytes::Bytes(_) => Err(anyhow!(
			"Binary values are not accepted: attribute `{}` of user `{}`",
			attribute,
			id
		)),
	}
}

/// Read an attribute from the entry
fn read_search_entry(entry: &SearchEntry, attribute: &AttributeMapping) -> Result<StringOrBytes> {
	match attribute {
		AttributeMapping::OptionalBinary { name, is_binary: false }
		| AttributeMapping::NoBinaryOption(name) => entry
			.attrs
			.get(name)
			.and_then(|entry| entry.first())
			.map(|entry| StringOrBytes::String(entry.to_owned())),

		AttributeMapping::OptionalBinary { name, is_binary: true } => entry
			.bin_attrs
			.get(name)
			// If an entry encodes as UTF-8, it will still only be
			// available from the `.attr_first` function, even if ldap
			// presents it with the `::` delimiter.
			//
			// Hence the configuration, we just treat it as binary
			// data if this is requested.
			.and_then(|entry| entry.first().cloned())
			.or_else(|| {
				entry
					.attrs
					.get(name)
					.and_then(|entry| entry.first())
					.map(|entry| entry.as_bytes().to_vec())
			})
			.map(StringOrBytes::Bytes),
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

impl LdapSourceConfig {
	/// Get the attribute list, taking into account whether we should
	/// be using the attribute filter or not.
	fn get_attribute_list(self) -> Vec<String> {
		if self.use_attribute_filter {
			self.attributes.get_attribute_list()
		} else {
			vec!["*".to_owned()]
		}
	}
}

impl TryFrom<LdapSourceConfig> for LdapConnSettings {
	type Error = anyhow::Error;

	fn try_from(cfg: LdapSourceConfig) -> Result<Self> {
		let mut settings = LdapConnSettings::new()
			.set_starttls(cfg.tls.as_ref().is_some_and(|tls| tls.danger_use_start_tls))
			.set_no_tls_verify(cfg.tls.as_ref().is_some_and(|tls| tls.danger_disable_tls_verify));

		if let Some(tls) = cfg.tls {
			let root_cert: Option<Certificate> = tls
				.server_certificate
				.as_ref()
				.map(std::fs::read)
				.transpose()
				.context("Failed to read server certificate")?
				.map(|cert_data| Certificate::from_pem(cert_data.as_slice()))
				.transpose()
				.context("Invalid server certificate")?;

			let identity: Option<Identity> = match (tls.client_key, tls.client_certificate) {
				(Some(client_key), Some(client_cert)) => Some(
					Identity::from_pkcs8(
						std::fs::read(client_cert)?.as_slice(),
						std::fs::read(client_key)?.as_slice(),
					)
					.context("Could not create client identity")?,
				),
				(None, None) => None,
				_ => {
					bail!("Both client key *and* certificate must be specified")
				}
			};

			if root_cert.is_some() || identity.is_some() {
				let mut connector = TlsConnector::builder();

				if let Some(root_cert) = root_cert {
					connector.add_root_certificate(root_cert);
				}

				if let Some(identity) = identity {
					connector.identity(identity);
				}

				settings = settings.set_connector(connector.build()?);
			};
		};

		Ok(settings)
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

impl LdapAttributesMapping {
	/// Get the attribute list; *Some* LDAP implementations accept
	/// `[*]` to report all attributes, but notably AD does not, so we
	/// need to send an exhaustive list of all attributes we want to
	/// get back.
	fn get_attribute_list(self) -> Vec<String> {
		let mut attrs = vec![
			self.first_name.get_name(),
			self.last_name.get_name(),
			self.preferred_username.get_name(),
			self.email.get_name(),
			self.phone.get_name(),
			self.user_id.get_name(),
			self.status.get_name(),
		];

		if let Some(last_modified) = self.last_modified {
			attrs.push(last_modified.get_name());
		}

		attrs
	}
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

	use indoc::indoc;
	use itertools::Itertools;
	use ldap3::SearchEntry;

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
              last_modified: "timestamp"
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
			ldap_config.get_attribute_list().into_iter().sorted().collect_vec(),
			vec![
				"uid",
				"shadowFlag",
				"cn",
				"sn",
				"displayName",
				"mail",
				"telephoneNumber",
				"timestamp"
			]
			.into_iter()
			.sorted()
			.collect_vec()
		);
	}

	#[test]
	fn test_no_attribute_filters() {
		let config = load_config();
		let mut ldap_config = config.sources.ldap.as_ref().expect("Expected LDAP config").clone();
		ldap_config.use_attribute_filter = false;

		assert_eq!(ldap_config.get_attribute_list(), vec!["*"]);
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
		assert_eq!(user.preferred_username, "testuser".to_owned());
		assert_eq!(user.email, "testuser@example.com");
		assert_eq!(user.phone, Some("123456789".to_owned()));
		assert_eq!(user.preferred_username, "testuser".to_owned());
		assert_eq!(user.external_user_id, hex::encode("testuser"));
		assert!(user.enabled);
	}

	#[tokio::test]
	async fn test_text_enabled() {
		let mut config = load_config();
		config.sources.ldap.as_mut().unwrap().attributes.disable_bitmasks =
			serde_yaml::from_str("[0]").expect("invalid config fragment");
		let ldap_source = LdapSource { ldap_config: config.sources.ldap.unwrap() };

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
