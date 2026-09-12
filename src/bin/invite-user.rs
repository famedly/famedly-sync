//! Explicit, single-user onboarding backfill. Dry-run unless --send is
//! supplied.
use std::{env, path::Path};

use anyhow::{Context, Result, ensure};
use famedly_sync::{Config, FeatureFlag, SkippedErrors, zitadel::Zitadel};

#[tokio::main]
async fn main() -> Result<()> {
	let args: Vec<_> = env::args().skip(1).collect();
	ensure!(
		!args.is_empty()
			&& args.len() <= 3
			&& args[0].bytes().all(|c| c.is_ascii_digit())
			&& !args[0].is_empty()
			&& args[1..].iter().all(|arg| arg == "--send" || arg == "--retry-uncertain"),
		"Usage: invite-user USER_ID [--send] [--retry-uncertain] (dry-run by default)"
	);
	let path = env::var("FAMEDLY_SYNC_CONFIG").context("Set FAMEDLY_SYNC_CONFIG")?;
	let mut config = Config::new(Path::new(&path))?;
	if !args.iter().any(|arg| arg == "--send") {
		config.feature_flags.push(FeatureFlag::DryRun);
	}
	let errors = SkippedErrors::new();
	let sync = Zitadel::new(config.zitadel, config.feature_flags, &errors).await?;
	sync.backfill_onboarding(&args[0], args.iter().any(|arg| arg == "--retry-uncertain")).await?;
	errors.assert_no_errors()
}
