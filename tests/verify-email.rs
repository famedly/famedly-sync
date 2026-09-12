//! Opt-in verification-mail regression against a disposable local Zitadel +
//! Mailpit. See README.md. This does not run the LDAP/full-sync
//! cleanup.

use std::{
	env,
	path::PathBuf,
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

/// Test-only fixture with a unique recipient and stable external ID/localpart.
fn user(run: &str, case: &str, suffix: &str) -> User {
	User::new(
		"Email".into(),
		"Regression".into(),
		recipient(run, case, suffix),
		None,
		true,
		None,
		hex::encode(format!("{run}-{case}")),
		format!("{run}-{case}"),
	)
}

/// Exact Mailpit recipient; reserved domain avoids real-world delivery.
fn recipient(run: &str, case: &str, suffix: &str) -> String {
	format!("{run}-{case}-{suffix}@example.test")
}

/// Read messages for exactly this recipient, never global inbox totals.
async fn messages(http: &reqwest::Client, mailpit: &str, email: &str) -> Result<Value> {
	let mut value: Value = http
		.get(format!("{mailpit}/api/v1/search"))
		.query(&[("query", format!("to:{email}")), ("limit", "100".into())])
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;
	// Mailpit `total` is the whole inbox; `messages_count` is the search total.
	ensure!(value["messages_count"].as_u64().is_some(), "Missing Mailpit search count");
	let listed = value["messages"].as_array().context("Missing Mailpit messages")?;
	ensure!(
		value["messages_count"].as_u64() == Some(listed.len() as u64),
		"Truncated Mailpit result"
	);
	ensure!(
		listed.iter().all(|message| message["To"]
			.as_array()
			.is_some_and(|to| to.iter().any(|to| to["Address"].as_str() == Some(email)))),
		"Recipient mismatch"
	);
	// Keep delivery evidence, not verification codes embedded in body snippets.
	for message in value["messages"].as_array_mut().context("Missing messages")? {
		message.as_object_mut().context("Invalid message")?.remove("Snippet");
	}
	Ok(value)
}

/// Poll for delivery, or observe the full bounded absence window.
async fn observe(
	http: &reqwest::Client,
	mailpit: &str,
	email: &str,
	expected: u64,
	seconds: u64,
) -> Result<Value> {
	let start = Instant::now();
	let deadline = start + Duration::from_secs(seconds);
	let mut polls = 0;
	loop {
		let result = messages(http, mailpit, email).await?;
		polls += 1;
		let count = result["messages_count"].as_u64().context("Missing total")?;
		// Absence observations (expected == 0) end at the first unexpected
		// message; delivery observations wait for every expected message.
		let settled = count >= expected.max(1);
		if settled || Instant::now() >= deadline {
			return Ok(json!({"recipient": email, "expected_count": expected, "count": count,
                "elapsed_seconds": start.elapsed().as_secs_f64(), "polls": polls, "messages": result["messages"],
                "passed": count == expected}));
		}
		tokio::time::sleep_until(
			(Instant::now() + Duration::from_millis(250)).min(deadline).into(),
		)
		.await;
	}
}

/// Build the real sync wrapper, including its normal service-account auth.
async fn sync<'a>(
	config: &ZitadelConfig,
	errors: &'a SkippedErrors,
	flags: &[FeatureFlag],
) -> Result<Zitadel<'a>> {
	let app: Config = serde_json::from_value(json!({"zitadel": config, "sources": {}}))?;
	let mut features = app.feature_flags;
	features.extend_from_slice(flags);
	Zitadel::new(config.clone(), features, errors).await
}

/// Resolve only the exact fixture recipient using the production lookup path.
async fn find_user(sync: &Zitadel<'_>, email: &str) -> Result<String> {
	let deadline = Instant::now() + Duration::from_secs(10);
	loop {
		let users: Vec<_> = sync.get_users_by_email(vec![email.into()])?.try_collect().await?;
		if let Some((id, _)) = users.first() {
			ensure!(users.len() == 1, "Fixture is not unique");
			return Ok(id.clone());
		}
		ensure!(Instant::now() < deadline, "User did not appear: {email}");
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
}

/// Save authoritative user state.
async fn state(sync: &Zitadel<'_>, id: &str) -> Result<Value> {
	Ok(serde_json::to_value(sync.zitadel_client.get_user_by_id(id).await?)?)
}

/// Exercise one real import or email-change operation, then read back its
/// effect.
async fn case(
	config: &ZitadelConfig,
	http: &reqwest::Client,
	mailpit: &str,
	run: &str,
	name: &str,
	seconds: u64,
) -> Result<Value> {
	let errors = SkippedErrors::new();
	let enabled = name.ends_with("on");
	let sso = name.starts_with("sso-");
	let import = name.contains("import");
	let mut flags = if enabled { vec![FeatureFlag::VerifyEmail] } else { vec![] };
	if sso {
		flags.push(FeatureFlag::SsoLogin);
	}
	let target = sync(config, &errors, &flags).await?;
	let initial = user(run, name, "initial");
	let email;
	let id;
	let seed_observation = if !import {
		let seed_flags = if sso { vec![FeatureFlag::SsoLogin] } else { vec![] };
		let seed = sync(config, &errors, &seed_flags).await?;
		seed.import_user(&initial).await?;
		id = find_user(&seed, &recipient(run, name, "initial")).await?;
		let updated = user(run, name, "changed");
		email = recipient(run, name, "changed");
		ensure!(messages(http, mailpit, &email).await?["messages_count"] == 0, "Dirty fixture");
		target.update_user(&id, &initial, &updated).await?;
		observe(http, mailpit, &recipient(run, name, "initial"), u64::from(!sso), seconds).await?
	} else {
		email = recipient(run, name, "initial");
		ensure!(messages(http, mailpit, &email).await?["messages_count"] == 0, "Dirty fixture");
		target.import_user(&initial).await?;
		id = find_user(&target, &email).await?;
		Value::Null
	};
	let persisted = state(&target, &id).await?;
	let expected = if !sso { 1 + u64::from(!import && enabled) } else { u64::from(enabled) };
	let observation = observe(http, mailpit, &email, expected, seconds).await?;
	let granted: Vec<_> = target.list_users_raw()?.try_collect().await?;
	let grant_found = granted.iter().any(|u| u.user_id() == Some(&id));
	errors.assert_no_errors()?;
	let email_state = &persisted["user"]["human"]["email"];
	let subjects: Vec<_> = observation["messages"]
		.as_array()
		.context("Missing messages")?
		.iter()
		.map(|message| message["Subject"].as_str().unwrap_or(""))
		.collect();
	let verification_mail = if !sso {
		subjects.iter().filter(|s| **s == "Invitation to ZITADEL").count() == 1
			&& subjects.iter().filter(|s| **s == "Verify email").count()
				== usize::from(!import && enabled)
	} else {
		subjects.iter().all(|s| *s == "Verify email")
	};
	if sso {
		let links: Vec<_> =
			target.zitadel_client.list_idp_links(&id, None, None)?.try_collect().await?;
		ensure!(
			links.iter().any(|link| link.idp_id() == config.idp_id.as_ref()),
			"Missing SSO link"
		);
		ensure!(
			target
				.zitadel_client
				.get_user_metadata(
					&id,
					&format!("famedly-sync.onboarding.{}", config.project_id),
					None
				)
				.await
				.is_err(),
			"SSO user enrolled in onboarding"
		);
	}
	let passed = observation["passed"] == true
		&& verification_mail
		&& email_state["email"] == email
		&& email_state["isVerified"].as_bool().unwrap_or(false) != enabled
		&& grant_found
		&& (seed_observation.is_null() || seed_observation["passed"] == true);
	Ok(json!({"case":name, "user_id":id, "verify_email":enabled, "user":persisted,
        "grant_found":grant_found, "mail":observation, "seed_mail":seed_observation, "passed":passed}))
}

#[tokio::test]
#[ignore = "requires explicitly configured disposable local Zitadel and Mailpit"]
async fn verify_email_live() -> Result<()> {
	let config_path = env::var("VERIFY_EMAIL_CONFIG").context("Set VERIFY_EMAIL_CONFIG")?;
	let config: ZitadelConfig = serde_json::from_slice(&std::fs::read(config_path)?)?;
	ensure!(
		config.url.scheme() == "http"
			&& matches!(config.url.host_str(), Some("localhost" | "127.0.0.1")),
		"Local HTTP test instance only"
	);
	let mailpit =
		env::var("VERIFY_EMAIL_MAILPIT").unwrap_or_else(|_| "http://localhost:8025".into());
	let mailpit_url = url::Url::parse(&mailpit)?;
	ensure!(
		mailpit_url.scheme() == "http"
			&& matches!(mailpit_url.host_str(), Some("localhost" | "127.0.0.1")),
		"Local Mailpit only"
	);
	let evidence =
		PathBuf::from(env::var("VERIFY_EMAIL_EVIDENCE").context("Set VERIFY_EMAIL_EVIDENCE")?);
	let seconds: u64 =
		env::var("VERIFY_EMAIL_WINDOW_SECONDS").unwrap_or_else(|_| "15".into()).parse()?;
	ensure!((1..=120).contains(&seconds), "Window must be 1..=120 seconds");
	let run = format!("sync-{}", SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos());
	let http = reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?;
	http.get(config.url.join("debug/ready")?).send().await?.error_for_status()?;
	let info: Value =
		http.get(format!("{mailpit}/api/v1/info")).send().await?.error_for_status()?.json().await?;
	let selected = env::var("VERIFY_EMAIL_CASES")
		.unwrap_or_else(|_| "import-off,import-on,update-off,update-on,sso-import-off,sso-import-on,sso-update-off,sso-update-on".into());
	let mut report = json!({"run":run, "zitadel_url":config.url, "mailpit":info, "observation_window_seconds":seconds,
        "scope":"Production Zitadel::import_user/update_user, including first-auth invitations; not LDAP reconciliation", "cases":[]});
	for name in selected.split(',') {
		ensure!(
			[
				"import-off",
				"import-on",
				"update-off",
				"update-on",
				"sso-import-off",
				"sso-import-on",
				"sso-update-off",
				"sso-update-on"
			]
			.contains(&name),
			"Unknown case"
		);
		let result = case(&config, &http, &mailpit, &run, name, seconds).await?;
		report["cases"].as_array_mut().context("Missing cases")?.push(result);
		std::fs::write(&evidence, serde_json::to_vec_pretty(&report)?)?;
	}
	let cases = report["cases"].as_array().context("Missing cases")?;
	ensure!(
		cases.iter().all(|case| case["passed"] == true),
		"Verification regression; evidence: {}",
		evidence.display()
	);
	Ok(())
}
