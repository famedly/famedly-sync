//! Sync tool between other sources and our infrastructure based on Zitadel.
use anyhow::{Context, Result};
use futures::{Stream, StreamExt};
use user::User;
use zitadel::{Zitadel, ZitadelUserBuilder};

mod config;
mod sources;
pub mod user;
pub mod zitadel;

use std::collections::VecDeque;

pub use config::{Config, FeatureFlag, LdapSourceConfig};
pub use sources::{
	csv::test_helpers as csv_test_helpers, ldap::AttributeMapping,
	ukt::test_helpers as ukt_test_helpers,
};
use sources::{csv::CsvSource, ldap::LdapSource, ukt::UktSource, Source};

/// Helper function to add metadata to streamed zitadel users
// TODO: If async closures become a reality, this should be factored
// into the `zitadel::search_result_to_user` function
pub async fn get_next_zitadel_user(
	stream: &mut (impl Stream<Item = Result<(ZitadelUserBuilder, String)>> + Send + Unpin),
	zitadel: &mut Zitadel,
) -> Result<Option<(User, String)>> {
	match stream.next().await.transpose()? {
		Some((zitadel_user, id)) => {
			let Some(localpart) = zitadel
				.zitadel_client
				.get_user_metadata(&id, "localpart")
				.await
				.ok()
				.and_then(|metadata| metadata.metadata().value())
			else {
				tracing::warn!("Zitadel user without a valid localpart: {}", id);
				return Box::pin(get_next_zitadel_user(stream, zitadel)).await;
			};

			let Some(preferred_username) = zitadel
				.zitadel_client
				.get_user_metadata(&id, "preferred_username")
				.await
				.ok()
				.and_then(|metadata| metadata.metadata().value())
			else {
				tracing::warn!("Zitadel user without a preferred username: {}", id);
				return Box::pin(get_next_zitadel_user(stream, zitadel)).await;
			};

			// We've supplied the required data, so this should never
			// error in practice; if it does, that's a bug
			let user = zitadel_user
				.with_localpart(localpart)
				.with_preferred_username(preferred_username)
				.build()?;

			Ok(Some((user, id)))
		}
		None => Ok(None),
	}
}

/// Perform a sync operation
pub async fn perform_sync(config: &Config) -> Result<()> {
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

	// The ukt source is handled specially, since it doesn't behave as
	// the others
	if let Some(ukt) = ukt {
		match ukt.get_removed_user_emails().await {
			Ok(users) => delete_users_by_email(config, users).await?,
			Err(err) => {
				anyhow::bail!("Failed to query users from ukt: {:?}", err);
			}
		}

		return Ok(());
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
		disable_users(config, &mut users).await?;
	} else {
		sync_users(config, &mut users).await?;
	}

	Ok(())
}

/// Delete a list of users given their email addresses
async fn delete_users_by_email(config: &Config, emails: Vec<String>) -> Result<()> {
	let mut zitadel = Zitadel::new(config).await?;
	let mut stream = zitadel.get_users_by_email(emails)?;

	while let Some(zitadel_user) = get_next_zitadel_user(&mut stream, &mut zitadel).await? {
		zitadel.delete_user(&zitadel_user.1).await?;
	}

	Ok(())
}

/// Only disable users
async fn disable_users(config: &Config, users: &mut VecDeque<User>) -> Result<()> {
	// We only care about disabled users for this flow
	users.retain(|user| !user.enabled);

	let mut zitadel = Zitadel::new(config).await?;
	let mut stream = zitadel.list_users()?;

	while let Some(zitadel_user) = get_next_zitadel_user(&mut stream, &mut zitadel).await? {
		if users.front().map(|user| user.external_user_id.clone())
			== Some(zitadel_user.0.external_user_id)
		{
			zitadel.delete_user(&zitadel_user.1).await?;
			users.pop_front();
		}
	}

	Ok(())
}

/// Fully sync users
async fn sync_users(config: &Config, sync_users: &mut VecDeque<User>) -> Result<()> {
	// Treat any disabled users as deleted, so we simply pretend they
	// are not in the list
	sync_users.retain(|user| user.enabled);

	let mut zitadel = Zitadel::new(config).await?;
	let mut stream = zitadel.list_users()?;

	let mut source_user = sync_users.pop_front();
	let mut zitadel_user = get_next_zitadel_user(&mut stream, &mut zitadel).await?;

	loop {
		tracing::debug!(
			"Comparing users {:?} and {:?}",
			source_user.as_ref().map(|user| user.external_user_id.clone()),
			zitadel_user.as_ref().map(|user| user.0.external_user_id.clone())
		);

		match (source_user.clone(), zitadel_user.clone()) {
			(None, None) => {
				tracing::info!("Sync completed successfully");
				break;
			}

			// Excess Zitadel users are not present in the sync
			// source, so we delete them
			(None, Some((_, zitadel_id))) => {
				let res = zitadel.delete_user(&zitadel_id).await;
				if let Err(error) = res {
					tracing::error!(
						"Failed to delete user with Zitadel ID `{}`: {}",
						zitadel_id,
						error
					);
				}

				zitadel_user = get_next_zitadel_user(&mut stream, &mut zitadel).await?;
			}

			// Excess sync source users are not yet in Zitadel, so
			// we import them
			(Some(new_user), None) => {
				let res = zitadel.import_user(&new_user).await;
				if let Err(error) = res {
					tracing::error!(
						"Failed to import user `{}`: {}",
						new_user.external_user_id,
						error
					);
				}

				source_user = sync_users.pop_front();
			}

			// If the sync source user matches the Zitadel user, the
			// user is already synced and we can move on
			(Some(new_user), Some((existing_user, _))) if new_user == existing_user => {
				zitadel_user = get_next_zitadel_user(&mut stream, &mut zitadel).await?;
				source_user = sync_users.pop_front();
			}

			// If the user ID of the user to be synced to Zitadel is <
			// the user ID of the current Zitadel user, we found a new
			// user which we should be importing
			(Some(new_user), Some((existing_user, _)))
				if new_user.external_user_id < existing_user.external_user_id =>
			{
				let res = zitadel.import_user(&new_user).await;
				if let Err(error) = res {
					tracing::error!(
						"Failed to import user `{}`: {}",
						new_user.external_user_id,
						error
					);
				}

				source_user = sync_users.pop_front();
				// Don't fetch the next zitadel user yet
			}

			// If the user ID of the user to be synced to Zitadel is >
			// the user ID of the current Zitadel user, the Zitadel
			// user needs to be deleted
			(Some(new_user), Some((existing_user, zitadel_id)))
				if new_user.external_user_id > existing_user.external_user_id =>
			{
				let res = zitadel.delete_user(&zitadel_id).await;
				if let Err(error) = res {
					tracing::error!(
						"Failed to delete user with Zitadel ID `{}`: {}",
						zitadel_id,
						error
					);
				}

				zitadel_user = get_next_zitadel_user(&mut stream, &mut zitadel).await?;
				// Don't move to the next source user yet
			}

			// If the users don't match (since we've failed the former
			// checks), but the user IDs are the same, the user has
			// been updated
			(Some(new_user), Some((existing_user, zitadel_id)))
				if new_user.external_user_id == existing_user.external_user_id =>
			{
				let res = zitadel.update_user(&zitadel_id, &existing_user, &new_user).await;
				if let Err(error) = res {
					tracing::error!(
						"Failed to update user `{}`: {}",
						new_user.external_user_id,
						error
					);
				}

				zitadel_user = get_next_zitadel_user(&mut stream, &mut zitadel).await?;
				source_user = sync_users.pop_front();
			}

			// Since the user IDs form a partial order, they must be
			// either equal, less than, or greater than, one another.
			//
			// Since all other possible conditions are checked in the
			// first case, this particular case is unreachable.
			(Some(new_user), Some((existing_user, _))) => {
				tracing::error!(
					"Unreachable condition met for users `{}` and `{}`",
					new_user.external_user_id,
					existing_user.external_user_id
				);
			}
		}
	}

	Ok(())
}
