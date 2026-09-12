//! Explicit first-authentication invitations, using the client's public token
//! API.
//!
//! A durable per-project marker is created atomically with each new non-SSO
//! user. `pending` retries definite failures; `sending` requires operator
//! reconciliation after an ambiguous outcome. Never blindly retry this
//! non-idempotent endpoint. `email:<base64 destination>` durably guards a
//! replacement invitation across interrupted email updates. It is not a
//! delivery receipt and must not be manually reset to pending before the
//! intended address has been confirmed.
use std::{sync::Arc, time::Duration};

use anyhow_ext::{Context, Result};
use base64::Engine;
use futures::TryStreamExt;
use reqwest::Method;
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use zitadel_rust_client::v2::authentication::Token;

use super::Zitadel;
use crate::FeatureFlag;

/// Lazy authenticated HTTP extension; the pinned client has no invite method.
#[derive(Debug, Clone, Default)]
pub(super) struct OnboardingClient(Arc<OnceCell<(reqwest::Client, Token)>>);

impl Zitadel<'_> {
	/// Namespace state by project so another sync cannot adopt an unfinished
	/// user.
	pub(super) fn onboarding_key(&self) -> String {
		format!("famedly-sync.onboarding.{}", self.zitadel_config.project_id)
	}

	/// Send a request without automatic retries or logging sensitive bodies.
	async fn onboarding_request(
		&self,
		method: Method,
		path: &str,
		body: Value,
	) -> Result<reqwest::Response> {
		let (http, token) = self
			.onboarding_client
			.0
			.get_or_try_init(|| async {
				let http = reqwest::Client::builder()
					.timeout(Duration::from_secs(10))
					.retry(reqwest::retry::never())
					.redirect(reqwest::redirect::Policy::none())
					.build()?;
				let token = Token::new(
					self.zitadel_config.url.clone(),
					&self.zitadel_config.key_file,
					reqwest_middleware::ClientBuilder::new(
						reqwest_middleware::reqwest::Client::builder()
							.timeout(Duration::from_secs(10))
							.build()?,
					)
					.build(),
					None,
					None,
				)
				.await?;
				Ok::<_, anyhow::Error>((http, token))
			})
			.await?;
		let response = http
			.request(method, self.zitadel_config.url.join(path)?)
			.bearer_auth(token.token().await?)
			.header("x-zitadel-orgid", &self.zitadel_config.organization_id)
			.json(&body)
			.send()
			.await?;
		Ok(response)
	}

	/// Read the durable state, distinguishing an absent key from a failed read.
	async fn onboarding_state(&self, id: &str) -> Result<Option<String>> {
		let response = self
			.onboarding_request(
				Method::GET,
				&format!("/management/v1/users/{id}/metadata/{}", self.onboarding_key()),
				Value::Null,
			)
			.await?;
		let status = response.status();
		if status == reqwest::StatusCode::NOT_FOUND {
			return Ok(None);
		}
		anyhow::ensure!(status.is_success(), "Cannot read onboarding state: HTTP {status}");
		let value: Value = response.json().await.context("Invalid onboarding API response")?;
		let value =
			value["metadata"]["value"].as_str().context("Missing onboarding metadata value")?;
		Ok(Some(String::from_utf8(base64::prelude::BASE64_STANDARD.decode(value)?)?))
	}

	/// Read a scoped human account. Never override administrative state.
	async fn onboarding_user(&self, id: &str) -> Result<Value> {
		let response =
			self.onboarding_request(Method::GET, &format!("/v2/users/{id}"), Value::Null).await?;
		anyhow::ensure!(response.status().is_success(), "Cannot read onboarding user");
		let value: Value = response.json().await.context("Invalid onboarding user response")?;
		let user = &value["user"];
		anyhow::ensure!(
			user["details"]["resourceOwner"] == self.zitadel_config.organization_id
				&& user["human"].is_object(),
			"User must be human in configured organization"
		);
		anyhow::ensure!(
			matches!(user["state"].as_str(), Some("USER_STATE_ACTIVE" | "USER_STATE_INITIAL")),
			"User must be active or initial for onboarding"
		);
		Ok(value)
	}

	/// Protobuf may omit an empty repeated field, but wrong types fail closed.
	async fn has_onboarding_authentication(&self, id: &str) -> Result<bool> {
		let response = self
			.onboarding_request(
				Method::GET,
				&format!("/v2/users/{id}/authentication_methods"),
				Value::Null,
			)
			.await?;
		anyhow::ensure!(response.status().is_success(), "Cannot read authentication methods");
		let methods: Value =
			response.json().await.context("Invalid authentication methods response")?;
		anyhow::ensure!(methods.is_object(), "Invalid authentication methods response");
		match methods.get("authMethodTypes") {
			None => Ok(false),
			Some(Value::Array(methods)) => Ok(!methods.is_empty()),
			Some(_) => anyhow::bail!("Invalid authentication methods response"),
		}
	}

	/// Record the intended destination before changing email. A failed or
	/// interrupted update cannot send to the old address on the next run.
	pub(super) async fn prepare_onboarding_email(&self, id: &str, email: &str) -> Result<()> {
		if self.feature_flags.is_enabled(FeatureFlag::SsoLogin) {
			return Ok(());
		}
		let state = self.onboarding_state(id).await?;
		match state.as_deref() {
			None => return Ok(()),
			Some("pending" | "sent") => {}
			Some(s) if s.starts_with("email:") => {}
			Some(_) => anyhow::bail!("Reconcile onboarding state before changing email for {id}"),
		}
		self.onboarding_user(id).await?;
		if self.has_onboarding_authentication(id).await? {
			return Ok(());
		}
		self.set_onboarding_state(
			id,
			&format!("email:{}", base64::prelude::BASE64_STANDARD.encode(email)),
		)
		.await
	}

	/// Persist and read back state before any non-idempotent operation.
	async fn set_onboarding_state(&self, id: &str, state: &str) -> Result<()> {
		self.zitadel_client
			.set_user_metadata(
				id,
				&self.onboarding_key(),
				state,
				Some(self.zitadel_config.organization_id.clone()),
			)
			.await?;
		anyhow::ensure!(
			self.onboarding_state(id).await?.as_deref() == Some(state),
			"Onboarding state readback mismatch"
		);
		Ok(())
	}

	/// Finish only users explicitly marked by this sync. Call even when
	/// unchanged. Runs must not overlap for the same organization/project
	/// (metadata has no CAS).
	pub async fn resume_onboarding(&self, id: &str) -> Result<()> {
		if self.feature_flags.is_enabled(FeatureFlag::DryRun)
			|| self.feature_flags.is_enabled(FeatureFlag::SsoLogin)
		{
			return Ok(());
		}
		let state = self.onboarding_state(id).await?;
		match state.as_deref() {
			None | Some("sent") => return Ok(()),
			Some("pending") => {
				self.onboarding_user(id).await?;
				// The user completed first authentication since the
				// retry was queued, e.g. through an earlier delivery or
				// an administrative action. Never re-invite, and never
				// fail the sync for them: mark the work as done.
				if self.has_onboarding_authentication(id).await? {
					return self.set_onboarding_state(id, "sent").await;
				}
			}
			Some(s) if s.starts_with("email:") => {
				let expected =
					String::from_utf8(base64::prelude::BASE64_STANDARD.decode(&s[6..])?)?;
				let user = self.onboarding_user(id).await?;
				anyhow::ensure!(
					user["user"]["human"]["email"]["email"] == expected,
					"Onboarding email update not confirmed for {id}; retry sync with intended address"
				);
				if self.has_onboarding_authentication(id).await? {
					return self.set_onboarding_state(id, "sent").await;
				}
			}
			Some("sending") => anyhow::bail!(
				"Invitation outcome ambiguous for {id}; reconcile before explicitly retrying onboarding"
			),
			Some(_) => anyhow::bail!("Unknown onboarding state for {id}"),
		}
		self.ensure_onboarding_grant(id).await?;
		self.set_onboarding_state(id, "sending").await?;
		let response = self
			.onboarding_request(
				Method::POST,
				&format!("/v2/users/{id}/invite_code"),
				json!({"sendCode":{}}),
			)
			.await?;
		let status = response.status();
		if !status.is_success() {
			// 5xx could be emitted after the event was committed: do not retry.
			if status.is_client_error() {
				self.set_onboarding_state(
					id,
					state.as_deref().context("Missing onboarding state")?,
				)
				.await?;
			}
			anyhow::bail!("CreateInviteCode failed for {id}: HTTP {status}");
		}
		self.set_onboarding_state(id, "sent").await
	}

	/// Repair grants before sending mail, including imports interrupted after
	/// creation.
	pub(super) async fn ensure_onboarding_grant(&self, id: &str) -> Result<()> {
		use zitadel_rust_client::v2::management::{
			V1UserGrantProjectIdQuery, V1UserGrantQuery, V1UserGrantUserIdQuery,
		};
		let grants: Vec<_> = self
			.zitadel_client
			.search_user_grants(
				Some(self.zitadel_config.organization_id.clone()),
				None,
				Some(vec![
					V1UserGrantQuery::ProjectId {
						project_id_query: V1UserGrantProjectIdQuery::new()
							.with_project_id(self.zitadel_config.project_id.clone()),
					},
					V1UserGrantQuery::UserId {
						user_id_query: V1UserGrantUserIdQuery::new().with_user_id(id.into()),
					},
				]),
			)?
			.try_collect()
			.await?;
		if grants.is_empty() {
			self.zitadel_client
				.add_user_grant(
					Some(self.zitadel_config.organization_id.clone()),
					id,
					self.zitadel_config.project_id.clone(),
					None,
					Some(vec![super::FAMEDLY_USER_ROLE.into()]),
				)
				.await?;
		} else {
			anyhow::ensure!(
				grants.iter().any(|g| g.role_keys().is_some_and(|roles| roles
					.iter()
					.any(|role| role == super::FAMEDLY_USER_ROLE))),
				"Existing grant lacks User role; refusing invitation"
			);
		}
		Ok(())
	}

	/// Explicitly enroll a reviewed legacy account, or reconcile an uncertain
	/// send. Never automatically backfill unmarked accounts or reset a sent
	/// invitation.
	pub async fn backfill_onboarding(&self, id: &str, retry_uncertain: bool) -> Result<()> {
		anyhow::ensure!(
			!self.feature_flags.is_enabled(FeatureFlag::SsoLogin),
			"SSO backfill is not supported"
		);
		anyhow::ensure!(
			!id.is_empty() && id.bytes().all(|c| c.is_ascii_digit()),
			"Expected exact numeric Zitadel user ID"
		);
		let response =
			self.onboarding_request(Method::GET, &format!("/v2/users/{id}"), Value::Null).await?;
		let status = response.status();
		let user: Value = response.json().await.context("Invalid onboarding API response")?;
		anyhow::ensure!(
			status.is_success()
				&& user["user"]["details"]["resourceOwner"] == self.zitadel_config.organization_id
				&& user["user"]["human"].is_object(),
			"User must be human in configured organization"
		);
		self.zitadel_client
			.get_user_metadata(id, "localpart", Some(self.zitadel_config.organization_id.clone()))
			.await?;
		let response = self
			.onboarding_request(
				Method::GET,
				&format!("/v2/users/{id}/authentication_methods"),
				Value::Null,
			)
			.await?;
		let status = response.status();
		let methods: Value = response.json().await.context("Invalid onboarding API response")?;
		anyhow::ensure!(
			status.is_success()
				&& methods.is_object()
				&& match methods.get("authMethodTypes") {
					None => true, // Protobuf omits empty repeated fields.
					Some(Value::Array(methods)) => methods.is_empty(),
					Some(_) => false,
				},
			"User already has authentication methods or lookup failed"
		);
		let state = self.onboarding_state(id).await?;
		anyhow::ensure!(
			matches!(state.as_deref(), None | Some("pending" | "sending" | "sent")),
			"Unknown onboarding state"
		);
		anyhow::ensure!(
			matches!(
				user["user"]["state"].as_str(),
				Some("USER_STATE_ACTIVE" | "USER_STATE_INITIAL")
			),
			"User must be active or initial for onboarding"
		);
		if state.as_deref() == Some("sent") {
			return Ok(());
		}
		anyhow::ensure!(
			state.as_deref() != Some("sending") || retry_uncertain,
			"Use explicit --retry-uncertain only after reconciling delivery"
		);
		let managed: Vec<_> = self.list_users_raw()?.try_collect().await?;
		anyhow::ensure!(
			managed.iter().any(|user| user.user_id().is_some_and(|user_id| user_id == id)),
			"Backfill requires existing configured project User grant"
		);
		if self.feature_flags.is_enabled(FeatureFlag::DryRun) {
			return Ok(());
		}
		self.set_onboarding_state(id, "pending").await?;
		self.resume_onboarding(id).await
	}
}
