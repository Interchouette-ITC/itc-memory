//! Database configuration loaded from the local operator environment.

use std::env;

use thiserror::Error;

/// Connection settings for the local PostgreSQL service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseConfig {
    /// PostgreSQL server host.
    pub host: String,
    /// PostgreSQL server port.
    pub port: u16,
    /// Database name.
    pub database: String,
    /// Database role name.
    pub user: String,
    /// Database role password. Do not log this value.
    pub password: String,
}

impl DatabaseConfig {
    /// Loads settings from the `ITC_MEMORY_POSTGRES_*` environment variables.
    ///
    /// # Errors
    ///
    /// Returns an error when a required setting is missing or the port is invalid.
    pub fn from_environment() -> Result<Self, DatabaseConfigError> {
        let port = env::var("ITC_MEMORY_POSTGRES_PORT")
            .unwrap_or_else(|_| "5432".to_owned())
            .parse()
            .map_err(DatabaseConfigError::InvalidPort)?;
        Ok(Self {
            host: env::var("ITC_MEMORY_POSTGRES_HOST").unwrap_or_else(|_| "127.0.0.1".to_owned()),
            port,
            database: required("ITC_MEMORY_POSTGRES_DB")?,
            user: required("ITC_MEMORY_POSTGRES_USER")?,
            password: required("ITC_MEMORY_POSTGRES_PASSWORD")?,
        })
    }
}

fn required(name: &'static str) -> Result<String, DatabaseConfigError> {
    env::var(name).map_err(|_| DatabaseConfigError::Missing(name))
}

/// An invalid local database configuration.
#[derive(Debug, Error)]
pub enum DatabaseConfigError {
    /// A required variable was absent.
    #[error("required environment variable {0} is not set")]
    Missing(&'static str),
    /// The configured port was not a valid `u16`.
    #[error("ITC_MEMORY_POSTGRES_PORT is invalid: {0}")]
    InvalidPort(#[source] std::num::ParseIntError),
}
