use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
	#[error("failed to query users from CSV")]
	CsvQuery(#[from] crate::sources::csv::CsvError),
	#[error("failed to query users from LDAP")]
	LdapQuery(#[from] crate::sources::ldap::LdapError),
}

pub(crate) type Result<T> = std::result::Result<T, Error>;
