//! Script to install LDAP IDs into Zitadel users whose email
//! addresses match the LDAP profile's.

use std::{path::Path, str::FromStr};

use anyhow_ext::{Context, Result};
use famedly_sync::{Config, SkippedErrors, link_user_ids};
use tracing::level_filters::LevelFilter;

#[tokio::main]
#[anyhow_trace::anyhow_trace]
async fn main() -> Result<()> {
	// Config
	let config_path =
		std::env::var("FAMEDLY_SYNC_CONFIG").unwrap_or_else(|_| "./config.yaml".to_owned());
	let config = Config::new(Path::new(&config_path))?;

	// Tracing
	let subscriber = tracing_subscriber::FmtSubscriber::builder()
		.with_max_level(
			config
				.log_level
				.as_ref()
				.map_or(Ok(LevelFilter::INFO), |s| LevelFilter::from_str(s))?,
		)
		.finish();
	tracing::subscriber::set_global_default(subscriber)
		.context("Setting default tracing subscriber failed")?;

	tracing::info!("Starting ID link");
	tracing::info!(
		"Existing Zitadel users will be matched to LDAP users by email addresses, and permanently linked"
	);

	let skipped_errors = SkippedErrors::new();
	link_user_ids(config.clone(), &skipped_errors).await.context("failed to link user IDs")?;

	tracing::info!("Completed ID linking");
	skipped_errors.assert_no_errors()
}
