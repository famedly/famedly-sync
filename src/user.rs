//! User data helpers
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use uuid::{uuid, Uuid};
use zitadel_rust_client::v2::users::HumanUser;

/// The Famedly UUID namespace to use to generate v5 UUIDs.
const FAMEDLY_NAMESPACE: Uuid = uuid!("d9979cff-abee-4666-bc88-1ec45a843fb8");

/// The encoding of the external ID in the database
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExternalIdEncoding {
	/// The external ID is stored as a hex string
	Hex,
	/// The external ID is stored as a base64 string
	Base64,
	/// The external ID is stored as a plain string
	Plain,
	/// The encoding could not be determined
	Ambiguous,
}

/// Source-agnostic representation of a user
#[derive(Clone)]
pub struct User {
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
	/// The user's localpart (used as Zitadel userId)
	pub(crate) localpart: Option<String>,
}

impl User {
	/// Create a new user instance, used in tests
	#[allow(clippy::must_use_candidate, clippy::too_many_arguments)]
	pub fn new(
		first_name: String,
		last_name: String,
		email: String,
		phone: Option<String>,
		enabled: bool,
		preferred_username: Option<String>,
		external_user_id: String,
		localpart: Option<String>,
	) -> Self {
		Self {
			first_name,
			last_name,
			email,
			phone,
			enabled,
			preferred_username,
			external_user_id,
			localpart,
		}
	}

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
			localpart: None,
		})
	}

	/// Get a display name for this user
	#[must_use]
	pub fn get_display_name(&self) -> String {
		format!("{}, {}", self.last_name, self.first_name)
	}

	/// Get the localpart
	#[must_use]
	pub fn get_localpart(&self) -> Option<&str> {
		self.localpart.as_deref()
	}

	/// Get the external user ID
	#[must_use]
	pub fn get_external_id(&self) -> &str {
		&self.external_user_id
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

	/// Convert external user ID to a new format based on the detected encoding
	pub fn create_user_with_converted_external_id(
		&self,
		expected_encoding: ExternalIdEncoding,
	) -> Result<User> {
		// Double check the encoding
		let detected_encoding = match &self.external_user_id {
			s if s.is_empty() => {
				tracing::warn!(?self, "Skipping user due to empty uid");
				return Ok(self.clone());
			}
			s if s.chars().all(|c| c.is_ascii_hexdigit()) && s.len() % 2 == 0 => {
				// Looks like hex encoding
				if expected_encoding != ExternalIdEncoding::Hex {
					tracing::warn!(
					  ?self,
					  ?expected_encoding,
					  detected_encoding = ?ExternalIdEncoding::Hex,
					  "Encoding mismatch detected"
					);
				}
				ExternalIdEncoding::Hex
			}
			s if s
				.chars()
				.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
				&& s.len() % 4 == 0 =>
			{
				// Looks like base64 encoding
				if expected_encoding != ExternalIdEncoding::Base64 {
					tracing::warn!(
					  ?self,
					  ?expected_encoding,
					  detected_encoding = ?ExternalIdEncoding::Base64,
					  "Encoding mismatch detected"
					);
				}
				ExternalIdEncoding::Base64
			}
			_ => {
				// Plain or unknown encoding
				if expected_encoding != ExternalIdEncoding::Plain {
					tracing::warn!(
						?self,
						?expected_encoding,
						detected_encoding = ?ExternalIdEncoding::Plain,
						"Encoding mismatch detected"
					);
				}
				ExternalIdEncoding::Plain
			}
		};

		let new_external_id = match expected_encoding {
			ExternalIdEncoding::Hex => self.external_user_id.clone(),
			ExternalIdEncoding::Base64 => decode_base64_or_fallback(
				&self.external_user_id,
				"Failed to decode base64 ID despite database heuristic",
			),
			ExternalIdEncoding::Plain => hex::encode(self.external_user_id.as_bytes()),
			ExternalIdEncoding::Ambiguous => {
				tracing::warn!(
					?self,
					"Using case-by-case detected encoding due to ambiguous expected encoding"
				);
				match detected_encoding {
					ExternalIdEncoding::Hex => self.external_user_id.clone(),
					ExternalIdEncoding::Base64 => decode_base64_or_fallback(
						&self.external_user_id,
						"Failed to decode base64 ID despite case-by-case handling",
					),
					ExternalIdEncoding::Plain => hex::encode(self.external_user_id.as_bytes()),
					ExternalIdEncoding::Ambiguous => {
						tracing::error!(
                      ?self,
                      "Unreachable code? Ambiguous encoding detected despite case-by-case handling."
                  );
						unreachable!("Ambiguous encoding should not be detected here");
					}
				}
			}
		};

		Ok(Self { external_user_id: new_external_id, ..self.clone() })
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
			&& self.localpart == other.localpart
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
			.field("localpart", &self.localpart)
			.field("enabled", &self.enabled)
			.finish()
	}
}

/// Helper function for base64 decoding with fallback
fn decode_base64_or_fallback(id: &str, warning_message: &str) -> String {
	match general_purpose::STANDARD.decode(id) {
		Ok(decoded) => hex::encode(decoded),
		Err(_) => {
			tracing::warn!(?id, "{}", warning_message);
			hex::encode(id.as_bytes())
		}
	}
}
