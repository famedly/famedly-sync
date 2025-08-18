//! UKT source for syncing with Famedly's Zitadel.

use std::collections::HashMap;

use anyhow_ext::{Context, Result};
use chrono::Utc;
use reqwest::Client;
use serde::Deserialize;
use url::Url;

/// UKT Source
pub struct UktSource {
	/// UKT Source configuration
	ukt_config: UktSourceConfig,
	/// Reqwest client
	client: Client,
}

#[anyhow_trace::anyhow_trace]
impl UktSource {
	/// Create a new UKT source
	pub fn new(ukt_config: UktSourceConfig) -> Self {
		let client = Client::new();

		Self { ukt_config, client }
	}

	/// Get list of user emails that have been removed
	pub async fn get_removed_user_emails(&self) -> Result<Vec<String>> {
		let oauth2_token = self.get_oauth2_token().await?;
		let email_list = self.fetch_list(oauth2_token).await?;

		Ok(email_list)
	}

	/// Get the OAuth2 token
	async fn get_oauth2_token(&self) -> Result<OAuth2Token> {
		let mut params = HashMap::new();
		params.insert("grant_type", &self.ukt_config.grant_type);
		params.insert("scope", &self.ukt_config.scope);
		params.insert("client_id", &self.ukt_config.client_id);
		params.insert("client_secret", &self.ukt_config.client_secret);

		let response =
			self.client.post(self.ukt_config.oauth2_url.clone()).form(&params).send().await?;

		response.error_for_status_ref().context("UKT oAuth2 received non-OK status code")?;

		let response: serde_json::Value = response.json().await?;

		if let Some(error) = response.get("error") {
			anyhow::bail!("Error in UKT oAuth2 response body: {}", error)
		}

		let oauth2_token: OAuth2Token = serde_json::from_value(response)
			.context("Failed to deserialize oAuth2 token response")?;

		Ok(oauth2_token)
	}

	/// Fetch the list of users
	async fn fetch_list(&self, oauth2_token: OAuth2Token) -> Result<EmailList> {
		let current_date = Utc::now().format("%Y%m%d").to_string();

		let response = self
			.client
			.get(self.ukt_config.endpoint_url.clone())
			.query(&[("date", &current_date)])
			.bearer_auth(oauth2_token.access_token)
			.send()
			.await?;

		response.error_for_status_ref().context("UKT endpoint received non-OK status code")?;

		let response: serde_json::Value = response.json().await?;

		if let Some(error) = response.get("error") {
			anyhow::bail!("Error in UKT endpoint response body: {}", error)
		}

		let email_list: EmailList = serde_json::from_value(response)
			.context("Failed to deserialize email list response")?;

		Ok(email_list)
	}
}

/// List of emails
type EmailList = Vec<String>;

/// OAuth2 token response
#[derive(Debug, Deserialize)]
struct OAuth2Token {
	/// Access token
	access_token: String,
}

/// Configuration to get a list of users from UKT
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct UktSourceConfig {
	/// The URL of the endpoint provided by UKT
	pub endpoint_url: Url,
	/// The OAuth2 URL
	pub oauth2_url: Url,
	/// The API client ID
	pub client_id: String,
	/// The API client secret
	pub client_secret: String,
	/// The scope
	pub scope: String,
	/// The grant type
	pub grant_type: String,
}

/// Helper module for unit and e2e tests
pub mod test_helpers {

	use http::StatusCode;
	use url::Url;
	use wiremock::{
		Mock, MockServer, ResponseTemplate,
		matchers::{body_string_contains, header, method, path, query_param},
	};

	use super::*;

	/// The path to the UKT maillist endpoint
	pub const ENDPOINT_PATH: &str = "/usersync4chat/maillist";

	/// The path to the UKT OAuth2 endpoint
	pub const OAUTH2_PATH: &str = "/token";

	/// Get the URL of the mock server with the given path
	pub fn get_mock_server_url(mock_server: &MockServer, path: &str) -> Result<Url> {
		let url_with_endpoint = format!("{}{}", mock_server.uri(), path);
		Url::parse(&url_with_endpoint)
			.map_err(|error| anyhow::anyhow!("Failed to parse URL: {}", error))
	}

	/// Prepare the OAuth2 mock
	pub async fn prepare_oauth2_mock(mock_server: &MockServer) {
		Mock::given(method("POST"))
			.and(path(OAUTH2_PATH))
			.and(body_string_contains("grant_type=client_credentials"))
			.and(body_string_contains("scope=openid+read-maillist"))
			.and(body_string_contains("client_id=mock_client_id"))
			.and(body_string_contains("client_secret=mock_client_secret"))
			.respond_with(ResponseTemplate::new(StatusCode::OK).set_body_string(
				r#"{
              "access_token": "mock_access_token",
              "token_type": "Bearer",
              "scope": "openid read-maillist",
              "expires_in": 3600
          }"#,
			))
			.up_to_n_times(1)
			.mount(mock_server)
			.await;
	}

	/// Prepare the endpoint mock
	pub async fn prepare_endpoint_mock(mock_server: &MockServer, email_to_delete: &str) {
		let current_date = Utc::now().format("%Y%m%d").to_string();

		Mock::given(method("GET"))
			.and(path(ENDPOINT_PATH))
			.and(query_param("date", &current_date))
			.and(header("Authorization", "Bearer mock_access_token"))
			.respond_with(ResponseTemplate::new(StatusCode::OK).set_body_string(format!(
				r#"[
              "{email_to_delete}"
          ]"#
			)))
			.up_to_n_times(1)
			.mount(mock_server)
			.await;
	}
}

#[cfg(test)]
mod tests {
	use indoc::indoc;
	use wiremock::MockServer;

	use super::*;
	use crate::Config;

	const EXAMPLE_CONFIG: &str = indoc! {r#"
        zitadel:
          url: http://localhost:8080
          key_file: tests/environment/zitadel/service-user.json
          organization_id: 1
          project_id: 1

        sources:
          ukt:
            endpoint_url: https://api-test.medizin.uni-tuebingen.de/usersync4chat/maillist
            oauth2_url: https://auth-test.medizin.uni-tuebingen.de/oauth2/token
            client_id: mock_client_id
            client_secret: mock_client_secret
            scope: "openid read-maillist"
            grant_type: client_credentials

        feature_flags: []
	"#};

	fn load_config() -> Config {
		serde_yaml::from_str(EXAMPLE_CONFIG).expect("invalid config")
	}

	#[tokio::test]
	async fn test_get_oauth2_token() {
		let mock_server = MockServer::start().await;
		test_helpers::prepare_oauth2_mock(&mock_server).await;

		// Use the mock server URL in the config
		let mut config = load_config();
		config
			.sources
			.ukt
			.as_mut()
			.map(|ukt| {
				ukt.oauth2_url =
					test_helpers::get_mock_server_url(&mock_server, test_helpers::OAUTH2_PATH)
						.expect("Failed to get mock server URL");
			})
			.expect("UktSource configuration is missing");

		let ukt_config = config.sources.ukt.expect("UktSource configuration is missing");

		let ukt = UktSource::new(ukt_config);

		let result = ukt.get_oauth2_token().await;
		assert!(result.is_ok(), "Failed to get OAuth2 token: {result:?}");
	}

	#[tokio::test]
	async fn test_fetch_list() {
		let mock_server = MockServer::start().await;
		test_helpers::prepare_oauth2_mock(&mock_server).await;
		test_helpers::prepare_endpoint_mock(&mock_server, "delete@famedly.de").await;

		let mut config = load_config();
		config
			.sources
			.ukt
			.as_mut()
			.map(|ukt| {
				ukt.oauth2_url =
					test_helpers::get_mock_server_url(&mock_server, test_helpers::OAUTH2_PATH)
						.expect("Failed to get mock server URL");
				ukt.endpoint_url =
					test_helpers::get_mock_server_url(&mock_server, test_helpers::ENDPOINT_PATH)
						.expect("Failed to get mock server URL");
			})
			.expect("UktSource configuration is missing");

		let ukt_config = config.sources.ukt.expect("UktSource configuration is missing");

		let ukt = UktSource::new(ukt_config);

		let oauth2_token = ukt.get_oauth2_token().await.expect("Failed to get access token");

		let result = ukt.fetch_list(oauth2_token).await;
		assert!(result.is_ok(), "Failed to fetch email list: {result:?}");

		let email_list = result.expect("Failed to get email list");
		assert_eq!(email_list.len(), 1, "Unexpected number of emails");
		assert_eq!(email_list[0], "delete@famedly.de", "Unexpected email at index 0");
	}

	#[tokio::test]
	async fn test_fetch_list_incorrect_verification() {
		let mock_server = MockServer::start().await;
		test_helpers::prepare_endpoint_mock(&mock_server, "delete@famedly.de").await;

		let mut config = load_config();
		config
			.sources
			.ukt
			.as_mut()
			.map(|ukt| {
				ukt.endpoint_url =
					test_helpers::get_mock_server_url(&mock_server, test_helpers::ENDPOINT_PATH)
						.expect("Failed to get mock server URL");
			})
			.expect("UktSource configuration is missing");

		let ukt_config = config.sources.ukt.expect("UktSource configuration is missing");

		let ukt = UktSource::new(ukt_config);

		let incorrect_oauth2_token = OAuth2Token { access_token: "wrong_token".to_owned() };

		let result = ukt.fetch_list(incorrect_oauth2_token).await;
		assert!(result.is_err(), "Didn't expect to fetch email list: {result:?}");
	}
}
