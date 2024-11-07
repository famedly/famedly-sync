//! CSV source for syncing with Famedly's Zitadel.

use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use csv::Reader;
use serde::Deserialize;

use super::Source;
use crate::user::User;

/// CSV Source
pub struct CsvSource {
	/// CSV Source configuration
	csv_config: CsvSourceConfig,
}

#[async_trait]
impl Source for CsvSource {
	fn get_name(&self) -> &'static str {
		"CSV"
	}

	async fn get_sorted_users(&self) -> Result<Vec<User>> {
		let mut new_users = self.read_csv()?;
		new_users.sort_by(|a, b| a.external_user_id.cmp(&b.external_user_id));
		return Ok(new_users);
	}
}

impl CsvSource {
	/// Create a new CSV source
	pub fn new(csv_config: CsvSourceConfig) -> Self {
		Self { csv_config }
	}

	/// Get list of users from CSV file
	fn read_csv(&self) -> Result<Vec<User>> {
		let file_path = &self.csv_config.file_path;
		let file = fs::File::open(&self.csv_config.file_path)
			.context(format!("Failed to open CSV file {}", file_path.to_string_lossy()))?;
		let mut reader = Reader::from_reader(file);
		Ok(reader
			.deserialize()
			.map(|r| r.inspect_err(|x| tracing::error!("Failed to deserialize: {x}")))
			.filter_map(Result::ok)
			.map(CsvData::to_user)
			.collect())
	}
}

/// Configuration to get a list of users from a CSV file
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CsvSourceConfig {
	/// The path to the CSV file
	pub file_path: PathBuf,
}

/// CSV data structure
#[derive(Debug, Deserialize)]
struct CsvData {
	/// The user's email address
	email: String,
	/// The user's first name
	first_name: String,
	/// The user's last name
	last_name: String,
	/// The user's phone number
	phone: String,
}

impl CsvData {
	/// Convert CsvData to User data
	fn to_user(csv_data: CsvData) -> User {
		User {
			email: csv_data.email.clone(),
			first_name: csv_data.first_name,
			last_name: csv_data.last_name,
			phone: if csv_data.phone.is_empty() { None } else { Some(csv_data.phone) },
			preferred_username: Some(csv_data.email.clone()),
			external_user_id: csv_data.email,
			enabled: true,
		}
	}
}

/// Helper module for unit and e2e tests
pub mod test_helpers {
	use std::fs::write;

	use anyhow::Result;
	use tempfile::NamedTempFile;

	use crate::Config;

	/// Prepare a temporary CSV file with the given content and update the
	/// config to use it as the CSV source file.
	pub fn temp_csv_file(config: &mut Config, csv_content: &str) -> Result<NamedTempFile> {
		let temp_file = NamedTempFile::new()?;
		write(temp_file.path(), csv_content)?;

		if let Some(csv) = config.sources.csv.as_mut() {
			csv.file_path = temp_file.path().to_path_buf();
		}

		Ok(temp_file)
	}
}

#[cfg(test)]
mod tests {

	use indoc::indoc;

	use super::*;
	use crate::Config;

	const EXAMPLE_CONFIG: &str = indoc! {r#"
        zitadel:
          url: http://localhost:8080
          key_file: tests/environment/zitadel/service-user.json
          organization_id: 1
          project_id: 1
          idp_id: 1

        sources:
          csv:
            file_path: ./test_users.csv

        feature_flags: [verify_phone]
    "#};

	fn load_config() -> Config {
		serde_yaml::from_str(EXAMPLE_CONFIG).expect("invalid config")
	}

	#[test]
	fn test_get_users() {
		let mut config = load_config();
		let csv_content = indoc! {r#"
          email,first_name,last_name,phone
          john.doe@example.com,John,Doe,+1111111111
          jane.smith@example.com,Jane,Smith,+2222222222
          alice.johnson@example.com,Alice,Johnson,
          bob.williams@example.com,Bob,Williams,+4444444444
        "#};
		let _file = test_helpers::temp_csv_file(&mut config, csv_content);

		let csv_config = config.sources.csv.expect("CsvSource configuration is missing");
		let csv = CsvSource::new(csv_config);

		let result = csv.read_csv();
		assert!(result.is_ok(), "Failed to get users: {:?}", result);

		let users = result.expect("Failed to get users");
		assert_eq!(users.len(), 4, "Unexpected number of users");
		assert_eq!(users[0].first_name, "John", "Unexpected first name at index 0");
		assert_eq!(users[0].email, "john.doe@example.com", "Unexpected email at index 0");
		assert_eq!(users[3].last_name, "Williams", "Unexpected last name at index 3");
		assert_eq!(users[2].phone, None, "Unexpected phone at index 2");
		assert_eq!(users[3].phone, Some("+4444444444".to_owned()), "Unexpected phone at index 3");
	}

	#[test]
	fn test_get_users_empty_file() {
		let mut config = load_config();
		let csv_content = indoc! {r#"
          email,first_name,last_name,phone
        "#};
		let _file = test_helpers::temp_csv_file(&mut config, csv_content);

		let csv_config = config.sources.csv.expect("CsvSource configuration is missing");
		let csv = CsvSource::new(csv_config);

		let result = csv.read_csv();
		assert!(result.is_ok(), "Failed to get users: {:?}", result);

		let users = result.expect("Failed to get users");
		assert_eq!(users.len(), 0, "Expected empty user list");
	}

	#[test]
	fn test_get_users_invalid_file() {
		let mut config = load_config();
		if let Some(csv) = config.sources.csv.as_mut() {
			csv.file_path = PathBuf::from("invalid_path.csv");
		}

		let csv_config = config.sources.csv.expect("CsvSource configuration is missing");
		let csv = CsvSource::new(csv_config);

		let result = csv.read_csv();
		let error = result.expect_err("Expected error for invalid CSV data");
		assert!(
			error.to_string().contains("Failed to open CSV file"),
			"Unexpected error message: {:?}",
			error
		);
	}

	#[test]
	fn test_get_users_invalid_headers() {
		let mut config = load_config();
		let csv_content = indoc! {r#"
          first_name
          john.doe@example.com,John,Doe,+1111111111
        "#};
		let _file = test_helpers::temp_csv_file(&mut config, csv_content);

		let csv_config = config.sources.csv.expect("CsvSource configuration is missing");
		let csv = CsvSource::new(csv_config);

		let result = csv.read_csv();
		let users = result.expect("Failed to get users");
		assert_eq!(users.len(), 0, "Unexpected number of users");
	}

	#[test]
	fn test_get_users_invalid_content() {
		let mut config = load_config();
		let csv_content = indoc! {r#"
          email,first_name,last_name,phone
          john.doe@example.com
          jane.smith@example.com,Jane,Smith,+2222222222
        "#};
		let _file = test_helpers::temp_csv_file(&mut config, csv_content);

		let csv_config = config.sources.csv.expect("CsvSource configuration is missing");
		let csv = CsvSource::new(csv_config);

		let result = csv.read_csv();
		assert!(result.is_ok(), "Failed to get users: {:?}", result);

		let users = result.expect("Failed to get users");
		assert_eq!(users.len(), 1, "Unexpected number of users");
		assert_eq!(
			users[0].email,
			"jane.smith@example.com",
			"Unexpected email at index 0"
		);
		assert_eq!(
			users[0].last_name,
			"Smith",
			"Unexpected last name at index 0"
		);
	}
}
