//! Sources of data we want to sync from.

use anyhow_ext::Result;
use async_trait::async_trait;

pub mod csv;
pub mod ldap;
pub mod ukt;

use crate::user::{Required, User};

/// A source of data we want to sync from.
#[async_trait]
pub trait Source {
	/// Get source name for debugging.
	fn get_name(&self) -> &'static str;

	/// Get a stream of the sources' users, sorted by external user ID
	// Ideally we would return a `Stream` here, as this would allow us
	// to cut down significantly on memory use, however none of our
	// sources currently support returning results sorted, so we would
	// need to buffer the results to sort them anyway.
	//
	// In addition, `async_trait` does not currently support returning
	// `impl` traits, making that technically infeasible with Rust.
	//
	// TODO: If we do get sources which *do* support sorting, and Rust
	// gains this feature, we should probably switch to a stream here,
	// though (and update existing sources to return sorted streams).
	async fn get_sorted_users(&self) -> Result<Vec<User<Required>>>;
}
