//! E2E tests for the id installation script

#![cfg(test)]

use test_log::test;

mod common;

use common::{Ldap, cleanup_test_users, ldap_config};
use famedly_sync::{SkippedErrors, link_user_ids};
use zitadel_rust_client::v2::{
	Zitadel,
	users::{AddHumanUserRequest, Organization, SetHumanEmail, SetHumanProfile},
};

/// Create a verified Zitadel human user (optionally with a nickname / external
/// ID) and grant them the `User` role, returning the new Zitadel user ID.
async fn seed_zitadel_user(
	zitadel: &Zitadel,
	org_id: &str,
	project_id: &str,
	email: &str,
	nick_name: Option<&str>,
) -> String {
	let mut profile = SetHumanProfile::new("Max".to_owned(), "Mustermann".to_owned())
		.with_display_name("Mustermann, Max".to_owned());
	if let Some(nick) = nick_name {
		profile = profile.with_nick_name(nick.to_owned());
	}

	let user = AddHumanUserRequest::new(
		profile,
		SetHumanEmail::new(email.to_owned()).with_is_verified(true),
	)
	.with_organization(Organization::new().with_org_id(org_id.to_owned()));

	let uid = zitadel
		.create_human_user(user)
		.await
		.expect("user must be created")
		.user_id()
		.expect("user must have an ID")
		.clone();

	zitadel
		.add_user_grant(
			Some(org_id.to_owned()),
			&uid,
			project_id.to_owned(),
			None,
			Some(vec!["User".to_owned()]),
		)
		.await
		.expect("user grant must be added");

	uid
}

/// Fetch a user's nickname (external ID) by Zitadel ID, if set.
async fn user_nick_name(zitadel: &Zitadel, uid: &str) -> Option<String> {
	zitadel
		.get_user_by_id(uid)
		.await
		.expect("user must exist")
		.user()
		.and_then(|u| u.human())
		.and_then(|h| h.profile())
		.and_then(|p| p.nick_name())
		.cloned()
}

/// Assert that the missing ID sync works
#[test(tokio::test)]
async fn test_e2e_install_missing_ids() {
	let skipped_errors = SkippedErrors::new();
	let config = ldap_config().await;
	cleanup_test_users(config).await;

	let mut ldap = Ldap::new().await;
	let zitadel = Zitadel::new(config.zitadel.url.clone(), config.zitadel.key_file.clone(), None)
		.await
		.expect("Zitadel connection must succeed");

	let user_with_missing_id = AddHumanUserRequest::new(
		SetHumanProfile::new("Max".to_owned(), "Mustermann".to_owned())
			// Deliberately don't set a nickname (external UID)
			.with_display_name("Mustermann, Max".to_owned()),
		SetHumanEmail::new("max.mustermann5@domain.invalid".to_owned()).with_is_verified(true),
	)
	.with_organization(Organization::new().with_org_id(config.zitadel.organization_id.clone()));

	let uid = zitadel
		.create_human_user(user_with_missing_id)
		.await
		.expect("user must be created")
		.user_id()
		.expect("user must have an ID")
		.clone();

	zitadel
		.add_user_grant(
			Some(config.zitadel.organization_id.clone()),
			&uid,
			config.zitadel.project_id.clone(),
			None,
			Some(vec!["User".to_owned()]),
		)
		.await
		.expect("user grant must be added");

	ldap.create_user(
		"Max",
		"Mustermann",
		"Mustermann, Max",
		"max.mustermann5@domain.invalid",
		None,
		"max.mustermann5",
		false,
	)
	.await;

	link_user_ids(config.clone(), &skipped_errors).await.expect("Linking should succeed");

	let nick = zitadel
		.get_user_by_id(&uid)
		.await
		.expect("user must exist")
		.user()
		.and_then(|u| u.human())
		.and_then(|h| h.profile())
		.and_then(|p| p.nick_name())
		.expect("Nickname must be set")
		.clone();

	assert_eq!(
		String::from_utf8_lossy(&hex::decode(nick).expect("must decode")),
		"max.mustermann5"
	);
}

/// Assert that the missing ID sync works, even if we encounter
/// problematic data
#[test(tokio::test)]
async fn test_e2e_install_ids_with_errors() {
	let skipped_errors = SkippedErrors::new();
	let config = ldap_config().await;
	cleanup_test_users(config).await;

	let mut ldap = Ldap::new().await;
	let zitadel = Zitadel::new(config.zitadel.url.clone(), config.zitadel.key_file.clone(), None)
		.await
		.expect("Zitadel connection must succeed");

	let org_id = config.zitadel.organization_id.clone();
	let project_id = config.zitadel.project_id.clone();

	// Just a normal user without an ID
	let missing_id_uid =
		seed_zitadel_user(&zitadel, &org_id, &project_id, "max.mustermann@domain.invalid", None)
			.await;
	ldap.create_user(
		"Max",
		"Mustermann",
		"Mustermann, Max",
		"max.mustermann@domain.invalid",
		None,
		"max.mustermann",
		false,
	)
	.await;

	// A user who actually does have an ID and doesn't need to be changed
	let extant_id_uid = seed_zitadel_user(
		&zitadel,
		&org_id,
		&project_id,
		"max.mustermann2@domain.invalid",
		Some(&hex::encode("max.mustermann2".as_bytes())),
	)
	.await;
	ldap.create_user(
		"Max",
		"Mustermann",
		"Mustermann, Max",
		"max.mustermann2@domain.invalid",
		None,
		"max.mustermann2",
		false,
	)
	.await;

	// A user who does not have a corresponding LDAP user
	let missing_ldap_uid =
		seed_zitadel_user(&zitadel, &org_id, &project_id, "max.mustermann3@domain.invalid", None)
			.await;

	// A user with an existing link that isn't actually correct
	let extant_broken_uid = seed_zitadel_user(
		&zitadel,
		&org_id,
		&project_id,
		"max.mustermann4@domain.invalid",
		Some(&hex::encode("max.mustermann4".as_bytes())),
	)
	.await;
	ldap.create_user(
		"Max",
		"Mustermann",
		"Mustermann, Max",
		"max.mustermann4@domain.invalid",
		None,
		"invalid",
		false,
	)
	.await;

	link_user_ids(config.clone(), &skipped_errors).await.expect("Linking should succeed");

	let nick = user_nick_name(&zitadel, &missing_id_uid).await.expect("Nickname must be set");
	assert_eq!(String::from_utf8_lossy(&hex::decode(nick).expect("must decode")), "max.mustermann");

	let nick = user_nick_name(&zitadel, &extant_id_uid).await.expect("Nickname must be set");
	assert_eq!(
		String::from_utf8_lossy(&hex::decode(nick).expect("must decode")),
		"max.mustermann2"
	);

	// Unfortunately, Zitadel gives an empty string for a missing field.
	let nick = user_nick_name(&zitadel, &missing_ldap_uid).await;
	assert_eq!(Some("".to_owned()), nick);

	let nick = user_nick_name(&zitadel, &extant_broken_uid).await.expect("Nickname must be set");
	// Assert this doesn't change
	assert_eq!(
		String::from_utf8_lossy(&hex::decode(&nick).expect("must decode")),
		"max.mustermann4"
	);
	assert_ne!(String::from_utf8_lossy(&hex::decode(&nick).expect("must decode")), "invalid");
}
