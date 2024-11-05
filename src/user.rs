//! User data helpers
use std::fmt::Display;

use anyhow::{anyhow, Result};
use base64::prelude::{Engine, BASE64_STANDARD};
use uuid::{uuid, Uuid};
use zitadel_rust_client::v2::users::HumanUser;

/// The Famedly UUID namespace to use to generate v5 UUIDs.
const FAMEDLY_NAMESPACE: Uuid = uuid!("d9979cff-abee-4666-bc88-1ec45a843fb8");

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
	pub(crate) preferred_username: Option<StringOrBytes>,
	/// The user's external (non-Zitadel) ID
	pub(crate) external_user_id: StringOrBytes,
}

impl User {
	/// Convert a Zitadel user to our internal representation
	pub fn try_from_zitadel_user(user: HumanUser, external_id: String) -> Result<Self> {
		let first_name = user
			.profile()
			.and_then(|profile| profile.given_name())
			.ok_or(anyhow!("Missing first name for {}", external_id))?
			.clone();

		let last_name = user
			.profile()
			.and_then(|profile| profile.family_name())
			.ok_or(anyhow!("Missing last name for {}", external_id))?
			.clone();

		let email = user
			.email()
			.and_then(|human_email| human_email.email())
			.ok_or(anyhow!("Missing email address for {}", external_id))?
			.clone();

		let phone = user.phone().and_then(|human_phone| human_phone.phone());

		let external_user_id = match BASE64_STANDARD.decode(external_id.clone()) {
			Ok(bytes) => bytes.into(),
			Err(_) => external_id.into(),
		};

		Ok(Self {
			first_name: first_name.into(),
			last_name: last_name.into(),
			email: email.into(),
			phone: phone.map(|phone| phone.clone().into()),
			preferred_username: None,
			external_user_id,
			enabled: true,
		})
	}

	/// Get a display name for this user
	pub fn get_display_name(&self) -> String {
		format!("{}, {}", self.last_name, self.first_name)
	}

	/// Return the user's UUID according to the Famedly UUID spec.
	///
	/// See
	/// https://www.notion.so/famedly/Famedly-UUID-Specification-adc576f0f2d449bba2f6f13b2611738f
	pub fn famedly_uuid(&self) -> String {
		Uuid::new_v5(&FAMEDLY_NAMESPACE, self.external_user_id.as_bytes()).to_string()
	}
}

impl PartialEq for User {
	fn eq(&self, other: &Self) -> bool {
		self.first_name == other.first_name
			&& self.last_name == other.last_name
			&& self.email == other.email
			&& self.phone == other.phone
			&& self.enabled == other.enabled
			&& self.preferred_username == other.preferred_username
			&& self.external_user_id == other.external_user_id
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

/// A structure that can either be a string or bytes
#[derive(Clone, Debug, Eq)]
pub(crate) enum StringOrBytes {
	/// A string
	String(String),
	/// A byte string
	Bytes(Vec<u8>),
}

impl StringOrBytes {
	/// Represent the object as raw bytes, regardless of whether it
	/// can be represented as a string
	pub fn as_bytes(&self) -> &[u8] {
		match self {
			Self::String(string) => string.as_bytes(),
			Self::Bytes(bytes) => bytes,
		}
	}
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

impl Ord for StringOrBytes {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		match (self, other) {
			(Self::String(s), Self::String(o)) => s.cmp(o),
			(Self::String(s), Self::Bytes(o)) => s.as_bytes().cmp(o.as_slice()),
			(Self::Bytes(s), Self::String(o)) => s.as_slice().cmp(o.as_bytes()),
			(Self::Bytes(s), Self::Bytes(o)) => s.cmp(o),
		}
	}
}

impl PartialOrd for StringOrBytes {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
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

impl From<Vec<u8>> for StringOrBytes {
	fn from(value: Vec<u8>) -> Self {
		Self::Bytes(value)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn strb_from_string(string: &str) -> StringOrBytes {
		StringOrBytes::from(string.to_owned())
	}

	fn strb_from_bytes(bytes: &[u8]) -> StringOrBytes {
		StringOrBytes::Bytes(bytes.to_owned())
	}

	#[test]
	fn test_strb_equality() {
		assert_eq!(strb_from_string("a"), strb_from_string("a"));
		assert_ne!(strb_from_string("a"), strb_from_string("b"));

		assert_eq!(strb_from_string("a"), strb_from_bytes(b"a"));
		assert_ne!(strb_from_string("a"), strb_from_bytes(b"b"));

		assert_eq!(strb_from_bytes(b"a"), strb_from_bytes(b"a"));
		assert_ne!(strb_from_bytes(b"a"), strb_from_bytes(b"b"));

		assert_eq!(strb_from_bytes(b"\xc3\x28"), strb_from_bytes(b"\xc3\x28"));
		assert_ne!(strb_from_bytes(b"a"), strb_from_bytes(b"\xc3\x28"));
	}

	#[test]
	fn test_strb_order() {
		assert!(strb_from_string("a") < strb_from_string("b"));
		assert!(strb_from_string("b") > strb_from_string("a"));
		assert!(strb_from_string("b") < strb_from_string("c"));
		assert!(strb_from_string("a") < strb_from_string("c"));

		assert!(strb_from_bytes(b"a") < strb_from_bytes(b"b"));
		assert!(strb_from_bytes(b"b") > strb_from_bytes(b"a"));
		assert!(strb_from_bytes(b"b") < strb_from_bytes(b"c"));
		assert!(strb_from_bytes(b"a") < strb_from_bytes(b"c"));

		assert!(strb_from_string("a") < strb_from_bytes(b"b"));
		assert!(strb_from_string("b") > strb_from_bytes(b"a"));
		assert!(strb_from_string("b") < strb_from_bytes(b"c"));
		assert!(strb_from_string("a") < strb_from_bytes(b"c"));

		assert!(strb_from_bytes(b"\xc3\x28") < strb_from_bytes(b"\xc3\x29"));
	}
}
