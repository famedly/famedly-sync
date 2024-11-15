//! User data helpers
use anyhow::{anyhow, Context, Result};
use uuid::{uuid, Uuid};
use zitadel_rust_client::v2::users::HumanUser;

/// The Famedly UUID namespace to use to generate v5 UUIDs.
const FAMEDLY_NAMESPACE: Uuid = uuid!("d9979cff-abee-4666-bc88-1ec45a843fb8");

/// Source-agnostic representation of a user
#[derive(Clone)]
pub(crate) struct User {
	/// The user's first name
	pub(crate) first_name: String,
	/// The user's last name
	pub(crate) last_name: String,
	/// The user's email address
	pub(crate) email: String,
	/// The user's phone number
	pub(crate) phone: Option<String>,
	/// Whether the user is enabled
	pub(crate) enabled: bool,
	/// The user's preferred username
	pub(crate) preferred_username: Option<String>,
	/// The user's external (non-Zitadel) ID
	pub(crate) external_user_id: String,
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

		Ok(Self {
			first_name,
			last_name,
			email,
			phone: phone.cloned(),
			preferred_username: None,
			external_user_id: external_id,
			enabled: true,
		})
	}

	/// Get a display name for this user
	pub fn get_display_name(&self) -> String {
		format!("{}, {}", self.last_name, self.first_name)
	}

	/// Get the external user ID in raw byte form
	pub fn get_external_id_bytes(&self) -> Result<Vec<u8>> {
		// This looks ugly at a glance, since we get the original
		// bytes at some point, however some users will be retrieved
		// from Zitadel at a later point, so we cannot assume that we
		// know the original bytes, and must always decode the
		// external user ID to get those.
		hex::decode(&self.external_user_id).context("Invalid external user ID")
	}

	/// Get the famedly UUID of this user
	pub fn get_famedly_uuid(&self) -> Result<String> {
		Ok(Uuid::new_v5(&FAMEDLY_NAMESPACE, self.get_external_id_bytes()?.as_slice()).to_string())
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
