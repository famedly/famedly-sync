//! User data helpers
use anyhow_ext::{Context, Result};
use base64::{Engine as _, engine::general_purpose};
use uuid::{Uuid, uuid};

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

/// Compute the famedly UUID for a given byte string
#[must_use]
pub fn compute_famedly_uuid(external_id: &[u8]) -> String {
	Uuid::new_v5(&FAMEDLY_NAMESPACE, external_id).to_string()
}

/// Helper trait (type function) to have to copies for `User` type: for
/// `[Required]` and `[Optional]`. `O::T<X>` should be used in fields that in
/// one case are required while in other are optional.
pub trait Optionable: Clone {
	/// Result of the type function
	type T<X: Clone>: Clone;
}

/// `[Optionable]` impl that returns the input type as is.
#[derive(Clone)]
#[allow(missing_debug_implementations)]
pub struct Required;
impl Optionable for Required {
	type T<X: Clone> = X;
}

/// `[Optionable]` impl that wraps the input type in `[Option]`.
#[derive(Clone)]
#[allow(missing_debug_implementations)]
pub struct Optional;
impl Optionable for Optional {
	type T<X: Clone> = Option<X>;
}

/// Source-agnostic representation of a user
#[derive(Clone)]
pub struct User<O: Optionable> {
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
	pub(crate) preferred_username: O::T<String>,
	/// The user's external (non-Zitadel) ID
	pub(crate) external_user_id: String,
	/// The user's localpart (used as Zitadel userId)
	pub(crate) localpart: String,
}

impl User<Optional> {
	/// `[Eq]` alternative to check for equality `User<Optional>` and
	/// `User<Required>`.
	pub fn is_up_to_date(&self, new_user_data: &User<Required>) -> bool {
		self.first_name == new_user_data.first_name
			&& self.last_name == new_user_data.last_name
			&& self.email == new_user_data.email
			&& self.phone == new_user_data.phone
			&& self.enabled == new_user_data.enabled
			&& self.preferred_username.as_ref() == Some(&new_user_data.preferred_username)
			&& self.external_user_id == new_user_data.external_user_id
			&& self.localpart == new_user_data.localpart
	}
}

impl User<Required> {
	/// Make `User` with optional fields out of `User` with strict fields.
	pub fn to_optional(self) -> User<Optional> {
		User {
			first_name: self.first_name,
			last_name: self.last_name,
			email: self.email,
			phone: self.phone,
			enabled: self.enabled,
			preferred_username: Some(self.preferred_username),
			external_user_id: self.external_user_id,
			localpart: self.localpart,
		}
	}
}

impl<O: Optionable> User<O> {
	/// Create a new user instance, used in tests
	#[allow(clippy::must_use_candidate, clippy::too_many_arguments)]
	pub fn new(
		first_name: String,
		last_name: String,
		email: String,
		phone: Option<String>,
		enabled: bool,
		preferred_username: O::T<String>,
		external_user_id: String,
		localpart: String,
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

	/// Get a display name for this user
	#[must_use]
	pub fn get_display_name(&self) -> String {
		format!("{}, {}", self.last_name, self.first_name)
	}

	/// Get the localpart
	#[must_use]
	pub fn get_localpart(&self) -> &str {
		self.localpart.as_ref()
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

	/// Convert external user ID to a new format based on the detected encoding
	pub fn create_user_with_converted_external_id(
		&self,
		expected_encoding: ExternalIdEncoding,
	) -> Result<Self> {
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

impl<O: Optionable> std::fmt::Debug for User<O> {
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
