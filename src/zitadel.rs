//! Helper functions for submitting data to Zitadel
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use url::Url;
use uuid::{uuid, Uuid};
use zitadel_rust_client::v1::{
	error::{Error as ZitadelError, TonicErrorCode},
	Zitadel as ZitadelClient,
};

use crate::{
	config::{Config, FeatureFlags},
	user::{StringOrBytes, User, ZitadelUser},
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
	zitadel_client: ZitadelClient,
}

impl Zitadel {
	/// Construct the Zitadel instance
	pub(crate) async fn new(config: &Config) -> Result<Self> {
		let zitadel_client =
			ZitadelClient::new(config.zitadel.url.clone(), config.zitadel.key_file.clone())
				.await
				.context("failed to configure zitadel_client")?;

		Ok(Self {
			zitadel_config: config.zitadel.clone(),
			feature_flags: config.feature_flags.clone(),
			zitadel_client,
		})
	}

	/// Import a list of new users into Zitadel
	pub(crate) async fn import_new_users(&self, users: Vec<User>) -> Result<()> {
		for user in users {
			let zitadel_user =
				user.to_zitadel_user(&self.feature_flags, &self.zitadel_config.idp_id);
			let status = self.import_user(&zitadel_user).await;

			if let Err(error) = status {
				tracing::error!(
					"Failed to sync-import user `{}`: {:?}",
					zitadel_user.log_name(),
					error
				);

				if Self::is_invalid_phone_error(error) {
					let zitadel_user = ZitadelUser {
						user_data: User { phone: None, ..zitadel_user.user_data },
						..zitadel_user
					};

					let retry_status = self.import_user(&zitadel_user).await;

					match retry_status {
						Ok(_) => {
							tracing::info!(
								"Retry sync-import succeeded for user `{}`",
								zitadel_user.log_name()
							);
						}
						Err(retry_error) => {
							tracing::error!(
								"Retry sync-import failed for user `{}`: {:?}",
								zitadel_user.log_name(),
								retry_error
							);
						}
					}
				}
			}
		}

		Ok(())
	}

	/// Delete a list of Zitadel users given their IDs
	pub(crate) async fn delete_users_by_id(&self, users: Vec<UserId>) -> Result<()> {
		for user_id in users {
			match user_id {
				UserId::Login(login) => {
					let status = self.delete_user_by_email(&login).await;
					if let Err(error) = status {
						tracing::error!("Failed to delete user by email `{}`: {:?}", login, error);
					}
				}
				UserId::Nick(nick) => {
					let status = self.delete_user_by_nick(&nick).await;
					if let Err(error) = status {
						tracing::error!("Failed to delete user by nick `{}`: {:?}", nick, error);
					}
				}
				UserId::ZitadelId(id) => {
					let status = self.delete_user_by_id(&id).await;
					if let Err(error) = status {
						tracing::error!("Failed to delete user by id `{}`: {:?}", id, error);
					}
				}
			}
		}

		Ok(())
	}

	/// Update a list of old/new user maps
	pub(crate) async fn update_users(&self, users: Vec<ChangedUser>) -> Result<()> {
		let disabled: Vec<ZitadelUser> = users
			.iter()
			.filter(|user| user.old.enabled && !user.new.enabled)
			.map(|user| {
				user.new.to_zitadel_user(&self.feature_flags, &self.zitadel_config.idp_id).clone()
			})
			.collect();

		let enabled: Vec<ZitadelUser> = users
			.iter()
			.filter(|user| !user.old.enabled && user.new.enabled)
			.map(|user| {
				user.new.to_zitadel_user(&self.feature_flags, &self.zitadel_config.idp_id).clone()
			})
			.collect();

		let changed: Vec<(ZitadelUser, ZitadelUser)> = users
			.into_iter()
			.filter(|user| user.new.enabled && user.old.enabled == user.new.enabled)
			.map(|user| {
				(
					user.old
						.to_zitadel_user(&self.feature_flags, &self.zitadel_config.idp_id)
						.clone(),
					user.new
						.to_zitadel_user(&self.feature_flags, &self.zitadel_config.idp_id)
						.clone(),
				)
			})
			.collect();

		for user in disabled {
			let status = self.delete_user(&user).await;

			if let Err(error) = status {
				tracing::error!("Failed to delete user `{}`: {:?}`", user.log_name(), error);
			}
		}

		if !self.feature_flags.is_enabled(FeatureFlag::DeactivateOnly) {
			for user in enabled {
				let status = self.import_user(&user).await;

				if let Err(error) = status {
					tracing::error!("Failed to re-create user `{}`: {:?}", user.log_name(), error);
				}
			}

			for (old, new) in changed {
				let status = self.update_user(&old, &new).await;

				if let Err(error) = status {
					tracing::error!("Failed to sync-update user `{}`: {:?}", new.log_name(), error);

					if Self::is_invalid_phone_error(error) {
						let new =
							ZitadelUser { user_data: User { phone: None, ..new.user_data }, ..new };

						let retry_status = self.update_user(&old, &new).await;

						match retry_status {
							Ok(_) => {
								tracing::info!(
									"Retry sync-update succeeded for user `{}`",
									new.log_name()
								);
							}
							Err(retry_error) => {
								tracing::error!(
									"Retry sync-update failed for user `{}`: {:?}",
									new.log_name(),
									retry_error
								);
							}
						}
					}
				}
			}
		}

		Ok(())
	}

	/// Update a Zitadel user
	async fn update_user(&self, old: &ZitadelUser, new: &ZitadelUser) -> Result<()> {
		if self.feature_flags.is_enabled(FeatureFlag::DryRun) {
			tracing::info!("Not updating user due to dry run: {:?} -> {:?}", old, new);
			return Ok(());
		}

		let Some(user_id) = self.get_user_id(old).await? else {
			bail!("could not find user `{}` to update", old.user_data.email);
		};

		if old.user_data.email != new.user_data.email {
			self.zitadel_client
				.update_human_user_name(
					&self.zitadel_config.organization_id,
					user_id.clone(),
					new.user_data.email.clone().to_string(),
				)
				.await?;

			tracing::warn!(
				"User email/login changed for {} -> {}",
				old.user_data.email,
				new.user_data.email
			);
		};

		if old.user_data.first_name != new.user_data.first_name
			|| old.user_data.last_name != new.user_data.last_name
		{
			self.zitadel_client
				.update_human_user_profile(
					&self.zitadel_config.organization_id,
					user_id.clone(),
					new.user_data.first_name.clone().to_string(),
					new.user_data.last_name.clone().to_string(),
					None,
					Some(new.get_display_name()),
					None,
					None,
				)
				.await?;
		};

		match (&old.user_data.phone, &new.user_data.phone) {
			(Some(_), None) => {
				self.zitadel_client
					.remove_human_user_phone(&self.zitadel_config.organization_id, user_id.clone())
					.await?;
			}
			(_, Some(new_phone)) => {
				self.zitadel_client
					.update_human_user_phone(
						&self.zitadel_config.organization_id,
						user_id.clone(),
						new_phone.clone().to_string(),
						!self.feature_flags.is_enabled(FeatureFlag::VerifyPhone),
					)
					.await?;
			}
			(None, None) => {}
		};

		if old.user_data.email != new.user_data.email {
			self.zitadel_client
				.update_human_user_email(
					&self.zitadel_config.organization_id,
					user_id.clone(),
					new.user_data.email.clone().to_string(),
					!self.feature_flags.is_enabled(FeatureFlag::VerifyEmail),
				)
				.await?;
		};

		if old.user_data.preferred_username != new.user_data.preferred_username {
			self.zitadel_client
				.set_user_metadata(
					Some(&self.zitadel_config.organization_id),
					user_id,
					"preferred_username".to_owned(),
					&new.user_data.preferred_username.clone().to_string(),
				)
				.await?;
		};

		tracing::info!("Successfully updated user {}", old.user_data.email);

		Ok(())
	}

	/// Delete a Zitadel user given only their LDAP id
	async fn delete_user_by_id(&self, user_id: &str) -> Result<()> {
		if self.feature_flags.is_enabled(FeatureFlag::DryRun) {
			tracing::info!("Not deleting user `{}` due to dry run", user_id);
			return Ok(());
		}

		let user = self
			.zitadel_client
			.get_user_by_nick_name(
				Some(self.zitadel_config.organization_id.clone()),
				user_id.to_owned(),
			)
			.await?;
		match user {
			Some(user) => self.zitadel_client.remove_user(user.id).await?,
			None => bail!("Could not find user with ldap uid '{user_id}' for deletion"),
		}

		tracing::info!("Successfully deleted user {}", user_id);

		Ok(())
	}

	/// Delete a Zitadel user given only their email address used as login name
	async fn delete_user_by_email(&self, email: &str) -> Result<()> {
		if self.feature_flags.is_enabled(FeatureFlag::DryRun) {
			tracing::info!("Not deleting user `{}` due to dry run", email);
			return Ok(());
		}

		let user = self.zitadel_client.get_user_by_login_name(email).await?;
		match user {
			Some(user) => self.zitadel_client.remove_user(user.id).await?,
			None => tracing::info!("Could not find user with email '{email}' for deletion"),
		}

		tracing::info!("Successfully deleted user {}", email);

		Ok(())
	}

	/// Delete a Zitadel user given only their nick name
	async fn delete_user_by_nick(&self, nick: &str) -> Result<()> {
		if self.feature_flags.is_enabled(FeatureFlag::DryRun) {
			tracing::info!("Not deleting user `{}` due to dry run", nick);
			return Ok(());
		}

		let user = self
			.zitadel_client
			.get_user_by_nick_name(
				Some(self.zitadel_config.organization_id.clone()),
				nick.to_owned(),
			)
			.await?;
		match user {
			Some(user) => self.zitadel_client.remove_user(user.id).await?,
			None => tracing::info!("Could not find user with nick '{nick}' for deletion"),
		}

		tracing::info!("Successfully deleted user {}", nick);

		Ok(())
	}

	/// Retrieve the Zitadel user ID of a user, or None if the user
	/// cannot be found
	async fn get_user_id(&self, user: &ZitadelUser) -> Result<Option<String>> {
		let status = self
			.zitadel_client
			.get_user_by_login_name(&user.user_data.email.clone().to_string())
			.await;

		if let Err(ZitadelError::TonicResponseError(ref error)) = status {
			if error.code() == TonicErrorCode::NotFound {
				return Ok(None);
			}
		}

		Ok(status.map(|user| user.map(|u| u.id))?)
	}

	/// Delete a Zitadel user
	async fn delete_user(&self, user: &ZitadelUser) -> Result<()> {
		if self.feature_flags.is_enabled(FeatureFlag::DryRun) {
			tracing::info!("Not deleting user due to dry run: {:?}", user);
			return Ok(());
		}

		if let Some(user_id) = self.get_user_id(user).await? {
			self.zitadel_client.remove_user(user_id).await?;
		} else {
			bail!("could not find user `{}` for deletion", user.user_data.email);
		}

		tracing::info!("Successfully deleted user {}", user.user_data.email);

		Ok(())
	}

	/// Import a user into Zitadel
	async fn import_user(&self, user: &ZitadelUser) -> Result<()> {
		if self.feature_flags.is_enabled(FeatureFlag::DryRun) {
			tracing::info!("Not importing user due to dry run: {:?}", user);
			return Ok(());
		}

		if !user.user_data.enabled {
			tracing::info!("Not importing disabled user: {:?}", user);
			return Ok(());
		}

		let id = match &user.user_data.external_user_id {
			StringOrBytes::String(value) => value.as_bytes(),
			StringOrBytes::Bytes(value) => value,
		};

		let uuid;
		let localpart = if self.feature_flags.contains(&FeatureFlag::PlainLocalpart) {
			match &user.user_data.external_user_id {
				StringOrBytes::String(value) => value,
				StringOrBytes::Bytes(_) => {
					bail!(
						"Unsupported binary external ID for user using plain localparts: {:?}",
						user
					);
				}
			}
		} else {
			uuid = Uuid::new_v5(&FAMEDLY_NAMESPACE, id).to_string();
			&uuid
		};

		let new_user_id = self
			.zitadel_client
			.create_human_user(&self.zitadel_config.organization_id, user.clone().into())
			.await?;

		self.zitadel_client
			.set_user_metadata(
				Some(&self.zitadel_config.organization_id),
				new_user_id.clone(),
				"preferred_username".to_owned(),
				&user.user_data.preferred_username.clone().to_string(),
			)
			.await?;

		self.zitadel_client
			.set_user_metadata(
				Some(&self.zitadel_config.organization_id),
				new_user_id.clone(),
				"localpart".to_owned(),
				localpart,
			)
			.await?;

		self.zitadel_client
			.add_user_grant(
				Some(self.zitadel_config.organization_id.clone()),
				new_user_id,
				self.zitadel_config.project_id.clone(),
				None,
				vec![FAMEDLY_USER_ROLE.to_owned()],
			)
			.await?;

		tracing::info!("Successfully imported user {:?}", user);

		Ok(())
	}

	/// Check if an error is an invalid phone error
	fn is_invalid_phone_error(error: anyhow::Error) -> bool {
		/// Part of the error message returned by Zitadel
		/// when a phone number is invalid for a new user
		const INVALID_PHONE_IMPORT_ERROR: &str = "invalid ImportHumanUserRequest_Phone";

		/// Part of the error message returned by Zitadel
		/// when a phone number is invalid for an existing user being updated
		const INVALID_PHONE_UPDATE_ERROR: &str = "invalid UpdateHumanPhoneRequest";

		if let Ok(ZitadelError::TonicResponseError(ref error)) = error.downcast::<ZitadelError>() {
			return error.code() == TonicErrorCode::InvalidArgument
				&& (error.message().contains(INVALID_PHONE_IMPORT_ERROR)
					|| error.message().contains(INVALID_PHONE_UPDATE_ERROR));
		}

		false
	}
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

/// The different ways to identify a user in Zitadel
#[derive(Debug)]
pub enum UserId {
	/// The login name is actually the email address
	Login(String),
	/// The nick name is actually the LDAP ID
	Nick(String),
	/// The Zitadel ID
	#[allow(dead_code)]
	ZitadelId(String),
}

/// The difference between the source and Zitadel
#[derive(Debug)]
pub struct SourceDiff {
	/// New users
	pub new_users: Vec<User>,
	/// Changed users
	pub changed_users: Vec<ChangedUser>,
	/// Deleted user IDs
	pub deleted_user_ids: Vec<UserId>,
}

/// A user that has changed returned from the LDAP poller
#[derive(Debug)]
pub struct ChangedUser {
	/// The old state
	pub old: User,
	/// The new state
	pub new: User,
}
