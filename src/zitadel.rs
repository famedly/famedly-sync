//! Helper functions for submitting data to Zitadel
use futures::StreamExt;
use std::path::PathBuf;
use zitadel_rust_client::v2::users::InUserEmailsQuery;
use zitadel_rust_client::v2::users::ListUsersRequest;
use zitadel_rust_client::v2::users::SearchQuery;
use zitadel_rust_client::v2::users::SetMetadataEntry;
use zitadel_rust_client::v2::users::TypeQuery;
use zitadel_rust_client::v2::users::UpdateHumanUserRequest;
use zitadel_rust_client::v2::users::UserFieldName;
use zitadel_rust_client::v2::users::Userv2Type;

use anyhow::{anyhow, Context, Result};
use futures::Stream;
use serde::Deserialize;
use url::Url;
use uuid::{uuid, Uuid};
use zitadel_rust_client::v1::Zitadel as ZitadelClientV1;
use zitadel_rust_client::v2::{
	users::{
		AddHumanUserRequest, IdpLink, Organization, SetHumanEmail, SetHumanPhone, SetHumanProfile,
		User as ZitadelUser,
	},
	Zitadel as ZitadelClient,
};

use crate::{
	config::{Config, FeatureFlags},
	user::User,
	FeatureFlag,
};

/// The Famedly UUID namespace to use to generate v5 UUIDs.
const FAMEDLY_NAMESPACE: Uuid = uuid!("d9979cff-abee-4666-bc88-1ec45a843fb8");

/// The Zitadel project role to assign to users.
const FAMEDLY_USER_ROLE: &str = "User";

/// A very high-level Zitadel zitadel_client
#[derive(Clone)]
pub(crate) struct Zitadel {
	/// Zitadel configuration
	zitadel_config: ZitadelConfig,
	/// Optional set of features
	feature_flags: FeatureFlags,
	/// The backing Zitadel zitadel_client
	pub zitadel_client: ZitadelClient,
	/// The backing Ztiadel client, but for v1 API requests - some are
	/// still required since the v2 API doesn't cover everything
	zitadel_client_v1: ZitadelClientV1,
}

impl Zitadel {
	/// Construct the Zitadel instance
	pub(crate) async fn new(config: &Config) -> Result<Self> {
		let zitadel_client =
			ZitadelClient::new(config.zitadel.url.clone(), config.zitadel.key_file.clone())
				.await
				.context("failed to configure zitadel_client")?;

		let zitadel_client_v1 =
			ZitadelClientV1::new(config.zitadel.url.clone(), config.zitadel.key_file.clone())
				.await
				.context("failed to configure zitadel_client_v1")?;

		Ok(Self {
			zitadel_config: config.zitadel.clone(),
			feature_flags: config.feature_flags.clone(),
			zitadel_client,
			zitadel_client_v1,
		})
	}

	/// Get a list of users by their email addresses
	pub fn get_users_by_email(
		&mut self,
		emails: Vec<String>,
	) -> Result<impl Stream<Item = Result<(User, String)>> + Send> {
		self.zitadel_client
			.list_users(
				ListUsersRequest::new(vec![
					SearchQuery::new().with_type_query(TypeQuery::new(Userv2Type::Human)),
					SearchQuery::new().with_in_user_emails_query(
						InUserEmailsQuery::new().with_user_emails(emails),
					),
				])
				.with_asc(true)
				.with_sorting_column(UserFieldName::NickName),
			)
			.map(|stream| {
				stream.map(|user| {
					let id = user.user_id().ok_or(anyhow!("Missing Zitadel user ID"))?.clone();
					let user = search_result_to_user(user)?;
					Ok((user, id))
				})
			})
	}

	/// Return a stream of Zitadel users
	pub fn list_users(&mut self) -> Result<impl Stream<Item = Result<(User, String)>> + Send> {
		self.zitadel_client
			.list_users(
				ListUsersRequest::new(vec![
					SearchQuery::new().with_type_query(TypeQuery::new(Userv2Type::Human))
				])
				.with_asc(true)
				.with_sorting_column(UserFieldName::NickName),
			)
			.map(|stream| {
				stream.map(|user| {
					let id = user.user_id().ok_or(anyhow!("Missing Zitadel user ID"))?.clone();
					let user = search_result_to_user(user)?;
					Ok((user, id))
				})
			})
	}

	/// Delete a Zitadel user
	pub async fn delete_user(&mut self, zitadel_id: &str) -> Result<()> {
		tracing::info!("Deleting user with Zitadel ID: {}", zitadel_id);

		if self.feature_flags.is_enabled(FeatureFlag::DryRun) {
			tracing::warn!("Skipping deletion due to dry run");
			return Ok(());
		}

		self.zitadel_client.delete_user(zitadel_id).await.map(|_o| ())
	}

	/// Import a user into Zitadel
	pub async fn import_user(&mut self, imported_user: &User) -> Result<()> {
		tracing::info!("Importing user with external ID: {}", imported_user.external_user_id);

		if self.feature_flags.is_enabled(FeatureFlag::DryRun) {
			tracing::warn!("Skipping import due to dry run");
			return Ok(());
		}

		let mut metadata = vec![SetMetadataEntry::new(
			"localpart".to_owned(),
			Uuid::new_v5(&FAMEDLY_NAMESPACE, imported_user.external_user_id.as_bytes()).to_string(),
		)];

		if let Some(preferred_username) = imported_user.preferred_username.clone() {
			metadata.push(SetMetadataEntry::new(
				"preferred_username".to_owned(),
				preferred_username.to_string(),
			));
		}

		let mut user = AddHumanUserRequest::new(
			SetHumanProfile::new(
				imported_user.first_name.to_string(),
				imported_user.last_name.to_string(),
			)
			.with_nick_name(imported_user.external_user_id.to_string())
			.with_display_name(imported_user.get_display_name()),
			SetHumanEmail::new(imported_user.email.to_string())
				.with_is_verified(!self.feature_flags.is_enabled(FeatureFlag::VerifyEmail)),
		)
		.with_organization(
			Organization::new().with_org_id(self.zitadel_config.organization_id.clone()),
		)
		.with_metadata(metadata);

		if let Some(phone) = imported_user.phone.clone() {
			user.set_phone(
				SetHumanPhone::new()
					.with_phone(phone.to_string())
					.with_is_verified(!self.feature_flags.is_enabled(FeatureFlag::VerifyPhone)),
			);
		};

		if self.feature_flags.is_enabled(FeatureFlag::SsoLogin) {
			user.set_idp_links(vec![IdpLink::new()
				.with_user_id(imported_user.external_user_id.to_string())
				.with_idp_id(self.zitadel_config.idp_id.clone())
				// TODO: Figure out if this is the correct value; empty is not permitted
				.with_user_name(imported_user.email.to_string())]);
		}

		match self.zitadel_client.create_human_user(user.clone()).await {
			Ok(res) => {
				let id = res
					.user_id()
					.ok_or(anyhow!(
						"Failed to create user ID for external user `{}`",
						imported_user.external_user_id
					))?
					.clone();

				self.zitadel_client_v1
					.add_user_grant(
						Some(self.zitadel_config.organization_id.clone()),
						id,
						self.zitadel_config.project_id.clone(),
						None,
						vec![FAMEDLY_USER_ROLE.to_owned()],
					)
					.await?;
			}

			Err(error) => {
				// If the phone number is invalid
				if error.to_string().contains("PHONE-so0wa") {
					user.reset_phone();
					self.zitadel_client.create_human_user(user).await?;
				} else {
					anyhow::bail!(error)
				}
			}
		}

		Ok(())
	}

	/// Update a user
	pub async fn update_user(
		&mut self,
		zitadel_id: &str,
		old_user: &User,
		updated_user: &User,
	) -> Result<()> {
		tracing::info!(
			"Updating user `{}` to `{}`",
			old_user.external_user_id,
			updated_user.external_user_id
		);

		if self.feature_flags.is_enabled(FeatureFlag::DryRun) {
			tracing::warn!("Skipping update due to dry run");
			return Ok(());
		}

		let mut request = UpdateHumanUserRequest::new();

		if old_user.email != updated_user.email {
			request.set_username(updated_user.email.to_string());
			request.set_email(
				SetHumanEmail::new(updated_user.email.to_string())
					.with_is_verified(!self.feature_flags.is_enabled(FeatureFlag::VerifyEmail)),
			);
		}

		if old_user.first_name != updated_user.first_name
			|| old_user.last_name != updated_user.last_name
		{
			request.set_profile(
				SetHumanProfile::new(
					updated_user.first_name.to_string(),
					updated_user.last_name.to_string(),
				)
				.with_display_name(updated_user.get_display_name()),
			);
		}

		if old_user.phone != updated_user.phone {
			if let Some(phone) = updated_user.phone.clone() {
				request.set_phone(
					SetHumanPhone::new()
						.with_phone(phone.to_string())
						.with_is_verified(!self.feature_flags.is_enabled(FeatureFlag::VerifyPhone)),
				);
			} else {
				self.zitadel_client.remove_phone(zitadel_id).await?;
			}
		}

		if let Err(error) = self.zitadel_client.update_human_user(zitadel_id, request.clone()).await
		{
			// If the new phone number is invalid
			if error.to_string().contains("PHONE-so0wa") {
				request.reset_phone();
				self.zitadel_client.update_human_user(zitadel_id, request).await?;

				if let Err(error) = self.zitadel_client.remove_phone(zitadel_id).await {
					// If the user didn't start out with a phone
					if !error.to_string().contains("COMMAND-ieJ2e") {
						anyhow::bail!(error);
					}
				};
			} else {
				anyhow::bail!(error);
			}
		};

		if old_user.preferred_username != updated_user.preferred_username {
			if let Some(preferred_username) = updated_user.preferred_username.clone() {
				self.zitadel_client
					.set_user_metadata(
						zitadel_id,
						"preferred_username",
						&preferred_username.to_string(),
					)
					.await?;
			} else {
				self.zitadel_client.delete_user_metadata(zitadel_id, "preferred_username").await?;
			}
		}

		Ok(())
	}
}

/// Convert a Zitadel search result to a user
fn search_result_to_user(user: ZitadelUser) -> Result<User> {
	let human_user = user.human().ok_or(anyhow!("Machine user found in human user search"))?;
	let nick_name = human_user
		.profile()
		.and_then(|p| p.nick_name())
		.ok_or(anyhow!("Missing external ID found for user"))?;

	// TODO: If async closures become a reality, we
	// should capture the correct preferred_username
	// here.
	let user = User::try_from_zitadel_user(human_user.clone(), nick_name.clone())?;
	Ok(user)
}

/// Configuration related to Famedly Zitadel
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ZitadelConfig {
	/// The URL for Famedly Zitadel authentication
	pub url: Url,
	/// File containing a private key for authentication to Famedly Zitadel
	pub key_file: PathBuf,
	/// Organization ID provided by Famedly Zitadel
	pub organization_id: String,
	/// Project ID provided by Famedly Zitadel
	pub project_id: String,
	/// IDP ID provided by Famedly Zitadel
	pub idp_id: String,
}
