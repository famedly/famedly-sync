//! User data helpers
use std::fmt::Display;

use base64::prelude::{Engine, BASE64_STANDARD};
use zitadel_rust_client::v1::{Email, Gender, Idp, ImportHumanUserRequest, Phone, Profile};

use crate::{config::FeatureFlags, FeatureFlag};

/// Source-agnostic representation of a user
#[derive(Clone)]
pub(crate) struct User {
	/// The user's first name
	pub(crate) first_name: StringOrBytes,
	/// The user's last name
	pub(crate) last_name: StringOrBytes,
	/// The user's email address
	pub(crate) email: StringOrBytes,
	/// The user's phone number
	pub(crate) phone: Option<StringOrBytes>,
	/// Whether the user is enabled
	pub(crate) enabled: bool,
	/// The user's preferred username
	pub(crate) preferred_username: StringOrBytes,
	/// The user's LDAP ID
	pub(crate) external_user_id: StringOrBytes,
}

impl User {
	/// Convert the agnostic user to a Zitadel user
	pub fn to_zitadel_user(&self, feature_flags: &FeatureFlags, idp_id: &str) -> ZitadelUser {
		ZitadelUser {
			user_data: self.clone(),
			needs_email_verification: feature_flags.is_enabled(FeatureFlag::VerifyEmail),
			needs_phone_verification: feature_flags.is_enabled(FeatureFlag::VerifyPhone),
			idp_id: feature_flags.contains(&FeatureFlag::SsoLogin).then(|| idp_id.to_owned()),
		}
	}
}

impl std::fmt::Debug for User {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("User")
			.field("first_name", &"***")
			.field("last_name", &"***")
			.field("email", &"***")
			.field("phone", &"***")
			.field("preferred_username", &"***")
			.field("external_user_id", &self.external_user_id)
			.field("enabled", &self.enabled)
			.finish()
	}
}

/// Crate-internal representation of a Zitadel user
#[derive(Clone, Debug)]
pub struct ZitadelUser {
	/// Details about the user
	pub(crate) user_data: User,

	/// Whether the user should be prompted to verify their email
	pub(crate) needs_email_verification: bool,
	/// Whether the user should be prompted to verify their phone number
	pub(crate) needs_phone_verification: bool,
	/// The ID of the identity provider to link with, if any
	pub(crate) idp_id: Option<String>,
}

impl ZitadelUser {
	/// Get a display name for the user
	pub(crate) fn get_display_name(&self) -> String {
		format!("{}, {}", self.user_data.last_name, self.user_data.first_name)
	}

	/// Return the name to be used in logs to identify this user
	pub(crate) fn log_name(&self) -> String {
		format!("external_id={}", &self.user_data.external_user_id)
	}

	/// Get idp link as required by Zitadel
	fn get_idps(&self) -> Vec<Idp> {
		if let Some(idp_id) = self.idp_id.clone() {
			vec![Idp {
				config_id: idp_id,
				external_user_id: self.user_data.external_user_id.clone().to_string(),
				display_name: self.get_display_name(),
			}]
		} else {
			vec![]
		}
	}
}

impl From<ZitadelUser> for ImportHumanUserRequest {
	fn from(user: ZitadelUser) -> Self {
		Self {
			user_name: user.user_data.email.clone().to_string(),
			profile: Some(Profile {
				first_name: user.user_data.first_name.clone().to_string(),
				last_name: user.user_data.last_name.clone().to_string(),
				display_name: user.get_display_name(),
				gender: Gender::Unspecified.into(), // 0 means "unspecified",
				nick_name: user.user_data.external_user_id.clone().to_string(),
				preferred_language: String::default(),
			}),
			email: Some(Email {
				email: user.user_data.email.clone().to_string(),
				is_email_verified: !user.needs_email_verification,
			}),
			phone: user.user_data.phone.as_ref().map(|phone| Phone {
				phone: phone.to_owned().to_string(),
				is_phone_verified: !user.needs_phone_verification,
			}),
			password: String::default(),
			hashed_password: None,
			password_change_required: false,
			request_passwordless_registration: false,
			otp_code: String::default(),
			idps: user.get_idps(),
		}
	}
}

impl Display for ZitadelUser {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "external_id={}", &self.user_data.external_user_id)
	}
}

/// A structure that can either be a string or bytes
#[derive(Clone, Debug)]
pub(crate) enum StringOrBytes {
	/// A string
	String(String),
	/// A byte string
	Bytes(Vec<u8>),
}

impl PartialEq for StringOrBytes {
	fn eq(&self, other: &Self) -> bool {
		match (self, other) {
			(Self::String(s), Self::String(o)) => s == o,
			(Self::String(s), Self::Bytes(o)) => s.as_bytes() == o,
			(Self::Bytes(s), Self::String(o)) => s == o.as_bytes(),
			(Self::Bytes(s), Self::Bytes(o)) => s == o,
		}
	}
}

impl Display for StringOrBytes {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			StringOrBytes::String(value) => write!(f, "{}", value),
			StringOrBytes::Bytes(value) => write!(f, "{}", BASE64_STANDARD.encode(value)),
		}
	}
}

impl From<String> for StringOrBytes {
	fn from(value: String) -> Self {
		Self::String(value)
	}
}
