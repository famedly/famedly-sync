//! E2E tests for the id installation script

use test_log::test;

mod common;

use common::{Ldap, cleanup_test_users, ldap_config};
use famedly_sync::{SkippedErrors, link_user_ids};
use zitadel_rust_client::v2::{
	Zitadel,
	users::{AddHumanUserRequest, Organization, SetHumanEmail, SetHumanProfile},
};

/// Assert that the missing ID sync works
#[test(tokio::test)]
async fn test_e2e_install_missing_ids() {
	let skipped_errors = SkippedErrors::new();
	let config = ldap_config().await;
	cleanup_test_users(config).await;

	let mut ldap = Ldap::new().await;
	let zitadel = Zitadel::new(config.zitadel.url.clone(), config.zitadel.key_file.clone())
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
	let zitadel = Zitadel::new(config.zitadel.url.clone(), config.zitadel.key_file.clone())
		.await
		.expect("Zitadel connection must succeed");

	// Just a normal user without an ID
	let missing_id_uid = {
		let user = AddHumanUserRequest::new(
			SetHumanProfile::new("Max".to_owned(), "Mustermann".to_owned())
				// Deliberately don't set a nickname (external UID)
				.with_display_name("Mustermann, Max".to_owned()),
			SetHumanEmail::new("max.mustermann@domain.invalid".to_owned()).with_is_verified(true),
		)
		.with_organization(Organization::new().with_org_id(config.zitadel.organization_id.clone()));

		let uid = zitadel
			.create_human_user(user)
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
			"max.mustermann@domain.invalid",
			None,
			"max.mustermann",
			false,
		)
		.await;

		uid
	};

	// A user who actually does have an ID and doesn't need to be
	// changed
	let extant_id_uid = {
		let id = "max.mustermann2";

		let user = AddHumanUserRequest::new(
			SetHumanProfile::new("Max".to_owned(), "Mustermann".to_owned())
				.with_nick_name(hex::encode(id.as_bytes()))
				.with_display_name("Mustermann, Max".to_owned()),
			SetHumanEmail::new("max.mustermann2@domain.invalid".to_owned()).with_is_verified(true),
		)
		.with_organization(Organization::new().with_org_id(config.zitadel.organization_id.clone()));

		let uid = zitadel
			.create_human_user(user)
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
			"max.mustermann2@domain.invalid",
			None,
			id,
			false,
		)
		.await;

		uid
	};

	// A user who does not have a corresponding LDAP user
	let missing_ldap_uid = {
		let user = AddHumanUserRequest::new(
			SetHumanProfile::new("Max".to_owned(), "Mustermann".to_owned())
				// Deliberately don't set a nickname (external UID)
				.with_display_name("Mustermann, Max".to_owned()),
			SetHumanEmail::new("max.mustermann3@domain.invalid".to_owned()).with_is_verified(true),
		)
		.with_organization(Organization::new().with_org_id(config.zitadel.organization_id.clone()));

		let uid = zitadel
			.create_human_user(user)
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

		uid
	};

	// A user with an existing link that isn't actually correct
	let extant_broken_uid = {
		let user = AddHumanUserRequest::new(
			SetHumanProfile::new("Max".to_owned(), "Mustermann".to_owned())
				.with_nick_name(hex::encode("max.mustermann4".as_bytes()))
				.with_display_name("Mustermann, Max".to_owned()),
			SetHumanEmail::new("max.mustermann4@domain.invalid".to_owned()).with_is_verified(true),
		)
		.with_organization(Organization::new().with_org_id(config.zitadel.organization_id.clone()));

		let uid = zitadel
			.create_human_user(user)
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
			"max.mustermann4@domain.invalid",
			None,
			"invalid",
			false,
		)
		.await;

		uid
	};

	link_user_ids(config.clone(), &skipped_errors).await.expect("Linking should succeed");

	let nick = zitadel
		.get_user_by_id(&missing_id_uid)
		.await
		.expect("user must exist")
		.user()
		.and_then(|u| u.human())
		.and_then(|h| h.profile())
		.and_then(|p| p.nick_name())
		.expect("Nickname must be set")
		.clone();

	assert_eq!(String::from_utf8_lossy(&hex::decode(nick).expect("must decode")), "max.mustermann");

	let nick = zitadel
		.get_user_by_id(&extant_id_uid)
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
		"max.mustermann2"
	);

	let nick = zitadel
		.get_user_by_id(&missing_ldap_uid)
		.await
		.expect("user must exist")
		.user()
		.and_then(|u| u.human())
		.and_then(|h| h.profile())
		.and_then(|p| p.nick_name())
		.cloned();

	// Unfortunately, Zitadel gives an empty string for a missing
	// field.
	assert_eq!(Some("".to_owned()), nick);

	let nick = zitadel
		.get_user_by_id(&extant_broken_uid)
		.await
		.expect("user must exist")
		.user()
		.and_then(|u| u.human())
		.and_then(|h| h.profile())
		.and_then(|p| p.nick_name())
		.expect("Nickname must be set")
		.clone();

	// Assert this doesn't change
	assert_eq!(
		String::from_utf8_lossy(&hex::decode(&nick).expect("must decode")),
		"max.mustermann4"
	);
	assert_ne!(String::from_utf8_lossy(&hex::decode(&nick).expect("must decode")), "invalid");
}
