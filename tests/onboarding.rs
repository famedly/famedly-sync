//! Real first-authentication invitation regression; run via onboarding/run.py.
use std::{
	env,
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use famedly_sync::{
	Config, FeatureFlag, SkippedErrors,
	user::User,
	zitadel::{Zitadel, ZitadelConfig},
};
use futures::TryStreamExt;
use serde_json::{Value, json};

/// Poll exact recipient for the full window to detect duplicate delivery.
async fn mail(http: &reqwest::Client, base: &str, email: &str) -> Result<Value> {
	let value: Value = http
		.get(format!("{base}/api/v1/search"))
		.query(&[("query", format!("to:{email}")), ("limit", "100".into())])
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;
	let messages = value["messages"].as_array().context("Missing messages")?;
	ensure!(
		value["messages_count"].as_u64() == Some(messages.len() as u64),
		"Truncated mail search"
	);
	let mut safe = Vec::new();
	for message in messages {
		ensure!(
			message["To"]
				.as_array()
				.context("Missing recipients")?
				.iter()
				.any(|to| to["Address"] == email),
			"Wrong recipient"
		);
		safe.push(json!({"id":message["ID"], "subject":message["Subject"]}));
	}
	Ok(json!(safe))
}

/// Legacy accounts are untouched until explicitly selected; dry-run never
/// enrolls.
async fn backfill_case(
	config: &ZitadelConfig,
	http: &reqwest::Client,
	mailpit: &str,
	run: &str,
) -> Result<Value> {
	use zitadel_rust_client::v2::users::{
		AddHumanUserRequest, Organization, SetHumanEmail, SetHumanProfile, SetMetadataEntry,
	};
	let errors = SkippedErrors::new();
	let app: Config = serde_json::from_value(json!({"zitadel":config,"sources":{}}))?;
	let sync = Zitadel::new(config.clone(), app.feature_flags.clone(), &errors).await?;
	let email = format!("{run}-legacy@example.test");
	let created = sync
		.zitadel_client
		.create_human_user(
			AddHumanUserRequest::new(
				SetHumanProfile::new("Legacy".into(), "Fixture".into())
					.with_nick_name(hex::encode(&email)),
				SetHumanEmail::new(email.clone()).with_is_verified(false),
			)
			.with_organization(Organization::new().with_org_id(config.organization_id.clone()))
			.with_metadata(vec![SetMetadataEntry::new(
				"localpart".into(),
				format!("{run}-legacy"),
			)]),
		)
		.await?;
	let id = created.user_id().context("Missing legacy ID")?;
	sync.zitadel_client
		.add_user_grant(
			Some(config.organization_id.clone()),
			id,
			config.project_id.clone(),
			None,
			Some(vec!["User".into()]),
		)
		.await?;
	sync.resume_onboarding(id).await?;
	let mut flags = app.feature_flags;
	flags.push(FeatureFlag::DryRun);
	let dry = Zitadel::new(config.clone(), flags, &errors).await?;
	dry.backfill_onboarding(id, false).await?;
	ensure!(
		sync.zitadel_client
			.get_user_metadata(id, &format!("famedly-sync.onboarding.{}", config.project_id), None)
			.await
			.is_err(),
		"Dry-run marked legacy user"
	);
	let before = mail(http, mailpit, &email).await?;
	ensure!(before.as_array().is_some_and(Vec::is_empty), "Unsolicited legacy invitation");
	sync.backfill_onboarding(id, false).await?;
	sync.backfill_onboarding(id, false).await?;
	let start = Instant::now();
	let messages = loop {
		let messages = mail(http, mailpit, &email).await?;
		if start.elapsed() >= Duration::from_secs(12) {
			break messages;
		}
		tokio::time::sleep(Duration::from_millis(250)).await;
	};
	ensure!(
		messages
			.as_array()
			.is_some_and(|m| m.len() == 1 && m[0]["subject"] == "Invitation to ZITADEL"),
		"Backfill must send exactly one invitation"
	);
	Ok(
		json!({"user_id":id,"messages":messages,"passed":true,"dry_run_unmarked":true,"unmarked_not_invited":true}),
	)
}

/// Exercise actual production imports and verify project grant and invitation.
#[tokio::test]
#[ignore = "requires disposable Zitadel and Mailpit; onboarding/run.py runs explicitly"]
async fn onboarding_live() -> Result<()> {
	let config: ZitadelConfig = serde_json::from_slice(&std::fs::read(
		env::var("ONBOARDING_CONFIG").context("Set ONBOARDING_CONFIG")?,
	)?)?;
	let mailpit = env::var("ONBOARDING_MAILPIT").context("Set ONBOARDING_MAILPIT")?;
	let evidence = env::var("ONBOARDING_EVIDENCE").context("Set ONBOARDING_EVIDENCE")?;
	for url in [config.url.clone(), url::Url::parse(&mailpit)?] {
		ensure!(
			url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1")),
			"Local fixtures only"
		);
	}
	let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
	let run = format!("invite-{}", SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos());
	let mut report = json!({"run":run, "cases":[]});
	report["backfill"] = backfill_case(&config, &http, &mailpit, &run).await?;
	for verify in [false, true] {
		let errors = SkippedErrors::new();
		let mut app: Config = serde_json::from_value(json!({"zitadel":config,"sources":{}}))?;
		if verify {
			app.feature_flags.push(FeatureFlag::VerifyEmail);
		}
		let sync = Zitadel::new(config.clone(), app.feature_flags.clone(), &errors).await?;
		let email = format!("{run}-{verify}@example.test");
		let user = User::new(
			"Invite".into(),
			"Regression".into(),
			email.clone(),
			None,
			true,
			Some(email.clone()),
			hex::encode(&email),
			format!("{run}-{verify}"),
		);
		sync.import_user(&user).await?;
		let users: Vec<_> = sync.get_users_by_email(vec![email.clone()])?.try_collect().await?;
		ensure!(users.len() == 1, "Missing/duplicate fixture");
		let id = &users[0].0;
		let csv = tempfile::NamedTempFile::new()?;
		std::fs::write(
			csv.path(),
			format!(
				"email,first_name,last_name,phone,localpart\n{email},Invite,Regression,,{run}-{verify}\n"
			),
		)?;
		let mut full: Config = serde_json::from_value(
			json!({"zitadel":config,"sources":{"csv":{"file_path":csv.path()}}}),
		)?;
		full.feature_flags = app.feature_flags.clone();
		for _ in 0..2 {
			famedly_sync::perform_sync(full.clone()).await?.assert_no_errors()?;
		}
		sync.resume_onboarding(id).await?;
		sync.import_user(&user).await?;
		let methods =
			serde_json::to_value(sync.zitadel_client.list_authentication_method_types(id).await?)?;
		ensure!(
			methods["authMethodTypes"].as_array().is_none_or(Vec::is_empty),
			"Sync must not add passwords or other authentication methods"
		);
		let mut dry: Config = serde_json::from_value(json!({"zitadel":config,"sources":{}}))?;
		dry.feature_flags.push(FeatureFlag::DryRun);
		let dry = Zitadel::new(config.clone(), dry.feature_flags, &errors).await?;
		dry.resume_onboarding(id).await?;
		dry.backfill_onboarding(id, false).await?;
		let state = serde_json::to_value(sync.zitadel_client.get_user_by_id(id).await?)?;
		let granted: Vec<_> = sync.list_users_raw()?.try_collect().await?;
		let grant = granted.iter().any(|u| u.user_id() == Some(id));
		let start = Instant::now();
		let mut messages;
		loop {
			messages = mail(&http, &mailpit, &email).await?;
			if start.elapsed() >= Duration::from_secs(12) {
				break;
			}
			tokio::time::sleep(Duration::from_millis(250)).await;
		}
		let passed = grant
			&& messages
				.as_array()
				.is_some_and(|m| m.len() == 1 && m[0]["subject"] == "Invitation to ZITADEL")
			&& state["user"]["human"]["email"]["isVerified"].as_bool().unwrap_or(false) != verify;
		report["cases"].as_array_mut().context("Missing cases")?.push(json!({"verify_email":verify,"user_id":id,"state":state,"grant":grant,"messages":messages,"window_seconds":start.elapsed().as_secs_f64(),"passed":passed}));
		std::fs::write(&evidence, serde_json::to_vec_pretty(&report)?)?;
		errors.assert_no_errors()?;
	}
	ensure!(
		report["cases"]
			.as_array()
			.context("Missing cases")?
			.iter()
			.all(|case| case["passed"] == true),
		"Onboarding regression; see {evidence}"
	);
	Ok(())
}
