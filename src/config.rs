//! All sync client configuration structs and logic
use std::{
	ops::{Deref, DerefMut},
	path::Path,
};

use anyhow::{bail, Result};
use serde::Deserialize;
use tracing::{error, warn};
use url::Url;

use crate::{
	sources::{
		csv::{CsvSource, CsvSourceConfig},
		ldap::{LdapSource, LdapSourceConfig},
		ukt::{UktSource, UktSourceConfig},
		Source,
	},
	zitadel::{Zitadel, ZitadelConfig},
};

/// App prefix for env var configuration
const ENV_VAR_CONFIG_PREFIX: &str = "FAMEDLY_SYNC";
/// Separator for setting a list using env vars
const ENV_VAR_LIST_SEP: &str = " ";

/// The main sync tool with all configurations
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Config {
	/// Configuration related to Zitadel provided by Famedly
	pub zitadel: ZitadelConfig,
	/// Sources configuration
	pub sources: SourcesConfig,
	/// Optional sync tool log level
	pub log_level: Option<String>,
	/// Opt-in features
	#[serde(default)]
	pub feature_flags: FeatureFlags,
}

/// Configuration for sources
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SourcesConfig {
	/// Optional LDAP configuration
	pub ldap: Option<LdapSourceConfig>,
	/// Optional UKT configuration
	pub ukt: Option<UktSourceConfig>,
	/// Optional CSV configuration
	pub csv: Option<CsvSourceConfig>,
}

impl Config {
	/// Create new config from file and env var
	pub fn new(path: &Path) -> Result<Self> {
		let config_builder = config::Config::builder()
			.add_source(config::File::from(path).required(false))
			.add_source(
				config::Environment::with_prefix(ENV_VAR_CONFIG_PREFIX)
					.separator("__")
					.list_separator(ENV_VAR_LIST_SEP)
					.with_list_parse_key("sources.ldap.attributes.disable_bitmasks")
					.with_list_parse_key("feature_flags")
					.try_parsing(true),
			);

		let config_builder = config_builder.build()?;

		let config: Config = config_builder.try_deserialize()?;

		config.validate()
	}

	/// Validate the config and return a valid configuration
	fn validate(mut self) -> Result<Self> {
		self.zitadel.url = validate_zitadel_url(self.zitadel.url)?;

		Ok(self)
	}

	/// Perform a sync operation
	pub async fn perform_sync(&self) -> Result<()> {
		if !self.feature_flags.is_enabled(FeatureFlag::SsoLogin) {
			anyhow::bail!("Non-SSO configuration is currently not supported");
		}

		let mut sources: Vec<Box<dyn Source + Send + Sync>> = Vec::new();

		if let Some(ldap_config) = &self.sources.ldap {
			let ldap = LdapSource::new(
				ldap_config.clone(),
				self.feature_flags.is_enabled(FeatureFlag::DryRun),
			);
			sources.push(Box::new(ldap));
		}

		if let Some(ukt_config) = &self.sources.ukt {
			let ukt = UktSource::new(ukt_config.clone());
			sources.push(Box::new(ukt));
		}

		if let Some(csv_config) = &self.sources.csv {
			let csv = CsvSource::new(csv_config.clone());
			sources.push(Box::new(csv));
		}

		// Setup Zitadel client
		let zitadel = Zitadel::new(self).await?;

		// Sync from each available source
		for source in sources.iter() {
			let diff = match source.get_diff().await {
				Ok(diff) => diff,
				Err(e) => {
					error!("Failed to get diff from {}: {:?}", source.get_name(), e);
					continue;
				}
			};

			if !self.feature_flags.is_enabled(FeatureFlag::DeactivateOnly) {
				if let Err(e) = zitadel.import_new_users(diff.new_users).await {
					warn!("Failed to import new users from {}: {:?}", source.get_name(), e);
				}
				if let Err(e) = zitadel.delete_users_by_id(diff.deleted_user_ids).await {
					warn!("Failed to delete users from {}: {:?}", source.get_name(), e);
				}
			}

			if let Err(e) = zitadel.update_users(diff.changed_users).await {
				warn!("Failed to update users from {}: {:?}", source.get_name(), e);
			}
		}

		Ok(())
	}
}

/// Opt-in features
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FeatureFlag {
	/// If SSO should be activated. It requires idpId, idpUserName, idpUserId
	/// mapping
	SsoLogin,
	/// If users should verify the mail. Users will receive a verification mail
	VerifyEmail,
	/// If users should verify the phone. Users will receive a verification sms
	VerifyPhone,
	/// If set, only log changes instead of writing anything
	DryRun,
	/// If only deactivated users should be synced
	DeactivateOnly,
	/// Use plain localpart
	PlainLocalpart,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct FeatureFlags(Vec<FeatureFlag>);

impl Deref for FeatureFlags {
	type Target = Vec<FeatureFlag>;

	fn deref(&self) -> &Self::Target {
		&self.0
	}
}

impl DerefMut for FeatureFlags {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.0
	}
}

impl FeatureFlags {
	/// Whether a feature flag is enabled
	pub fn is_enabled(&self, flag: FeatureFlag) -> bool {
		self.contains(&flag)
	}
}

/// Validate the Zitadel URL provided by Famedly
fn validate_zitadel_url(url: Url) -> Result<Url> {
	// If a URL contains a port, the domain name may appear as a
	// scheme and pass through URL parsing despite lacking a scheme
	if url.scheme() != "https" && url.scheme() != "http" {
		bail!("zitadel URL scheme must be `http` or `https`, e.g. `https://{}`", url);
	}

	Ok(url)
}

// Run these tests with
// RUST_TEST_THREADS=1 cargo test --lib
#[cfg(test)]
mod tests {
	use std::{env, fs::File, io::Write, path::PathBuf};

	use indoc::indoc;
	use tempfile::TempDir;

	use super::*;

	const EXAMPLE_CONFIG: &str = indoc! {r#"
        zitadel:
          url: http://localhost:8080
          key_file: tests/environment/zitadel/service-user.json
          organization_id: 1
          project_id: 1
          idp_id: 1

        sources:
          test: 1

        feature_flags: []
	"#};

	fn load_config() -> Config {
		serde_yaml::from_str(EXAMPLE_CONFIG).expect("invalid config")
	}

	fn example_env_vars() -> Vec<(String, String)> {
		let config: serde_yaml::Value =
			serde_yaml::from_str(EXAMPLE_CONFIG).expect("invalid config");
		let mut prefix_stack = Vec::new();
		get_env_vars_from_map(
			config.as_mapping().expect("Expected a map but it isn't"),
			&mut prefix_stack,
		)
	}

	fn get_string(value: &serde_yaml::Value) -> String {
		match value {
			serde_yaml::Value::Bool(value) => value.to_string(),
			serde_yaml::Value::Number(value) => value.to_string(),
			serde_yaml::Value::String(value) => value.to_string(),
			serde_yaml::Value::Sequence(arr) => {
				let mut values: Vec<String> = Vec::new();
				for value in arr {
					values.push(get_string(value));
				}
				values.join(ENV_VAR_LIST_SEP)
			}
			_ => "".to_owned(),
		}
	}

	fn get_env_vars_from_map(
		map: &serde_yaml::Mapping,
		prefix_stack: &mut Vec<String>,
	) -> Vec<(String, String)> {
		let mut ret = Vec::new();
		for (key, value) in map {
			let key = key.as_str().expect("Key should be a str").to_owned().to_uppercase();
			if value.is_mapping() {
				prefix_stack.push(key);
				ret.append(&mut get_env_vars_from_map(value.as_mapping().unwrap(), prefix_stack));
				let _ = prefix_stack.pop();
			} else {
				let var_key = if prefix_stack.is_empty() {
					format!("{ENV_VAR_CONFIG_PREFIX}__{key}")
				} else {
					format!(
						"{ENV_VAR_CONFIG_PREFIX}__{}__{key}",
						prefix_stack.join("__").to_uppercase()
					)
				};
				let var_value = get_string(value);
				ret.push((var_key, var_value));
			}
		}
		ret
	}

	fn create_config_file(dir: &Path) -> PathBuf {
		let file_path = dir.join("config.yaml");
		let mut config_file = File::create(&file_path).expect("failed to create config file");
		config_file
			.write_all(EXAMPLE_CONFIG.as_bytes())
			.expect("Failed to write config file content");
		file_path
	}

	#[test]
	fn test_zitadel_url_validate_valid() {
		let url = Url::parse("https://famedly.de").expect("invalid url");
		let validated = validate_zitadel_url(url).expect("url failed to validate");
		assert_eq!(validated.to_string(), "https://famedly.de/");
	}

	#[test]
	fn test_zitadel_url_validate_trailing_slash_path() {
		let url = Url::parse("https://famedly.de/test/").expect("invalid url");
		let validated = validate_zitadel_url(url).expect("url failed to validate");
		assert_eq!(validated.to_string(), "https://famedly.de/test/");
	}

	#[test]
	fn test_zitadel_url_validate_scheme() {
		let url = Url::parse("famedly.de:443").expect("invalid url");
		assert!(validate_zitadel_url(url).is_err());
	}

	#[tokio::test]
	async fn test_sample_config() {
		let config = Config::new(Path::new("./config.sample.yaml"));

		assert!(config.is_ok(), "Invalid config: {:?}", config);
	}

	#[test]
	fn test_config_from_file() {
		let tempdir = TempDir::new().expect("failed to initialize cache dir");
		let file_path = create_config_file(tempdir.path());
		let config = Config::new(file_path.as_path()).expect("Failed to create config object");

		assert_eq!(load_config(), config);
	}

	#[test]
	fn test_config_env_var_override() {
		let tempdir = TempDir::new().expect("failed to initialize cache dir");
		let file_path = create_config_file(tempdir.path());

		let env_var_name = format!("{ENV_VAR_CONFIG_PREFIX}__FEATURE_FLAGS");
		env::set_var(&env_var_name, "dry_run");

		let loaded_config =
			Config::new(file_path.as_path()).expect("Failed to create config object");
		env::remove_var(env_var_name);

		let mut sample_config = load_config();
		sample_config.feature_flags.push(FeatureFlag::DryRun);

		assert_eq!(sample_config, loaded_config);
	}

	#[test]
	fn test_no_config_file() {
		let env_vars = example_env_vars();
		for (key, value) in &env_vars {
			if !value.is_empty() {
				env::set_var(key, value);
			}
		}

		let config =
			Config::new(Path::new("no_file.yaml")).expect("Failed to create config object");

		for (key, _) in &env_vars {
			env::remove_var(key);
		}

		assert_eq!(load_config(), config);
	}

	#[test]
	fn test_config_env_var_feature_flag() {
		let tempdir = TempDir::new().expect("failed to initialize cache dir");
		let file_path = create_config_file(tempdir.path());

		let env_var_name = format!("{ENV_VAR_CONFIG_PREFIX}__FEATURE_FLAGS");
		env::set_var(
			&env_var_name,
			"sso_login verify_email verify_phone dry_run deactivate_only plain_localpart",
		);

		let loaded_config =
			Config::new(file_path.as_path()).expect("Failed to create config object");
		let mut sample_config = load_config();

		sample_config.feature_flags.push(FeatureFlag::SsoLogin);
		sample_config.feature_flags.push(FeatureFlag::VerifyEmail);
		sample_config.feature_flags.push(FeatureFlag::VerifyPhone);
		sample_config.feature_flags.push(FeatureFlag::DryRun);
		sample_config.feature_flags.push(FeatureFlag::DeactivateOnly);
		sample_config.feature_flags.push(FeatureFlag::PlainLocalpart);

		env::remove_var(env_var_name);

		assert_eq!(sample_config, loaded_config);
	}
}
