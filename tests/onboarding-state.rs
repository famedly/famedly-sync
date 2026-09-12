//! Onboarding state regressions at the production HTTP boundary.
#![allow(clippy::expect_used, clippy::print_stdout)]
use std::{
	process::{Command, Stdio},
	sync::{Arc, Mutex},
};

use anyhow::{Context, Result, ensure};
use base64::{Engine, prelude::BASE64_STANDARD};
use famedly_sync::{Config, SkippedErrors, user::User, zitadel::Zitadel};
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::any};

#[derive(Clone)]
struct Api(Arc<Mutex<State>>);
struct State {
	marker: Option<String>,
	user_state: String,
	methods: Value,
	email: String,
	invite_status: u16,
	update_status: u16,
	phone_rejected: bool,
	import_conflict: bool,
	grant_missing: bool,
	grants: usize,
	invitations: Vec<String>,
	writes: usize,
}
/// v2 user endpoint responses, including the interrupted-import retry
/// sequence: a duplicate create followed by the email lookup of the account
/// that was created before the interruption.
fn users_endpoint(r: &Request, state: &mut State) -> Option<ResponseTemplate> {
	let ok = |v| ResponseTemplate::new(200).set_body_json(v);
	if r.url.path() == "/v2/users/human" && r.method == "POST" {
		if state.import_conflict {
			return Some(ResponseTemplate::new(400).set_body_string("V3-DKcYh"));
		}
		return Some(ok(json!({"userId":"123","details":{}})));
	}
	if r.url.path() == "/v2/users" || r.url.path().ends_with("/users/_search") {
		if state.import_conflict {
			return Some(ok(json!({
				"details":{"totalResult":"1"},
				"result":[{
					"userId":"123",
					"state":"USER_STATE_INITIAL",
					"details":{"resourceOwner":"org"},
					"human":{
						"profile":{"givenName":"Test","familyName":"User","nickName":"1234"},
						"email":{"email":"old@example.test"}
					}
				}]
			})));
		}
		return Some(ok(json!({"details":{"totalResult":"1"},"result":[{"userId":"123"}]})));
	}
	None
}
impl Respond for Api {
	fn respond(&self, r: &Request) -> ResponseTemplate {
		let mut s = self.0.lock().expect("fixture lock");
		let p = r.url.path();
		let ok = |v| ResponseTemplate::new(200).set_body_json(v);
		if p == "/oauth/v2/token" {
			return ok(
				json!({"access_token":"test","token_type":"Bearer","expires_in":4_102_444_800_i64}),
			);
		}
		if r.method != "GET" && !p.ends_with("_search") {
			s.writes += 1;
		}
		if p.ends_with("/invite_code") {
			let email = s.email.clone();
			s.invitations.push(email);
			return ResponseTemplate::new(s.invite_status).set_body_string("not JSON");
		}
		if p.ends_with("/metadata/localpart") {
			return ok(json!({"metadata":{"key":"localpart","value":"dGVzdA=="}}));
		}
		if p.contains("/metadata/") {
			let onboarding = p.ends_with("famedly-sync.onboarding.project");
			if r.method == "POST" {
				let b: Value = r.body_json().expect("metadata");
				s.marker = Some(
					String::from_utf8(
						BASE64_STANDARD
							.decode(b["value"].as_str().expect("value"))
							.expect("base64"),
					)
					.expect("utf8"),
				);
				return ok(json!({"details":{}}));
			}
			if !onboarding {
				return ResponseTemplate::new(404).set_body_string("not found");
			}
			return match &s.marker {
				Some(v) => ok(json!({"metadata":{"value":BASE64_STANDARD.encode(v)}})),
				None => ResponseTemplate::new(404).set_body_string("not found"),
			};
		}
		if p.ends_with("/authentication_methods") {
			return ok(s.methods.clone());
		}
		if p == "/v2/users/123" && r.method == "GET" {
			return ok(
				json!({"user":{"userId":"123","state":s.user_state,"details":{"resourceOwner":"org"},"human":{"email":{"email":s.email}}}}),
			);
		}
		if p == "/v2/users/human/123" {
			if s.update_status != 200 {
				return ResponseTemplate::new(s.update_status)
					.set_body_json(json!({"code":7,"message":"denied"}));
			}
			let b: Value = r.body_json().expect("update");
			// Zitadel rejects an invalid phone in the request; the sync's
			// documented fallback retries the update without it.
			if s.phone_rejected && b["phone"].is_object() {
				return ResponseTemplate::new(400).set_body_string("PHONE-so0wa");
			}
			if let Some(email) = b["email"]["email"].as_str() {
				s.email = email.into();
			}
			return ok(json!({"details":{}}));
		}
		if p == "/v2/users/123/phone" && r.method == "DELETE" {
			// Removing a phone that was never stored is tolerated.
			if s.phone_rejected {
				return ResponseTemplate::new(400).set_body_string("COMMAND-ieJ2e");
			}
			return ok(json!({"details":{}}));
		}
		if p.ends_with("/grants/_search") {
			if s.grant_missing {
				return ok(json!({"details":{"totalResult":"0"},"result":[]}));
			}
			return ok(
				json!({"details":{"totalResult":"1"},"result":[{"id":"grant","roleKeys":["User"]}]}),
			);
		}
		if p == "/management/v1/users/123/grants" && r.method == "POST" {
			s.grants += 1;
			return ok(json!({"userGrantId":"grant","details":{}}));
		}
		if let Some(response) = users_endpoint(r, &mut s) {
			return response;
		}
		ResponseTemplate::new(404).set_body_json(json!({"code":5}))
	}
}
async fn fixture(marker: &str) -> Result<(MockServer, tempfile::TempDir, Config, Api)> {
	let server = MockServer::start().await;
	let dir = tempfile::tempdir()?;
	let pem = dir.path().join("test.pem");
	ensure!(
		Command::new("openssl")
			.args(["genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out"])
			.arg(&pem)
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status()?
			.success(),
		"key generation"
	);
	let key = dir.path().join("key.json");
	std::fs::write(
		&key,
		serde_json::to_vec(
			&json!({"type":"serviceaccount","keyId":"test","userId":"test","key":std::fs::read_to_string(pem)?}),
		)?,
	)?;
	let app = serde_json::from_value(
		json!({"zitadel":{"url":server.uri(),"key_file":key,"organization_id":"org","project_id":"project"},"sources":{}}),
	)?;
	let api = Api(Arc::new(Mutex::new(State {
		marker: Some(marker.into()),
		user_state: "USER_STATE_ACTIVE".into(),
		methods: json!({}),
		email: "old@example.test".into(),
		invite_status: 200,
		update_status: 200,
		phone_rejected: false,
		import_conflict: false,
		grant_missing: false,
		grants: 0,
		invitations: vec![],
		writes: 0,
	})));
	Mock::given(any()).respond_with(api.clone()).mount(&server).await;
	Ok((server, dir, app, api))
}
fn user(email: &str) -> User {
	User::new(
		"Test".into(),
		"User".into(),
		email.into(),
		None,
		true,
		None,
		"1234".into(),
		"test".into(),
	)
}

#[tokio::test]
async fn backfill_rejects_inactive_accounts_before_writes() -> Result<()> {
	for state in ["USER_STATE_INACTIVE", "USER_STATE_LOCKED", "USER_STATE_UNSPECIFIED"] {
		let (_server, _dir, app, api) = fixture("pending").await?;
		api.0.lock().expect("state").user_state = state.into();
		let errors = SkippedErrors::new();
		let sync = Zitadel::new(app.zitadel, app.feature_flags, &errors).await?;
		let error = sync.backfill_onboarding("123", false).await.expect_err("inactive must reject");
		assert!(error.to_string().contains("active or initial"), "{error:#}");
		assert_eq!(api.0.lock().expect("state").writes, 0);
	}
	Ok(())
}

#[tokio::test]
async fn malformed_authentication_methods_fail_closed() -> Result<()> {
	for methods in [
		json!({"authMethodTypes":null}),
		json!({"authMethodTypes":{}}),
		json!({"authMethodTypes":"bad"}),
		json!([]),
	] {
		let (_server, _dir, app, api) = fixture("pending").await?;
		api.0.lock().expect("state").methods = methods;
		let errors = SkippedErrors::new();
		let sync = Zitadel::new(app.zitadel, app.feature_flags, &errors).await?;
		assert!(sync.backfill_onboarding("123", false).await.is_err());
		assert_eq!(api.0.lock().expect("state").writes, 0);
	}
	Ok(())
}

#[tokio::test]
async fn changed_email_replaces_invitation_once_after_success() -> Result<()> {
	let (_server, _dir, app, api) = fixture("sent").await?;
	let errors = SkippedErrors::new();
	let sync = Zitadel::new(app.zitadel.clone(), app.feature_flags.clone(), &errors).await?;
	sync.update_user("123", &user("old@example.test"), &user("new@example.test")).await?;
	let restarted = Zitadel::new(app.zitadel, app.feature_flags, &errors).await?;
	restarted.resume_onboarding("123").await?;
	assert_eq!(api.0.lock().expect("state").invitations, ["new@example.test"]);
	assert_eq!(api.0.lock().expect("state").marker.as_deref(), Some("sent"));
	Ok(())
}

#[tokio::test]
async fn resume_respects_administrative_lock() -> Result<()> {
	let (_server, _dir, app, api) = fixture("pending").await?;
	api.0.lock().expect("state").user_state = "USER_STATE_LOCKED".into();
	let errors = SkippedErrors::new();
	let sync = Zitadel::new(app.zitadel, app.feature_flags, &errors).await?;
	assert!(sync.resume_onboarding("123").await.is_err());
	assert_eq!(api.0.lock().expect("state").writes, 0);
	Ok(())
}

/// A user who completed first authentication while a retry was queued must be
/// marked done instead of failing every later sync.
#[tokio::test]
async fn completed_authentication_marks_pending_retry_sent() -> Result<()> {
	let (_server, _dir, app, api) = fixture("pending").await?;
	api.0.lock().expect("state").methods =
		json!({"authMethodTypes":["AUTHENTICATION_METHOD_TYPE_PASSWORD"]});
	let errors = SkippedErrors::new();
	let sync = Zitadel::new(app.zitadel.clone(), app.feature_flags.clone(), &errors).await?;
	sync.resume_onboarding("123").await?;
	assert_eq!(api.0.lock().expect("state").marker.as_deref(), Some("sent"));
	assert!(api.0.lock().expect("state").invitations.is_empty());
	let restarted = Zitadel::new(app.zitadel, app.feature_flags, &errors).await?;
	restarted.resume_onboarding("123").await?;
	assert!(api.0.lock().expect("state").invitations.is_empty());
	Ok(())
}

#[tokio::test]
async fn failed_email_update_never_invites_stale_address() -> Result<()> {
	let (_server, _dir, app, api) = fixture("sent").await?;
	let errors = SkippedErrors::new();
	let sync = Zitadel::new(app.zitadel.clone(), app.feature_flags.clone(), &errors).await?;
	api.0.lock().expect("state").update_status = 403;
	assert!(
		sync.update_user("123", &user("old@example.test"), &user("new@example.test"))
			.await
			.is_err()
	);
	let restarted = Zitadel::new(app.zitadel, app.feature_flags, &errors).await?;
	assert!(restarted.resume_onboarding("123").await.is_err());
	assert!(api.0.lock().expect("state").invitations.is_empty());
	api.0.lock().expect("state").update_status = 200;
	restarted.update_user("123", &user("old@example.test"), &user("new@example.test")).await?;
	restarted.resume_onboarding("123").await?;
	assert_eq!(api.0.lock().expect("state").invitations, ["new@example.test"]);
	Ok(())
}

/// Uses the same disposable fixture environment as onboarding.rs.
#[tokio::test]
#[ignore = "requires disposable local Zitadel and Mailpit"]
async fn onboarding_state_live() -> Result<()> {
	use std::time::{Duration, SystemTime, UNIX_EPOCH};

	use famedly_sync::zitadel::ZitadelConfig;
	use futures::TryStreamExt;
	let config: ZitadelConfig =
		serde_json::from_slice(&std::fs::read(std::env::var("ONBOARDING_CONFIG")?)?)?;
	let mailpit = std::env::var("ONBOARDING_MAILPIT")?;
	for url in [config.url.clone(), url::Url::parse(&mailpit)?] {
		ensure!(
			url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1")),
			"Local fixtures only"
		);
	}
	let app: Config = serde_json::from_value(json!({"zitadel":config,"sources":{}}))?;
	let errors = SkippedErrors::new();
	let sync = Zitadel::new(config.clone(), app.feature_flags.clone(), &errors).await?;
	let run = format!("state-{}", SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos());
	let old_email = format!("{run}-old@example.test");
	let new_email = format!("{run}-new@example.test");
	let old = User::new(
		"State".into(),
		"Fixture".into(),
		old_email.clone(),
		None,
		true,
		None,
		hex::encode(&run),
		run.clone(),
	);
	let new = User::new(
		"State".into(),
		"Fixture".into(),
		new_email.clone(),
		None,
		true,
		None,
		hex::encode(&run),
		run.clone(),
	);
	sync.import_user(&old).await?;
	let users: Vec<_> = sync.get_users_by_email(vec![old_email.clone()])?.try_collect().await?;
	ensure!(users.len() == 1, "Missing or duplicate user");
	let id = &users[0].0;
	sync.update_user(id, &old, &new).await?;
	let restarted = Zitadel::new(config.clone(), app.feature_flags, &errors).await?;
	restarted.resume_onboarding(id).await?;
	restarted.update_user(id, &new, &new).await?;
	let actual = serde_json::to_value(sync.zitadel_client.get_user_by_id(id).await?)?;
	ensure!(actual["user"]["human"]["email"]["email"] == new_email, "Email readback failed");
	let metadata = serde_json::to_value(
		sync.zitadel_client
			.get_user_metadata(id, &format!("famedly-sync.onboarding.{}", config.project_id), None)
			.await?,
	)?;
	ensure!(metadata["metadata"]["value"] == BASE64_STANDARD.encode("sent"), "Marker not sent");
	let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
	// Observe the entire delivery window, rather than stopping at first mail.
	let mut report = json!({"user_id":id});
	for _ in 0..48 {
		for (label, email) in [("old", &old_email), ("new", &new_email)] {
			let mail: Value = http
				.get(format!("{mailpit}/api/v1/search"))
				.query(&[("query", format!("to:{email}")), ("limit", "100".into())])
				.send()
				.await?
				.error_for_status()?
				.json()
				.await?;
			let messages = mail["messages"].as_array().context("Missing messages")?;
			ensure!(
				mail["messages_count"].as_u64() == Some(messages.len() as u64),
				"Truncated mail search"
			);
			report[label] =
				json!(messages.iter().filter(|m| m["Subject"] == "Invitation to ZITADEL").count());
		}
		tokio::time::sleep(Duration::from_millis(250)).await;
	}
	ensure!(
		report["old"] == 1 && report["new"] == 1,
		"Expected one invitation per address: {report}"
	);
	println!("onboarding state live evidence: {report}");
	Ok(())
}

#[tokio::test]
async fn profile_only_migration_does_not_send_pending_invitation() -> Result<()> {
	let (_server, _dir, app, api) = fixture("pending").await?;
	let errors = SkippedErrors::new();
	let sync = Zitadel::new(app.zitadel, app.feature_flags, &errors).await?;
	let migrated = User::new(
		"Test".into(),
		"User".into(),
		"old@example.test".into(),
		None,
		true,
		None,
		"5678".into(),
		"test".into(),
	);
	sync.update_user("123", &user("old@example.test"), &migrated).await?;
	assert!(api.0.lock().expect("state").invitations.is_empty());
	Ok(())
}

/// A phone Zitadel rejects keeps the user on the update path on every later
/// sync; a queued invitation must still be retried there.
#[tokio::test]
async fn rejected_phone_update_retries_pending_invitation() -> Result<()> {
	let (_server, _dir, app, api) = fixture("pending").await?;
	api.0.lock().expect("state").phone_rejected = true;
	let errors = SkippedErrors::new();
	let sync = Zitadel::new(app.zitadel, app.feature_flags, &errors).await?;
	let source = User::new(
		"Test".into(),
		"User".into(),
		"old@example.test".into(),
		Some("abc".into()),
		true,
		None,
		"1234".into(),
		"test".into(),
	);
	sync.update_user("123", &user("old@example.test"), &source).await?;
	assert_eq!(api.0.lock().expect("state").invitations, ["old@example.test"]);
	assert_eq!(api.0.lock().expect("state").marker.as_deref(), Some("sent"));
	Ok(())
}

/// A refused localpart change keeps the user on the update path on every
/// later sync; a queued invitation must still be retried there.
#[tokio::test]
async fn refused_localpart_change_retries_pending_invitation() -> Result<()> {
	let (_server, _dir, app, api) = fixture("pending").await?;
	let errors = SkippedErrors::new();
	let sync = Zitadel::new(app.zitadel, app.feature_flags, &errors).await?;
	let source = User::new(
		"Test".into(),
		"User".into(),
		"old@example.test".into(),
		None,
		true,
		None,
		"1234".into(),
		"other".into(),
	);
	sync.update_user("123", &user("old@example.test"), &source).await?;
	assert_eq!(api.0.lock().expect("state").invitations, ["old@example.test"]);
	assert_eq!(api.0.lock().expect("state").marker.as_deref(), Some("sent"));
	Ok(())
}

/// An import interrupted between user creation and grant creation leaves a
/// grant-less account the sync cannot see; the retry must repair the grant
/// and send the queued invitation.
#[tokio::test]
async fn import_retry_repairs_missing_grant_and_invites() -> Result<()> {
	let (_server, _dir, app, api) = fixture("pending").await?;
	{
		let mut s = api.0.lock().expect("state");
		s.import_conflict = true;
		s.grant_missing = true;
	}
	let errors = SkippedErrors::new();
	let sync = Zitadel::new(app.zitadel, app.feature_flags, &errors).await?;
	sync.import_user(&user("old@example.test")).await?;
	let s = api.0.lock().expect("state");
	assert_eq!(s.grants, 1);
	assert_eq!(s.invitations, ["old@example.test"]);
	assert_eq!(s.marker.as_deref(), Some("sent"));
	drop(s);
	Ok(())
}

#[tokio::test]
async fn email_recovery_is_durable_and_does_not_adopt_authenticated_users() -> Result<()> {
	for methods in [
		json!({}),
		json!({"authMethodTypes":[]}),
		json!({"authMethodTypes":["AUTHENTICATION_METHOD_TYPE_PASSWORD"]}),
	] {
		let (_server, _dir, app, api) = fixture("sent").await?;
		api.0.lock().expect("state").methods = methods.clone();
		let errors = SkippedErrors::new();
		let sync = Zitadel::new(app.zitadel, app.feature_flags, &errors).await?;
		if methods["authMethodTypes"].as_array().is_some_and(|a| !a.is_empty()) {
			sync.update_user("123", &user("old@example.test"), &user("new@example.test")).await?;
			assert!(api.0.lock().expect("state").invitations.is_empty());
		} else {
			// Simulate restart after the update committed but before mail.
			{
				let mut s = api.0.lock().expect("state");
				s.marker = Some(format!("email:{}", BASE64_STANDARD.encode("new@example.test")));
				s.email = "new@example.test".into();
				s.invite_status = 403;
			}
			assert!(sync.resume_onboarding("123").await.is_err());
			assert!(
				api.0
					.lock()
					.expect("state")
					.marker
					.as_deref()
					.is_some_and(|s| s.starts_with("email:"))
			);
			api.0.lock().expect("state").invite_status = 200;
			sync.resume_onboarding("123").await?;
			sync.resume_onboarding("123").await?;
			assert_eq!(
				api.0.lock().expect("state").invitations,
				["new@example.test", "new@example.test"]
			);
		}
	}
	Ok(())
}

#[tokio::test]
async fn ambiguous_email_invitation_is_not_replayed() -> Result<()> {
	let (_server, _dir, app, api) = fixture("sent").await?;
	let errors = SkippedErrors::new();
	let sync = Zitadel::new(app.zitadel.clone(), app.feature_flags.clone(), &errors).await?;
	api.0.lock().expect("state").invite_status = 500;
	assert!(
		sync.update_user("123", &user("old@example.test"), &user("new@example.test"))
			.await
			.is_err()
	);
	let restarted = Zitadel::new(app.zitadel, app.feature_flags, &errors).await?;
	assert!(restarted.resume_onboarding("123").await.is_err());
	assert_eq!(api.0.lock().expect("state").marker.as_deref(), Some("sending"));
	assert_eq!(api.0.lock().expect("state").invitations, ["new@example.test"]);
	Ok(())
}

#[tokio::test]
async fn unmarked_and_sso_email_updates_do_not_enroll() -> Result<()> {
	for sso in [false, true] {
		let (_server, _dir, mut app, api) = fixture("sent").await?;
		if sso {
			app.feature_flags.push(famedly_sync::FeatureFlag::SsoLogin);
		} else {
			api.0.lock().expect("state").marker = None;
		}
		let errors = SkippedErrors::new();
		let sync = Zitadel::new(app.zitadel, app.feature_flags, &errors).await?;
		sync.update_user("123", &user("old@example.test"), &user("new@example.test")).await?;
		assert!(api.0.lock().expect("state").invitations.is_empty());
	}
	Ok(())
}

#[tokio::test]
async fn non_json_invitation_status_survives_body_decoding() -> Result<()> {
	let (_server, _dir, app, api) = fixture("pending").await?;
	let errors = SkippedErrors::new();
	let sync = Zitadel::new(app.zitadel.clone(), app.feature_flags.clone(), &errors).await?;
	api.0.lock().expect("state").invite_status = 403;
	let error = sync.resume_onboarding("123").await.expect_err("rejected");
	assert!(error.to_string().contains("403"), "{error:#}");
	assert_eq!(api.0.lock().expect("state").marker.as_deref(), Some("pending"));
	api.0.lock().expect("state").invite_status = 200;
	let restarted = Zitadel::new(app.zitadel, app.feature_flags, &errors).await?;
	restarted.resume_onboarding("123").await?;
	restarted.resume_onboarding("123").await?;
	assert_eq!(api.0.lock().expect("state").marker.as_deref(), Some("sent"));
	assert_eq!(api.0.lock().expect("state").invitations.len(), 2);
	Ok(())
}
