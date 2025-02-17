//! Helper functions for submitting data to Zitadel
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use base64::prelude::{Engine, BASE64_STANDARD};
use futures::{Stream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use url::Url;
use zitadel_rust_client::{
	v1::Zitadel as ZitadelClientV1,
	v2::{
		management::{
			Userv1Type, V1UserGrantProjectIdQuery, V1UserGrantQuery, V1UserGrantRoleKeyQuery,
			V1UserGrantUserTypeQuery, Zitadeluserv1UserGrant,
		},
		users::{
			AddHumanUserRequest, AndQuery, IdpLink, InUserEmailsQuery, ListUsersRequest,
			Organization, OrganizationIdQuery, SearchQuery, SetHumanEmail, SetHumanPhone,
			SetHumanProfile, SetMetadataEntry, TypeQuery, UpdateHumanUserRequest,
			User as ZitadelUser, UserFieldName, Userv2Type,
		},
		Zitadel as ZitadelClient,
	},
};

use crate::{
	config::{Config, FeatureFlags},
	get_next_zitadel_user,
	user::User,
	FeatureFlag,
};

/// The Zitadel project role to assign to users.
const FAMEDLY_USER_ROLE: &str = "User";

/// The number of users to sample for encoding detection
const USER_SAMPLE_SIZE: usize = 50;

/// A very high-level Zitadel zitadel_client
#[derive(Clone, Debug)]
pub struct Zitadel {
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
	pub async fn new(config: &Config) -> Result<Self> {
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
	) -> Result<impl Stream<Item = Result<(ZitadelUserBuilder, String)>> + Send> {
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
	pub fn list_all_users(
		&mut self,
	) -> Result<impl Stream<Item = Result<(ZitadelUserBuilder, String)>> + Send> {
		self.zitadel_client
			.list_users(
				ListUsersRequest::new(vec![SearchQuery::new().with_and_query(
					AndQuery::new().with_queries(vec![
						SearchQuery::new().with_type_query(TypeQuery::new(Userv2Type::Human)),
						SearchQuery::new().with_organization_id_query(OrganizationIdQuery::new(
							self.zitadel_config.organization_id.clone(),
						)),
					]),
				)])
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

	/// Uses "Search User Grants" method to fetch users. Since it doesn't
	/// support sorting, we read all the users into the memory and sort them,
	/// so this function is computation intensive. TODO: make an issue to
	/// zitadel to support sorting
	pub async fn list_users_with_project_id_filtering(
		&mut self,
	) -> Result<impl Stream<Item = Result<(ZitadelUserBuilder, String)>> + Send> {
		let all_users = self
			.zitadel_client
			.search_user_grants(
				Some(self.zitadel_config.organization_id.clone()),
				Some(vec![
					V1UserGrantQuery::ProjectId {
						project_id_query: (V1UserGrantProjectIdQuery::new()
							.with_project_id(self.zitadel_config.project_id.clone())),
					},
					V1UserGrantQuery::UserType {
						user_type_query: V1UserGrantUserTypeQuery::new()
							.with__type(Userv1Type::Human),
					},
					V1UserGrantQuery::RoleKey {
						role_key_query: V1UserGrantRoleKeyQuery::new().with_role_key("User".into()),
					},
				]),
			)?
			.map(|user| {
				let id = user.user_id().context("Missing Zitadel user ID")?.clone();
				let user = ZitadelUserBuilder::try_from(user)
					.map_err(|f| anyhow!("Missing {f} in Zitadel user"))?;
				Ok::<_, anyhow::Error>((user.external_user_id.clone(), (user, id)))
			})
			.try_collect::<std::collections::BTreeMap<String, _>>()
			.await?;
		Ok(futures::stream::iter(all_users.into_values().map(Ok)))
	}

	/// Wrapper over two different methods to fetch users depending on the
	/// configuration.
	pub async fn list_users(
		&mut self,
	) -> Result<Box<dyn Stream<Item = Result<(ZitadelUserBuilder, String)>> + Send + Unpin>> {
		Ok(if self.zitadel_config.filter_by_project_id {
			Box::new(self.list_users_with_project_id_filtering().await?)
		} else {
			Box::new(self.list_all_users()?)
		})
	}

	/// Return a vector of a random sample of Zitadel users
	/// We use this to determine the encoding of the external IDs
	pub async fn get_users_sample(&mut self) -> Result<Vec<User>> {
		let mut stream = self
			.zitadel_client
			.list_users(
				ListUsersRequest::new(vec![
					SearchQuery::new().with_type_query(TypeQuery::new(Userv2Type::Human))
				])
				.with_asc(true)
				.with_sorting_column(UserFieldName::NickName)
				.with_page_size(USER_SAMPLE_SIZE),
			)
			.map(|stream| {
				stream.map(|user| {
					let id = user.user_id().ok_or(anyhow!("Missing Zitadel user ID"))?.clone();
					let user = search_result_to_user(user)?;
					Ok((user, id))
				})
			})?;

		let mut users = Vec::new();

		while let Some(user) = get_next_zitadel_user(&mut stream, self).await? {
			users.push(user.0);
		}

		Ok(users)
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

		let mut metadata =
			vec![SetMetadataEntry::new("localpart".to_owned(), imported_user.localpart.clone())];
		metadata.push(SetMetadataEntry::new(
			"preferred_username".to_owned(),
			imported_user.preferred_username.clone(),
		));

		let mut user = AddHumanUserRequest::new(
			SetHumanProfile::new(imported_user.first_name.clone(), imported_user.last_name.clone())
				.with_nick_name(imported_user.external_user_id.clone())
				.with_display_name(imported_user.get_display_name()),
			SetHumanEmail::new(imported_user.email.clone())
				.with_is_verified(!self.feature_flags.is_enabled(FeatureFlag::VerifyEmail)),
		)
		.with_organization(
			Organization::new().with_org_id(self.zitadel_config.organization_id.clone()),
		)
		.with_metadata(metadata)
		.with_user_id(imported_user.localpart.clone()); // Set the Zitadel userId to the localpart

		if let Some(phone) = imported_user.phone.clone() {
			user.set_phone(
				SetHumanPhone::new()
					.with_phone(phone.clone())
					.with_is_verified(!self.feature_flags.is_enabled(FeatureFlag::VerifyPhone)),
			);
		};

		if self.feature_flags.is_enabled(FeatureFlag::SsoLogin) {
			user.set_idp_links(vec![IdpLink::new()
				.with_user_id(get_zitadel_encoded_id(imported_user.get_external_id_bytes()?))
				.with_idp_id(self.zitadel_config.idp_id.clone())
				.with_user_name(imported_user.email.clone())]);
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

		// Check if localpart has changed and emit warning if it has
		if old_user.localpart != updated_user.localpart {
			tracing::warn!(
				"Refusing to change user localparts for user {} from {} to {}",
				old_user.external_user_id,
				old_user.localpart,
				updated_user.localpart
			);
		}

		if self.feature_flags.is_enabled(FeatureFlag::DryRun) {
			tracing::warn!("Skipping update due to dry run");
			return Ok(());
		}

		let mut request = UpdateHumanUserRequest::new();

		if old_user.email != updated_user.email {
			request.set_username(updated_user.email.clone());
			request.set_email(
				SetHumanEmail::new(updated_user.email.clone())
					.with_is_verified(!self.feature_flags.is_enabled(FeatureFlag::VerifyEmail)),
			);
		}

		if old_user.first_name != updated_user.first_name
			|| old_user.last_name != updated_user.last_name
			|| old_user.external_user_id != updated_user.external_user_id
		{
			request.set_profile(
				SetHumanProfile::new(
					updated_user.first_name.clone(),
					updated_user.last_name.clone(),
				)
				.with_display_name(updated_user.get_display_name())
				.with_nick_name(updated_user.external_user_id.clone()),
			);
		}

		if old_user.phone != updated_user.phone {
			if let Some(phone) = updated_user.phone.clone() {
				request.set_phone(
					SetHumanPhone::new()
						.with_phone(phone.clone())
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
			self.zitadel_client
				.set_user_metadata(
					zitadel_id,
					"preferred_username",
					&updated_user.preferred_username.clone(),
				)
				.await?;
		}

		Ok(())
	}
}

/// Convert a Zitadel search result to a user
pub fn search_result_to_user(user: ZitadelUser) -> Result<ZitadelUserBuilder> {
	let human_user = user.human().ok_or(anyhow!("Machine user found in human user search"))?;
	let external_id = human_user
		.profile()
		.and_then(|p| p.nick_name())
		.ok_or(anyhow!("Missing external ID found for user"))?;

	let first_name = human_user
		.profile()
		.and_then(|profile| profile.given_name())
		.ok_or(anyhow!("Missing first name for {}", external_id))?
		.clone();

	let last_name = human_user
		.profile()
		.and_then(|profile| profile.family_name())
		.ok_or(anyhow!("Missing last name for {}", external_id))?
		.clone();

	let email = human_user
		.email()
		.and_then(|human_email| human_email.email())
		.ok_or(anyhow!("Missing email address for {}", external_id))?
		.clone();

	let phone = human_user.phone().and_then(|human_phone| human_phone.phone());

	// TODO: If async closures become a reality, we
	// should capture the correct preferred_username and localpart from metadata
	// here.
	let user = ZitadelUserBuilder::new(
		first_name,
		last_name,
		email,
		external_id.to_owned(),
		phone.cloned(),
	);
	Ok(user)
}

impl TryFrom<Zitadeluserv1UserGrant> for ZitadelUserBuilder {
	type Error = &'static str;
	fn try_from(user: Zitadeluserv1UserGrant) -> Result<Self, Self::Error> {
		Ok(ZitadelUserBuilder::new(
			user.first_name().ok_or("first_name")?.clone(),
			user.last_name().ok_or("last_name")?.clone(),
			user.email().ok_or("email")?.clone(),
			user.user_name().ok_or("user_name")?.clone(),
			None, // TODO: think of something
		))
	}
}

/// A builder for a `User` to be used for users gathered from Zitadel
#[derive(Debug)]
pub struct ZitadelUserBuilder {
	/// The user's first name
	first_name: String,
	/// The user's last name
	last_name: String,
	/// The user's email address
	email: String,
	/// The user's external ID
	external_user_id: String,

	/// The user's preferred username - must be set before building
	preferred_username: Option<String>,
	/// The user's localpart - must be set before building
	localpart: Option<String>,

	/// The user's phone number
	phone: Option<String>,
}

impl ZitadelUserBuilder {
	/// Basic constructor
	#[must_use]
	pub fn new(
		first_name: String,
		last_name: String,
		email: String,
		external_user_id: String,
		phone: Option<String>,
	) -> Self {
		Self {
			first_name,
			last_name,
			email,
			external_user_id,
			phone,

			preferred_username: None,
			localpart: None,
		}
	}

	/// Set the user's localpart
	#[must_use]
	pub fn with_localpart(mut self, localpart: String) -> Self {
		self.localpart = Some(localpart);
		self
	}

	/// Set the user's preferred username
	#[must_use]
	pub fn with_preferred_username(mut self, preferred_username: String) -> Self {
		self.preferred_username = Some(preferred_username);
		self
	}

	/// Build the resulting user struct - this will return an `Err`
	/// variant if the localpart or preferred username are missing
	pub fn build(self) -> Result<User> {
		let Some(localpart) = self.localpart else {
			bail!("No valid localpart set");
		};

		let Some(preferred_username) = self.preferred_username else {
			bail!("No valid preferred username set");
		};

		Ok(User {
			first_name: self.first_name,
			last_name: self.last_name,
			email: self.email,
			phone: self.phone,
			enabled: true,
			external_user_id: self.external_user_id,

			localpart,
			preferred_username,
		})
	}
}

/// Get a base64-encoded external user ID, if the ID is raw bytes,
/// or a UTF-8 string if not.
///
/// Note: This encoding scheme is inherently broken, because it is
/// impossible to tell apart base64 encoded strings from
/// non-base64 encoded strings. We can therefore never know if the
/// ID should be decoded or not when re-parsing it, and it may
/// create collisions (although this is unlikely).
///
/// Only use this for Zitadel support.
#[allow(clippy::must_use_candidate)]
pub fn get_zitadel_encoded_id(external_id_bytes: Vec<u8>) -> String {
	String::from_utf8(external_id_bytes.clone())
		.unwrap_or_else(|_| BASE64_STANDARD.encode(external_id_bytes))
}

/// Configuration related to Famedly Zitadel
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
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
	/// Enable this only if your service user's permissions aren't specific
	/// enough, or if there are manually created users in the project.
	/// Computation intensive, may struggle or fail with large number of users.
	#[serde(default)]
	pub filter_by_project_id: bool,
}
