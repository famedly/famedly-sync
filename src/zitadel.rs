//! Helper functions for submitting data to Zitadel
mod onboarding;
use std::{path::PathBuf, pin::pin};

use anyhow_ext::{Context, Result};
use base64::prelude::{BASE64_STANDARD, Engine};
use futures::{Stream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use url::Url;
use zitadel_rust_client::v2::{
	Zitadel as ZitadelClient,
	management::{
		V1UserGrantProjectIdQuery, V1UserGrantQuery, V1UserGrantRoleKeyQuery,
		V1UserGrantUserIdQuery,
	},
	pagination::PaginationParams,
	users::{
		AddHumanUserRequest, AndQuery, IdpLink, InUserEmailsQuery, Organization,
		OrganizationIdQuery, SearchQuery, SendEmailVerificationCode, SetHumanEmail, SetHumanPhone,
		SetHumanProfile, SetMetadataEntry, TypeQuery, UpdateHumanUserRequest, User as ZitadelUser,
		UserFieldName, Userv2Type,
	},
};

use crate::{FeatureFlag, SkippedErrors, config::FeatureFlags, user::User};

/// Zitadel user ID alias
pub type ZitadelUserId = String;

/// The Zitadel project role to assign to users.
const FAMEDLY_USER_ROLE: &str = "User";

/// The number of users to sample for encoding detection
const USER_SAMPLE_SIZE: usize = 50;

/// Build the email verification choice shared by import and email changes.
fn human_email(email: String, feature_flags: &FeatureFlags) -> SetHumanEmail {
	let email = SetHumanEmail::new(email);
	if feature_flags.is_enabled(FeatureFlag::VerifyEmail) {
		// These are mutually exclusive verification oneof branches. In
		// particular, isVerified: false does not request a verification email
		// on import.
		email.with_send_code(SendEmailVerificationCode::new())
	} else {
		email.with_is_verified(true)
	}
}

/// A very high-level Zitadel zitadel_client
#[derive(Clone, Debug)]
pub struct Zitadel<'s> {
	/// Zitadel configuration
	zitadel_config: ZitadelConfig,
	/// Optional set of features
	feature_flags: FeatureFlags,
	/// The backing Zitadel zitadel_client
	pub zitadel_client: ZitadelClient,
	/// Lazy first-authentication API extension
	onboarding_client: onboarding::OnboardingClient,
	/// Skipped errors tracker
	skipped_errors: &'s SkippedErrors,
}

#[anyhow_trace::anyhow_trace]
impl<'s> Zitadel<'s> {
	/// Construct the Zitadel instance
	pub async fn new(
		zitadel_config: ZitadelConfig,
		feature_flags: FeatureFlags,
		skipped_errors: &'s SkippedErrors,
	) -> Result<Self> {
		let zitadel_client =
			ZitadelClient::new(zitadel_config.url.clone(), zitadel_config.key_file.clone(), None)
				.await
				.context("failed to configure zitadel_client")?;

		Ok(Self {
			zitadel_config,
			feature_flags,
			zitadel_client,
			skipped_errors,
			onboarding_client: onboarding::OnboardingClient::default(),
		})
	}

	/// Get a list of users by their email addresses
	#[tracing::instrument(skip_all)]
	pub fn get_users_by_email(
		&self,
		emails: Vec<String>,
	) -> Result<impl Stream<Item = Result<(ZitadelUserId, User)>> + Send + use<'_>> {
		Ok(self
			.zitadel_client
			.list_users(
				None,
				Some(PaginationParams::DEFAULT.with_asc(true)),
				Some(UserFieldName::NickName),
				Some(vec![
					SearchQuery::new().with_type_query(TypeQuery::new(Userv2Type::Human)),
					SearchQuery::new().with_in_user_emails_query(
						InUserEmailsQuery::new().with_user_emails(emails),
					),
				]),
			)?
			// TODO: possibly remove this and abort sync,
			// currently preserves previous behavior
			.filter_map(async |res| {
				res.skip_zitadel_error("fetching users by email", self.skipped_errors)
			})
			.then(async |user| self.search_result_to_user(user).await)
			// TODO: figure out what to do if zitadel users lack metadata
			.filter_map(Skippable::filter_out))
	}

	/// Return a stream of raw Zitadel users
	#[tracing::instrument(skip_all)]
	pub fn list_users_raw(
		&self,
	) -> Result<impl Stream<Item = Result<ZitadelUser>> + Send + use<'_>> {
		Ok(self
			.zitadel_client
			.list_users(
				None,
				Some(PaginationParams::DEFAULT.with_asc(true)),
				Some(UserFieldName::NickName),
				Some(vec![SearchQuery::new().with_and_query(AndQuery::new().with_queries(vec![
					SearchQuery::new().with_type_query(TypeQuery::new(Userv2Type::Human)),
					SearchQuery::new().with_organization_id_query(OrganizationIdQuery::new(
						self.zitadel_config.organization_id.clone(),
					)),
				]))]),
			)?
			.try_filter_map(async |user| {
				let id = user.user_id().context("Missing Zitadel user ID")?.clone();

				let grant = self
					.zitadel_client
					.search_user_grants(
						Some(self.zitadel_config.organization_id.clone()),
						Some(PaginationParams::default().with_page_size(1)),
						Some(vec![
							V1UserGrantQuery::ProjectId {
								project_id_query: (V1UserGrantProjectIdQuery::new()
									.with_project_id(self.zitadel_config.project_id.clone())),
							},
							V1UserGrantQuery::RoleKey {
								role_key_query: V1UserGrantRoleKeyQuery::new()
									.with_role_key("User".into()),
							},
							V1UserGrantQuery::UserId {
								user_id_query: V1UserGrantUserIdQuery::new()
									.with_user_id(id.clone()),
							},
						]),
					)?
					.next()
					.await;
				Ok(grant.transpose()?.is_some().then_some(user))
			}))
	}

	/// Return a stream of Zitadel users
	#[tracing::instrument(skip_all)]
	pub fn list_users(
		&self,
	) -> Result<impl Stream<Item = Result<(ZitadelUserId, User)>> + Send + use<'_>> {
		Ok(self
			.list_users_raw()?
			// TODO: possibly remove this and abort sync
			// currently preserves previous behavior
			.filter_map(async |res| {
				res.skip_zitadel_error("fetching users by email", self.skipped_errors)
			})
			.then(async |user| self.search_result_to_user(user).await)
			// TODO: figure out what to do if zitadel users lack metadata
			.filter_map(Skippable::filter_out))
	}

	/// Return a vector of a random sample of Zitadel users
	/// We use this to determine the encoding of the external IDs
	pub async fn get_users_sample(&self) -> Result<Vec<User>> {
		self.zitadel_client
			.list_users(
				None,
				Some(PaginationParams::DEFAULT.with_asc(true).with_page_size(USER_SAMPLE_SIZE)),
				Some(UserFieldName::NickName),
				Some(vec![SearchQuery::new().with_type_query(TypeQuery::new(Userv2Type::Human))]),
			)?
			// TODO: possibly remove this and abort sync
			// currently preserves previous behavior
			.filter_map(async |res| {
				res.skip_zitadel_error("fetching users by email", self.skipped_errors)
			})
			.then(async |user| Ok(self.search_result_to_user(user).await?.1))
			// TODO: figure out what to do if zitadel users lack metadata
			.filter_map(Skippable::filter_out)
			.try_collect::<Vec<_>>()
			.await
	}

	/// Delete a Zitadel user
	pub async fn delete_user(&self, zitadel_id: &str) -> Result<()> {
		tracing::info!("Deleting user with Zitadel ID: {}", zitadel_id);

		if self.feature_flags.is_enabled(FeatureFlag::DryRun) {
			tracing::warn!("Skipping deletion due to dry run");
			return Ok(());
		}

		self.zitadel_client.delete_user(zitadel_id).await.map(|_o| ())
	}

	/// Invitation verification verifies the address, so imports need only one
	/// mail.
	fn import_email(&self, email: String) -> SetHumanEmail {
		if self.feature_flags.is_enabled(FeatureFlag::SsoLogin) {
			human_email(email, &self.feature_flags)
		} else {
			SetHumanEmail::new(email)
				.with_is_verified(!self.feature_flags.is_enabled(FeatureFlag::VerifyEmail))
		}
	}

	/// Import a user into Zitadel
	pub async fn import_user(&self, imported_user: &User) -> Result<()> {
		tracing::info!("Importing user with external ID: {}", imported_user.external_user_id);

		if self.feature_flags.is_enabled(FeatureFlag::DryRun) {
			tracing::warn!("Skipping import due to dry run");
			return Ok(());
		}

		let mut metadata =
			vec![SetMetadataEntry::new("localpart".to_owned(), imported_user.localpart.clone())];
		if let Some(preferred_username) = &imported_user.preferred_username {
			metadata.push(SetMetadataEntry::new(
				"preferred_username".to_owned(),
				preferred_username.clone(),
			));
		}

		let invite = !self.feature_flags.is_enabled(FeatureFlag::SsoLogin);
		if invite {
			metadata.push(SetMetadataEntry::new(self.onboarding_key(), "pending".to_owned()));
		}
		let email = self.import_email(imported_user.email.clone());
		let mut user = AddHumanUserRequest::new(
			SetHumanProfile::new(imported_user.first_name.clone(), imported_user.last_name.clone())
				.with_nick_name(imported_user.external_user_id.clone())
				.with_display_name(imported_user.get_display_name()),
			email,
		)
		.with_organization(
			Organization::new().with_org_id(self.zitadel_config.organization_id.clone()),
		)
		.with_metadata(metadata);

		if let Some(phone) = imported_user.phone.clone() {
			user.set_phone(
				SetHumanPhone::new()
					.with_phone(phone.clone())
					.with_is_verified(!self.feature_flags.is_enabled(FeatureFlag::VerifyPhone)),
			);
		};

		if self.feature_flags.is_enabled(FeatureFlag::SsoLogin) {
			let idp_id = self
				.zitadel_config
				.idp_id
				.as_ref()
				.context("idp_id is required when sso_login feature flag is enabled")?;
			user.set_idp_links(vec![
				IdpLink::new()
					.with_user_id(get_zitadel_encoded_id(imported_user.get_external_id_bytes()?))
					.with_idp_id(idp_id.clone())
					.with_user_name(imported_user.email.clone()),
			]);
		}

		match self.zitadel_client.create_human_user(user.clone()).await {
			Ok(res) => {
				let id = res.user_id().with_context(|| {
					format!(
						"Failed to create user ID for external user `{}`",
						imported_user.external_user_id
					)
				})?;

				self.ensure_onboarding_grant(id).await?;
				self.resume_onboarding(id).await?;
			}

			Err(error) => {
				// The v2 client wraps the actual Zitadel error (including the
				// error code we match on below) as a source in the error chain,
				// so we must inspect the full chain rather than just the
				// top-level `Display`.
				let error_chain = format!("{error:?}");

				// If the phone number is invalid
				if error_chain.contains("PHONE-so0wa") {
					user.reset_phone();
					let created = self.zitadel_client.create_human_user(user).await?;
					let id = created.user_id().context("Missing created user ID")?;
					self.ensure_onboarding_grant(id).await?;
					self.resume_onboarding(id).await?;

				// If the user already exists
				} else if error_chain.contains("V3-DKcYh") {
					// Handle the case where a user with the same email already
					// exists This can happen when the external ID changes
					// but the email stays the same Since we are keeping
					// deleted users in Zitadel for safety reasons unless they
					// are explicitly disabled in LDAP, we need to update the
					// Zitadel user instead of how we did it previously
					// (deleting old and creating a new one)
					tracing::info!(
						"User with a different external ID ({}) having the same email already exists in Zitadel, attempting to update",
						imported_user.external_user_id
					);

					// Look up the existing user by email
					let mut existing_users =
						pin!(self.get_users_by_email(vec![imported_user.email.clone()])?);

					let (existing_zitadel_id, existing_user) =
						existing_users.next().await.transpose()?.with_context(|| {
							format!(
								"Failed to find existing user having different external ID {} by email",
								imported_user.external_user_id
							)
						})?;

					tracing::info!(
						"Found existing user with Zitadel ID {} by email, updating from external ID {} to {} and possibly other changes",
						existing_zitadel_id,
						existing_user.external_user_id,
						imported_user.external_user_id
					);

					// Update the existing user with the new external ID and
					// other changes
					self.update_user(&existing_zitadel_id, &existing_user, imported_user).await?;
					// An import interrupted between creation and grant
					// creation is invisible to the sync (list_users requires
					// the project grant), so repair and resume it here.
					self.resume_onboarding(&existing_zitadel_id).await?;
				} else {
					anyhow::bail!(error)
				}
			}
		}

		Ok(())
	}

	/// Update a user
	pub async fn update_user(
		&self,
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
			self.prepare_onboarding_email(zitadel_id, &updated_user.email).await?;
			request.set_username(updated_user.email.clone());
			request.set_email(human_email(updated_user.email.clone(), &self.feature_flags));
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

		let mut phone_rejected = false;
		if let Err(error) = self.zitadel_client.update_human_user(zitadel_id, request.clone()).await
		{
			// The v2 client surfaces the Zitadel error code as a source in the
			// error chain, so we match against the whole chain.
			// If the new phone number is invalid
			if format!("{error:?}").contains("PHONE-so0wa") {
				phone_rejected = true;
				request.reset_phone();
				self.zitadel_client.update_human_user(zitadel_id, request).await?;

				if let Err(error) = self.zitadel_client.remove_phone(zitadel_id).await {
					// If the user didn't start out with a phone
					if !format!("{error:?}").contains("COMMAND-ieJ2e") {
						anyhow::bail!(error);
					}
				};
			} else {
				anyhow::bail!(error);
			}
		};

		if old_user.preferred_username != updated_user.preferred_username {
			if let Some(preferred_username) = &updated_user.preferred_username {
				self.zitadel_client
					.set_user_metadata(zitadel_id, "preferred_username", preferred_username, None)
					.await?;
			} else {
				self.zitadel_client
					.delete_user_metadata(zitadel_id, "preferred_username", None)
					.await?;
			}
		}

		// Profile-only migrations must not implicitly send onboarding mail.
		// Normal unchanged syncs explicitly resume pending work. A rejected
		// phone or a refused localpart change, however, keeps the user on
		// this update path on every later sync, so queued retries must also
		// run here or they never do.
		if old_user.email != updated_user.email
			|| phone_rejected
			|| old_user.localpart != updated_user.localpart
		{
			self.resume_onboarding(zitadel_id).await?;
		}
		Ok(())
	}

	/// Convert a Zitadel search result to a user
	async fn search_result_to_user(&self, user: ZitadelUser) -> Result<(ZitadelUserId, User)> {
		let id = user.user_id().context("Missing Zitadel user ID")?.clone();
		let human_user = user.human().context("Machine user found in human user search")?;
		let external_id = human_user
			.profile()
			.and_then(|p| p.nick_name())
			.context(format!("Missing external ID (nickname) for user {id}"))?
			.clone();

		let mk_err = |smth| format!("Missing {smth} for zitadel user {external_id} ({id})");

		let first_name = human_user
			.profile()
			.and_then(|profile| profile.given_name())
			.with_context(|| mk_err("first name"))?
			.clone();

		let last_name = human_user
			.profile()
			.and_then(|profile| profile.family_name())
			.with_context(|| mk_err("last name"))?
			.clone();

		let email = human_user
			.email()
			.and_then(|human_email| human_email.email())
			.with_context(|| mk_err("email address"))?
			.clone();

		let phone = human_user.phone().and_then(|human_phone| human_phone.phone()).cloned();
		let localpart = self
			.zitadel_client
			.get_user_metadata(&id, "localpart", None)
			.await
			.pipe(|x| anyhow::Context::context(x, Skippable))
			.with_context(|| format!("Fetching localpart metadata for {external_id:?} ({id})"))?
			.metadata()
			.value()
			.pipe(|x| anyhow::Context::context(x, Skippable))
			.with_context(|| mk_err("localpart"))?;

		let preferred_username = self
			.zitadel_client
			.get_user_metadata(&id, "preferred_username", None)
			.await
			.ok()
			.and_then(|res| res.metadata().value());

		Ok((
			id,
			User {
				first_name,
				last_name,
				email,
				phone,
				enabled: true,
				preferred_username,
				external_user_id: external_id,
				localpart,
			},
		))
	}
}

/// Marker error to filter out some errors
#[derive(Debug, Clone)]
struct Skippable;

impl Skippable {
	/// Helper function to use with `[StreamExt::filter_map]`
	#[allow(clippy::unused_async)]
	#[tracing::instrument(skip_all)]
	pub async fn filter_out<T: Send>(res: Result<T>) -> Option<Result<T>> {
		// should we `skipped_errors.notify_error()` here?
		if let Err(e) = res.as_ref()
			&& e.is::<Skippable>()
		{
			tracing::warn!("{e:?}");
			return None;
		}
		Some(res)
	}
}

impl std::fmt::Display for Skippable {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
		write!(f, "Skippable error")
	}
}

/// TODO: add to `famedly-rust-utils`
trait GenericCombinatorsExt {
	/// We need this to call `anyhow::Context::context` method and not
	/// `anyhow_ext::Context::context` as the latter serializes errors into
	/// strings, and we need to do `anyhow::Error::is` ~
	/// `anyhow::Error::downcast_ref` on nonserialized error marker
	/// [Skippable]. This method is needed to not disrupt existing chains.
	fn pipe<X>(self, f: impl Fn(Self) -> X) -> X
	where
		Self: Sized,
	{
		f(self)
	}
}

impl<X> GenericCombinatorsExt for X {}

/// Helper trait for skippable zitadel errors to use with `[SkippedErrors]`
pub trait SkippableZitadelResult<X: Send> {
	/// Helper method for skippable zitadel errors to use with `[SkippedErrors]`
	fn skip_zitadel_error(
		self,
		operation: &'static str,
		skipped_errors: &SkippedErrors,
	) -> Option<X>;
}

impl<X: Send> SkippableZitadelResult<X> for Result<X> {
	fn skip_zitadel_error(
		self,
		operation: &'static str,
		skipped_errors: &SkippedErrors,
	) -> Option<X> {
		self.inspect_err(|err| {
			skipped_errors.notify_error(format!("Zitadel operation {operation} failed: {err:?}"));
		})
		.ok()
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
	/// IDP ID provided by Famedly Zitadel (only required when SSO is enabled)
	pub idp_id: Option<String>,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn verify_email_requests_code_without_selecting_another_oneof_branch() -> Result<()> {
		let mut flags = FeatureFlags::default();
		flags.push(FeatureFlag::VerifyEmail);
		let email = serde_json::to_value(human_email("verify@example.test".into(), &flags))?;
		assert_eq!(email["email"], "verify@example.test");
		assert!(email["sendCode"].is_object(), "verify_email must request code delivery");
		assert!(email["isVerified"].is_null());
		assert!(email["returnCode"].is_null());
		Ok(())
	}

	#[test]
	fn verify_email_disabled_marks_verified_without_requesting_code() -> Result<()> {
		let email = serde_json::to_value(human_email(
			"verified@example.test".into(),
			&FeatureFlags::default(),
		))?;
		assert_eq!(email["email"], "verified@example.test");
		assert_eq!(email["isVerified"], true);
		assert!(email["sendCode"].is_null());
		assert!(email["returnCode"].is_null());
		Ok(())
	}
}
