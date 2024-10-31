//! Tool for syncing different sources to Famedly's Zitadel
use std::{path::Path, process::ExitCode, str::FromStr};

use anyhow::{Context, Result};
use famedly_sync::{Config, perform_sync};
use tracing::level_filters::LevelFilter;

#[tokio::main]
async fn main() -> ExitCode {
	match run_sync().await {
		Ok(_) => ExitCode::SUCCESS,
		Err(e) => {
			tracing::error!("{:?}", e);
			ExitCode::FAILURE
		}
	}
}

/// Simple entrypoint without any bells or whistles
#[allow(clippy::print_stderr)]
async fn run_sync() -> Result<()> {
	let config = {
		let config_path = std::env::var("FAMEDLY_LDAP_SYNC_CONFIG").unwrap_or("config.yaml".into());
		let config_path = Path::new(&config_path);
		match Config::new(config_path) {
			Ok(config) => config,
			Err(error) => {
				// Tracing subscriber is not yet configured, so we
				// need to manually log this
				eprintln!("Failed to load config file from {:?}: {}", config_path, error);
				anyhow::bail!(error);
			}
		}
	};

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

	perform_sync(&config).await
}
