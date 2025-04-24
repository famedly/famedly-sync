//! Sync tool between other sources and our infrastructure based on Zitadel.
use std::{
	collections::{HashMap, HashSet},
	sync::atomic::{AtomicUsize, Ordering},
};

use anyhow_ext::{Context, Result};
use futures::{StreamExt, TryStreamExt};
use user::User;
use zitadel::{SkipableZitadelResult, Zitadel};
use zitadel_rust_client::v2::users::{SetHumanProfile, UpdateHumanUserRequest};

mod config;
mod sources;
pub mod user;
pub mod zitadel;

use std::{collections::VecDeque, pin::pin};

pub use config::{Config, FeatureFlag, LdapSourceConfig};
use sources::{Source, csv::CsvSource, ldap::LdapSource, ukt::UktSource};
pub use sources::{
	csv::test_helpers as csv_test_helpers, ldap::AttributeMapping,
	ukt::test_helpers as ukt_test_helpers,
};

/// Perform a sync operation
#[anyhow_trace::anyhow_trace]
pub async fn perform_sync(config: Config) -> Result<SkippedErrors> {
	/// Get users from a source
	async fn get_users_from_source(source: impl Source + Send) -> Result<VecDeque<User>> {
		source
			.get_sorted_users()
			.await
			.map(VecDeque::from)
			.context(format!("Failed to query users from {}", source.get_name()))
	}

	let deactivate_only = config.feature_flags.is_enabled(FeatureFlag::DeactivateOnly);

	let skipped_errors = SkippedErrors::new();
	let zitadel = Zitadel::new(config.zitadel, config.feature_flags, &skipped_errors).await?;

	let csv = config.sources.csv.map(CsvSource::new);
	let ldap = config.sources.ldap.map(LdapSource::new);
	let ukt = config.sources.ukt.map(UktSource::new);

	// The ukt source is handled specially, since it doesn't behave as
	// the others
	if let Some(ukt) = ukt {
		match ukt.get_removed_user_emails().await {
			Ok(users) => delete_users_by_email(&zitadel, users).await?,
			Err(err) => {
				anyhow::bail!("Failed to query users from ukt: {:?}", err);
			}
		}

		return Ok(skipped_errors);
	}

	let mut users = match (csv, ldap, ukt) {
		(Some(csv), None, None) => get_users_from_source(csv).await?,
		(None, Some(ldap), None) => get_users_from_source(ldap).await?,
		(None, None, Some(_)) => VecDeque::new(),
		_ => {
			anyhow::bail!("Exactly one source must be defined");
		}
	};

	if deactivate_only {
		disable_users(&zitadel, &mut users).await?;
	} else {
		sync_users(&zitadel, &skipped_errors, &mut users).await?;
	}

	Ok(skipped_errors)
}

/// Delete a list of users given their email addresses
#[anyhow_trace::anyhow_trace]
async fn delete_users_by_email(
	zitadel: &Zitadel<'_>,
	// skipped_errors: &SkippedErrors,
	emails: Vec<String>,
) -> Result<()> {
	zitadel
		.get_users_by_email(emails)?
		.try_for_each_concurrent(Some(4), async |(zitadel_id, _)| {
			zitadel.delete_user(&zitadel_id).await?;
			// .skip_zitadel_error("deleting user", skipped_errors);
			Ok(())
		})
		.await?;

	Ok(())
}

/// Only disable users
#[tracing::instrument(skip_all)]
#[anyhow_trace::anyhow_trace]
async fn disable_users(
	zitadel: &Zitadel<'_>,
	// skipped_errors: &SkippedErrors,
	users: &mut VecDeque<User>,
) -> Result<()> {
	// We only care about disabled users for this flow
	users.retain(|user| !user.enabled);

	let mut stream = pin!(zitadel.list_users()?);

	while let Some((zitadel_id, zitadel_user)) = stream.next().await.transpose()? {
		if users.front().map(|user| user.external_user_id.clone())
			== Some(zitadel_user.external_user_id)
		{
			zitadel.delete_user(&zitadel_id).await?;
			// .skip_zitadel_error("deleting user", skipped_errors);
			users.pop_front();
		}
	}

	Ok(())
}

/// Fully sync users
#[anyhow_trace::anyhow_trace]
#[tracing::instrument(skip_all)]
async fn sync_users(
	zitadel: &Zitadel<'_>,
	skipped_errors: &SkippedErrors,
	sync_users: &mut VecDeque<User>,
) -> Result<()> {
	// Treat any disabled users as deleted, so we simply pretend they
	// are not in the list
	sync_users.retain(|user| user.enabled);

	let mut stream = pin!(zitadel.list_users()?);

	let mut source_user = sync_users.pop_front();
	let mut zitadel_user = stream.next().await.transpose()?;

	loop {
		tracing::debug!(
			"Comparing users {:?} and {:?}",
			source_user.as_ref().map(|user| user.external_user_id.clone()),
			zitadel_user.as_ref().map(|user| user.1.external_user_id.clone())
		);

		match (source_user.clone(), zitadel_user.clone()) {
			(None, None) => {
				tracing::info!("Sync completed successfully");
				break;
			}

			// Excess Zitadel users are not present in the sync
			// source, so we delete them
			(None, Some((zitadel_id, _))) => {
				zitadel
					.delete_user(&zitadel_id)
					.await
					.with_context(|| {
						format!("Failed to delete user with Zitadel ID `{}`", zitadel_id,)
					})
					.skip_zitadel_error("deleting user", skipped_errors);

				zitadel_user = stream.next().await.transpose()?;
			}

			// Excess sync source users are not yet in Zitadel, so
			// we import them
			(Some(new_user), None) => {
				zitadel
					.import_user(&new_user)
					.await
					.with_context(|| {
						format!("Failed to import user `{}`", new_user.external_user_id)
					})
					.skip_zitadel_error("importing user", skipped_errors);

				source_user = sync_users.pop_front();
			}

			// If the sync source user matches the Zitadel user, the
			// user is already synced and we can move on
			(Some(new_user), Some((_, existing_user))) if new_user == existing_user => {
				zitadel_user = stream.next().await.transpose()?;
				source_user = sync_users.pop_front();
			}

			// If the user ID of the user to be synced to Zitadel is <
			// the user ID of the current Zitadel user, we found a new
			// user which we should be importing
			(Some(new_user), Some((_, existing_user)))
				if new_user.external_user_id < existing_user.external_user_id =>
			{
				zitadel
					.import_user(&new_user)
					.await
					.with_context(|| {
						format!("Failed to import user `{}`", new_user.external_user_id,)
					})
					.skip_zitadel_error("importing user", skipped_errors);

				source_user = sync_users.pop_front();
				// Don't fetch the next zitadel user yet
			}

			// If the user ID of the user to be synced to Zitadel is >
			// the user ID of the current Zitadel user, the Zitadel
			// user needs to be deleted
			(Some(new_user), Some((zitadel_id, existing_user)))
				if new_user.external_user_id > existing_user.external_user_id =>
			{
				zitadel
					.delete_user(&zitadel_id)
					.await
					.with_context(|| {
						format!("Failed to delete user with Zitadel ID `{}`", zitadel_id,)
					})
					.skip_zitadel_error("deleting user", skipped_errors);

				zitadel_user = stream.next().await.transpose()?;
				// Don't move to the next source user yet
			}

			// If the users don't match (since we've failed the former
			// checks), but the user IDs are the same, the user has
			// been updated
			(Some(new_user), Some((zitadel_id, existing_user)))
				if new_user.external_user_id == existing_user.external_user_id =>
			{
				zitadel
					.update_user(&zitadel_id, &existing_user, &new_user)
					.await
					.with_context(|| {
						format!("Failed to update user `{}`", new_user.external_user_id,)
					})
					.skip_zitadel_error("updating user", skipped_errors);

				zitadel_user = stream.next().await.transpose()?;
				source_user = sync_users.pop_front();
			}

			// Since the user IDs form a partial order, they must be
			// either equal, less than, or greater than, one another.
			//
			// Since all other possible conditions are checked in the
			// first case, this particular case is unreachable.
			(Some(new_user), Some((_, existing_user))) => {
				skipped_errors.notify_error(format!(
					"Unreachable condition met for users `{}` and `{}`",
					new_user.external_user_id, existing_user.external_user_id
				));
			}
		}
	}

	Ok(())
}

/// Pull user IDs from LDAP into Zitadel
pub async fn link_user_ids(config: Config, skipped_errors: &SkippedErrors) -> Result<()> {
	let Some(ldap_config) = config.sources.ldap else {
		anyhow::bail!("LDAP must be configured to link user IDs")
	};

	let ldap_client = LdapSource::new(ldap_config);
	let zitadel_client =
		Zitadel::new(config.zitadel.clone(), config.feature_flags, skipped_errors).await?;

	let ldap_users: HashMap<String, User> = {
		let users = ldap_client.get_sorted_users().await.context("Failed to query LDAP users")?;
		users.into_iter().map(|user| (user.email.clone(), user)).collect()
	};

	tracing::trace!("Found LDAP users: {:?}", ldap_users);

	let mut zitadel_users =
		pin!(zitadel_client.list_users_raw().context("failed to query Zitadel users")?);

	let mut seen_emails: HashSet<String> = HashSet::new();

	while let Some(user) = zitadel_users.next().await.transpose().context("failed to query user")? {
		let Some(zitadel_id) = user.user_id() else {
			tracing::error!(
				"Skipping user without a Zitadel ID. Users like this should never appear, this Zitadel instance is very broken."
			);
			continue;
		};

		let Some(human_user) = user.human() else {
			tracing::error!("Skipping ID linking for non-human user `{zitadel_id}`");
			continue;
		};

		let Some(email) = human_user.email().and_then(|e| e.email()) else {
			tracing::error!("Skipping ID linking for user `{zitadel_id}` without an email address");
			continue;
		};

		tracing::trace!("Trying to match user with email {}", email);

		if !seen_emails.insert(email.to_owned()) {
			// Zitadel doesn't actually allow this case, but it's here
			// just in case
			tracing::error!("Multiple users with the same email address exist");
			tracing::error!("Refusing to duplicate ID of user `{zitadel_id}`");
			tracing::error!("Manual conversion of some users will be required");
			continue;
		};

		let Some(given_name) = human_user.profile().and_then(|p| p.given_name()) else {
			tracing::error!(
				"Skipping work for a user without a given name, because they are immune to Zitadel's API. Give user `{zitadel_id}` a name!"
			);
			continue;
		};
		let Some(last_name) = human_user.profile().and_then(|p| p.family_name()) else {
			tracing::error!(
				"Skipping work for a user without a last name, because they are immune to Zitadel's API. Give user `{zitadel_id}` a name!"
			);
			continue;
		};
		let nick = human_user.profile().and_then(|p| p.nick_name());
		let Some(ldap_id) = ldap_users.get(email).map(|lu| lu.external_user_id.clone()) else {
			tracing::error!("User `{zitadel_id}` does not have a corresponding LDAP user");
			continue;
		};

		tracing::debug!("Found LDAP user `{}`", ldap_id);

		match nick {
			Some(nick) if nick.is_empty() => {
				let mut request = UpdateHumanUserRequest::new();
				request.set_profile(
					SetHumanProfile::new(given_name.clone(), last_name.clone())
						.with_nick_name(ldap_id),
				);

				if let Err(error) =
					zitadel_client.zitadel_client.update_human_user(zitadel_id, request).await
				{
					tracing::error!(
						"Failed to set nickname field for user `{zitadel_id}: {:?}",
						error
					);
					continue;
				};

				tracing::info!("Updated LDAP link for user `{zitadel_id}`");
			}
			Some(nick) if *nick != ldap_id => {
				tracing::error!(
					"External ID for user `{zitadel_id}` does not match the external ID for the LDAP user with their email address, {nick} {ldap_id}"
				);
				tracing::error!(
					"Something has gone very wrong for this user, please correct their data manually"
				);
				continue;
			}
			Some(nick) => {
				tracing::info!("User `{zitadel_id}` is already linked to user `{nick}`");
			}
			None => {
				unreachable!()
			}
		}
	}

	Ok(())
}

/// Skipped errors tracker
#[derive(Debug)]
pub struct SkippedErrors(AtomicUsize);

#[allow(missing_docs, clippy::new_without_default)]
impl SkippedErrors {
	#[must_use]
	pub fn new() -> Self {
		Self(AtomicUsize::new(0))
	}
	pub fn notify_error(&self, err: impl AsRef<str>) {
		self.0.fetch_add(1, Ordering::Relaxed);
		tracing::error!("{}", err.as_ref());
	}
	pub fn assert_no_errors(&self) -> Result<()> {
		let n = self.0.load(Ordering::Relaxed);
		anyhow::ensure!(n == 0, "During the execution {n} errors occurred that were skipped");
		Ok(())
	}
}
