//! Sync tool between other sources and our infrastructure based on Zitadel.
use std::sync::{
	atomic::{AtomicUsize, Ordering},
	Arc,
};

use anyhow::{Context, Result};
use futures::{StreamExt, TryStreamExt};
use user::User;
use zitadel::{SkipableZitadelResult, Zitadel};

mod config;
mod sources;
pub mod user;
pub mod zitadel;

use std::{collections::VecDeque, pin::pin};

pub use config::{Config, FeatureFlag, LdapSourceConfig};
pub use sources::{
	csv::test_helpers as csv_test_helpers, ldap::AttributeMapping,
	ukt::test_helpers as ukt_test_helpers,
};
use sources::{csv::CsvSource, ldap::LdapSource, ukt::UktSource, Source};

/// Perform a sync operation
pub async fn perform_sync(config: &Config) -> Result<SkippedErrors> {
	/// Get users from a source
	async fn get_users_from_source(source: impl Source + Send) -> Result<VecDeque<User>> {
		source
			.get_sorted_users()
			.await
			.map(VecDeque::from)
			.context(format!("Failed to query users from {}", source.get_name()))
	}

	let csv = config.sources.csv.clone().map(CsvSource::new);
	let ldap = config.sources.ldap.clone().map(LdapSource::new);
	let ukt = config.sources.ukt.clone().map(UktSource::new);

	let skipped_errors = SkippedErrors::new();

	// The ukt source is handled specially, since it doesn't behave as
	// the others
	if let Some(ukt) = ukt {
		match ukt.get_removed_user_emails().await {
			Ok(users) => delete_users_by_email(config, users, skipped_errors.clone()).await?,
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

	if config.feature_flags.is_enabled(FeatureFlag::DeactivateOnly) {
		disable_users(config, &mut users, skipped_errors.clone()).await?;
	} else {
		sync_users(config, &mut users, skipped_errors.clone()).await?;
	}

	Ok(skipped_errors)
}

/// Delete a list of users given their email addresses
async fn delete_users_by_email(
	config: &Config,
	emails: Vec<String>,
	skipped_errors: SkippedErrors,
) -> Result<()> {
	let zitadel = Zitadel::new(config, skipped_errors).await?;
	zitadel
		.get_users_by_email(emails)?
		.try_for_each_concurrent(Some(4), |(zitadel_id, _)| {
			let zitadel = zitadel.clone();
			async move { zitadel.delete_user(&zitadel_id).await }
		})
		.await?;

	Ok(())
}

/// Only disable users
#[tracing::instrument(skip_all)]
async fn disable_users(
	config: &Config,
	users: &mut VecDeque<User>,
	skipped_errors: SkippedErrors,
) -> Result<()> {
	// We only care about disabled users for this flow
	users.retain(|user| !user.enabled);

	let zitadel = Zitadel::new(config, skipped_errors).await?;
	let mut stream = pin!(zitadel.list_users()?);

	while let Some((zitadel_id, zitadel_user)) = stream.next().await.transpose()? {
		if users.front().map(|user| user.external_user_id.clone())
			== Some(zitadel_user.external_user_id)
		{
			zitadel.delete_user(&zitadel_id).await?;
			users.pop_front();
		}
	}

	Ok(())
}

/// Fully sync users
#[tracing::instrument(skip_all)]
async fn sync_users(
	config: &Config,
	sync_users: &mut VecDeque<User>,
	skipped_errors: SkippedErrors,
) -> Result<()> {
	// Treat any disabled users as deleted, so we simply pretend they
	// are not in the list
	sync_users.retain(|user| user.enabled);

	let zitadel = Zitadel::new(config, skipped_errors.clone()).await?;
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
					.skip_zitadel_error("deleting user", skipped_errors.clone());

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
					.skip_zitadel_error("importing user", skipped_errors.clone());

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
					.skip_zitadel_error("importing user", skipped_errors.clone());

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
					.skip_zitadel_error("deleting user", skipped_errors.clone());

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
					.skip_zitadel_error("updating user", skipped_errors.clone());

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

/// Skipped errors tracker
#[derive(Debug, Clone)]
pub struct SkippedErrors(Arc<AtomicUsize>);

#[allow(missing_docs, clippy::new_without_default)]
impl SkippedErrors {
	#[must_use]
	pub fn new() -> Self {
		Self(Arc::new(AtomicUsize::new(0)))
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
