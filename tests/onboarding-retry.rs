//! Deterministic failure/retry tests against the real sync HTTP boundary.
use std::{
	process::{Command, Stdio},
	sync::{
		Arc, Mutex,
		atomic::{AtomicUsize, Ordering},
	},
};

use anyhow::{Context, Result, ensure};
use base64::{Engine, prelude::BASE64_STANDARD};
use famedly_sync::{Config, FeatureFlag, SkippedErrors, zitadel::Zitadel};
use serde_json::json;
use wiremock::{
	Mock, MockServer, Request, Respond, ResponseTemplate,
	matchers::{method, path},
};

/// Stateful fake server, not a replacement for production invitation logic.
#[derive(Clone)]
struct Api {
	/// Durable metadata visible across reconstructed clients.
	state: Arc<Mutex<Option<String>>>,
	/// Requested invitation count.
	invites: Arc<AtomicUsize>,
	/// First invitation response; subsequent requests succeed.
	status: u16,
}
impl Respond for Api {
	fn respond(&self, request: &Request) -> ResponseTemplate {
		let path = request.url.path();
		if path.ends_with("/invite_code") {
			let n = self.invites.fetch_add(1, Ordering::SeqCst);
			assert_eq!(
				request.body_json::<serde_json::Value>().unwrap_or_default(),
				json!({"sendCode":{}})
			);
			return ResponseTemplate::new(if n == 0 { self.status } else { 200 })
				.set_body_json(json!({}));
		}
		if path.contains("/metadata/") {
			let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
			if request.method == "POST" {
				let body: serde_json::Value = request.body_json().unwrap_or_default();
				*state = body["value"]
					.as_str()
					.and_then(|v| BASE64_STANDARD.decode(v).ok())
					.and_then(|v| String::from_utf8(v).ok());
				return ResponseTemplate::new(200).set_body_json(json!({"details":{}}));
			}
			return match state.as_ref() {
                Some(value) => ResponseTemplate::new(200).set_body_json(json!({"metadata":{"key":"famedly-sync.onboarding.project","value":BASE64_STANDARD.encode(value)}})),
                None => ResponseTemplate::new(404).set_body_json(json!({"code":5})),
            };
		}
		ResponseTemplate::new(200).set_body_json(
			json!({"details":{"totalResult":"1"},"result":[{"id":"grant","roleKeys":["User"]}]}),
		)
	}
}

/// Use an ephemeral RSA key; it never leaves the tempdir or enters logs.
fn key_file(dir: &std::path::Path) -> Result<std::path::PathBuf> {
	let pem = dir.join("test.pem");
	ensure!(
		Command::new("openssl")
			.args(["genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out"])
			.arg(&pem)
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status()?
			.success(),
		"RSA fixture generation failed"
	);
	let key = dir.join("key.json");
	std::fs::write(
		&key,
		serde_json::to_vec(
			&json!({"type":"serviceaccount","keyId":"test","userId":"test","key":std::fs::read_to_string(pem)?}),
		)?,
	)?;
	Ok(key)
}

/// Definite rejection retries after restart; ambiguous outcomes never resend.
#[tokio::test]
async fn invitation_failure_retry_and_no_duplicates() -> Result<()> {
	for status in [200, 403, 500] {
		let server = MockServer::start().await;
		let dir = tempfile::tempdir()?;
		let key = key_file(dir.path())?;
		Mock::given(method("POST"))
			.and(path("/oauth/v2/token"))
			.respond_with(ResponseTemplate::new(200).set_body_json(
				json!({"access_token":"test","token_type":"Bearer","expires_in":4_102_444_800_i64}),
			))
			.mount(&server)
			.await;
		active_human(&server).await;
		let api = Api {
			state: Arc::new(Mutex::new(Some("pending".into()))),
			invites: Arc::new(AtomicUsize::new(0)),
			status,
		};
		Mock::given(path("/management/v1/users/grants/_search"))
			.respond_with(api.clone())
			.mount(&server)
			.await;
		Mock::given(path("/management/v1/users/123/metadata/famedly-sync.onboarding.project"))
			.respond_with(api.clone())
			.mount(&server)
			.await;
		Mock::given(path("/v2/users/123/invite_code"))
			.respond_with(api.clone())
			.mount(&server)
			.await;
		let app: Config = serde_json::from_value(
			json!({"zitadel":{"url":server.uri(),"key_file":key,"organization_id":"org","project_id":"project"},"sources":{}}),
		)?;
		let errors = SkippedErrors::new();
		let sync = Zitadel::new(app.zitadel.clone(), app.feature_flags.clone(), &errors).await?;
		assert_eq!(sync.resume_onboarding("123").await.is_ok(), status == 200);
		let restarted =
			Zitadel::new(app.zitadel.clone(), app.feature_flags.clone(), &errors).await?;
		assert_eq!(restarted.resume_onboarding("123").await.is_ok(), status != 500);
		assert_eq!(restarted.resume_onboarding("123").await.is_ok(), status != 500);
		assert_eq!(api.invites.load(Ordering::SeqCst), if status == 403 { 2 } else { 1 });
		for flag in [FeatureFlag::DryRun, FeatureFlag::SsoLogin] {
			*api.state.lock().map_err(|_| anyhow::anyhow!("State poisoned"))? =
				Some("pending".into());
			let mut flags = app.feature_flags.clone();
			flags.push(flag);
			let sync = Zitadel::new(app.zitadel.clone(), flags, &errors).await?;
			sync.resume_onboarding("123").await?;
		}
		*api.state.lock().map_err(|_| anyhow::anyhow!("State poisoned"))? = None;
		restarted.resume_onboarding("123").await?;
		assert_eq!(api.invites.load(Ordering::SeqCst), if status == 403 { 2 } else { 1 });
		let requests = server.received_requests().await.context("Request capture disabled")?;
		let invite_index = requests
			.iter()
			.position(|r| r.url.path().ends_with("/invite_code"))
			.context("No invitation request")?;
		ensure!(
			requests[..invite_index].iter().any(|r| r.url.path().ends_with("/grants/_search")),
			"Invitation preceded grant lookup"
		);
	}
	Ok(())
}

/// Build authenticated clients without a live service or persistent keys.
async fn fixture() -> Result<(MockServer, tempfile::TempDir, Config)> {
	let server = MockServer::start().await;
	let dir = tempfile::tempdir()?;
	let key = key_file(dir.path())?;
	Mock::given(path("/oauth/v2/token"))
		.respond_with(ResponseTemplate::new(200).set_body_json(
			json!({"access_token":"test","token_type":"Bearer","expires_in":4_102_444_800_i64}),
		))
		.mount(&server)
		.await;
	let app = serde_json::from_value(
		json!({"zitadel":{"url":server.uri(),"key_file":key,"organization_id":"org","project_id":"project"},"sources":{}}),
	)?;
	Ok((server, dir, app))
}

/// Rejected grant creation must leave pending state and never send an invite.
#[tokio::test]
async fn failed_grant_retries_before_invitation() -> Result<()> {
	use std::sync::{
		Arc, Mutex,
		atomic::{AtomicUsize, Ordering},
	};
	let (server, _dir, app) = fixture().await?;
	active_human(&server).await;
	let api = Api {
		state: Arc::new(Mutex::new(Some("pending".into()))),
		invites: Arc::new(AtomicUsize::new(0)),
		status: 200,
	};
	Mock::given(path("/management/v1/users/123/metadata/famedly-sync.onboarding.project"))
		.respond_with(api.clone())
		.mount(&server)
		.await;
	Mock::given(path("/v2/users/123/invite_code")).respond_with(api.clone()).mount(&server).await;
	Mock::given(path("/management/v1/users/grants/_search"))
		.respond_with(
			ResponseTemplate::new(200)
				.set_body_json(json!({"details":{"totalResult":"0"},"result":[]})),
		)
		.mount(&server)
		.await;
	let rejection = Mock::given(path("/management/v1/users/123/grants"))
		.respond_with(
			ResponseTemplate::new(403).set_body_json(json!({"code":7,"message":"denied"})),
		)
		.mount_as_scoped(&server)
		.await;
	let errors = SkippedErrors::new();
	let sync = Zitadel::new(app.zitadel.clone(), app.feature_flags.clone(), &errors).await?;
	assert!(sync.resume_onboarding("123").await.is_err());
	assert_eq!(api.invites.load(Ordering::SeqCst), 0);
	assert_eq!(api.state.lock().expect("state").as_deref(), Some("pending"));
	drop(rejection);
	Mock::given(path("/management/v1/users/123/grants"))
		.respond_with(
			ResponseTemplate::new(200).set_body_json(json!({"userGrantId":"grant","details":{}})),
		)
		.expect(1)
		.mount(&server)
		.await;
	let restarted = Zitadel::new(app.zitadel, app.feature_flags, &errors).await?;
	restarted.resume_onboarding("123").await?;
	restarted.resume_onboarding("123").await?;
	assert_eq!(api.invites.load(Ordering::SeqCst), 1);
	assert_eq!(api.state.lock().expect("state").as_deref(), Some("sent"));
	Ok(())
}

/// Invalid IDs, SSO, foreign organizations and machine accounts fail closed.
#[tokio::test]
async fn backfill_rejects_untrusted_targets_without_writes() -> Result<()> {
	let (server, _dir, app) = fixture().await?;
	let errors = SkippedErrors::new();
	let sync = Zitadel::new(app.zitadel.clone(), app.feature_flags.clone(), &errors).await?;
	for id in ["", " 123", "123 ", "../123", "12/3", "１２３"] {
		assert!(sync.backfill_onboarding(id, true).await.is_err(), "accepted {id:?}");
	}
	let mut flags = app.feature_flags.clone();
	flags.push(FeatureFlag::SsoLogin);
	let sso = Zitadel::new(app.zitadel.clone(), flags, &errors).await?;
	assert!(sso.backfill_onboarding("123", true).await.is_err());
	for user in [
		json!({"details":{"resourceOwner":"foreign"},"human":{}}),
		json!({"details":{"resourceOwner":"org"},"machine":{}}),
	] {
		let guard = Mock::given(path("/v2/users/123"))
			.respond_with(ResponseTemplate::new(200).set_body_json(json!({"user":user})))
			.mount_as_scoped(&server)
			.await;
		assert!(sync.backfill_onboarding("123", false).await.is_err());
		drop(guard);
	}
	let requests = server.received_requests().await.expect("capture");
	assert!(requests.iter().all(|r| r.method == "GET" || r.url.path() == "/oauth/v2/token"));
	Ok(())
}

/// Existing authentication methods, missing ownership metadata, and unknown
/// states cannot be opted into invitations; ambiguous state requires explicit
/// retry.
#[tokio::test]
async fn backfill_rejects_existing_authentication_and_unknown_state() -> Result<()> {
	for (metadata_status, methods, state) in [
		(404, json!([]), "pending"),
		(200, json!(["AUTHENTICATION_METHOD_TYPE_PASSWORD"]), "pending"),
		(200, json!([]), "invalid"),
		(200, json!([]), "sending"),
	] {
		let (server, _dir, app) = fixture().await?;
		Mock::given(path("/v2/users/123"))
			.respond_with(ResponseTemplate::new(200).set_body_json(
				json!({"user":{"state":"USER_STATE_ACTIVE","details":{"resourceOwner":"org"},"human":{}}}),
			))
			.mount(&server)
			.await;
		Mock::given(path("/management/v1/users/123/metadata/localpart"))
			.respond_with(
				ResponseTemplate::new(metadata_status)
					.set_body_json(json!({"metadata":{"key":"localpart","value":"dGVzdA=="}})),
			)
			.mount(&server)
			.await;
		Mock::given(path("/v2/users/123/authentication_methods"))
			.respond_with(
				ResponseTemplate::new(200).set_body_json(json!({"authMethodTypes":methods})),
			)
			.mount(&server)
			.await;
		Mock::given(method("GET"))
			.and(path("/management/v1/users/123/metadata/famedly-sync.onboarding.project"))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(json!({"metadata":{"value":BASE64_STANDARD.encode(state)}})),
			)
			.mount(&server)
			.await;
		let errors = SkippedErrors::new();
		let sync = Zitadel::new(app.zitadel, app.feature_flags, &errors).await?;
		let error = sync.backfill_onboarding("123", false).await.expect_err("must reject");
		if metadata_status == 200 {
			assert!(
				error.to_string().contains(if methods != serde_json::Value::Array(vec![]) {
					"authentication methods"
				} else if state == "sending" {
					"--retry-uncertain"
				} else {
					"Unknown onboarding state"
				}),
				"{error:#}"
			);
		}
		assert!(
			server
				.received_requests()
				.await
				.expect("capture")
				.iter()
				.all(|r| r.method == "GET" || r.url.path() == "/oauth/v2/token")
		);
	}
	Ok(())
}

/// Valid user boundary for invitation retry scenarios.
async fn active_human(server: &MockServer) {
	Mock::given(path("/v2/users/123"))
		.respond_with(ResponseTemplate::new(200).set_body_json(
			json!({"user":{"state":"USER_STATE_ACTIVE","details":{"resourceOwner":"org"},"human":{}}}),
		))
		.mount(server)
		.await;
	Mock::given(path("/v2/users/123/authentication_methods"))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({"authMethodTypes":[]})))
		.mount(server)
		.await;
}
