//! Typed configuration loaded from environment variables.
//!
//! Each service defines its own config struct with `serde::Deserialize` and
//! loads it once at startup:
//!
//! ```
//! use serde::Deserialize;
//!
//! #[derive(Debug, Deserialize)]
//! struct Config {
//!     database_url: String,              // required: DATABASE_URL
//!     #[serde(default = "default_port")]
//!     http_port: u16,                    // optional: HTTP_PORT
//! }
//!
//! fn default_port() -> u16 { 8081 }
//!
//! let config: Config = common::config::from_vars([
//!     ("DATABASE_URL", "postgres://localhost/auth_db"),
//! ])
//! .unwrap();
//! assert_eq!(config.http_port, 8081);
//! ```
//!
//! An environment variable is the field name in `UPPER_SNAKE_CASE`
//! (`database_url` -> `DATABASE_URL`).
//!
//! Every process variable (`PATH`, `HOME`, ...) is read too, so config
//! structs must not use `#[serde(deny_unknown_fields)]`.

use std::collections::HashMap;

use serde::de::DeserializeOwned;

/// Configuration could not be loaded. The service should exit at startup.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ConfigError {
    message: String,
    // Boxed so `Result<T, ConfigError>` stays small (clippy::result_large_err).
    #[source]
    source: Box<config::ConfigError>,
}

impl From<config::ConfigError> for ConfigError {
    fn from(source: config::ConfigError) -> Self {
        // For a missing field, name the environment variable the user must set.
        let message = match &source {
            config::ConfigError::NotFound(field) => format!(
                "missing required environment variable {} (config field `{field}`)",
                field.to_uppercase()
            ),
            other => format!("invalid configuration: {other}"),
        };
        Self {
            message,
            source: Box::new(source),
        }
    }
}

/// Loads `.env` from the current directory or its parents, if present.
///
/// Meant for local development only; real environment variables always win.
///
/// # Errors
///
/// A missing file is fine; a `.env` that exists but cannot be read or parsed
/// is returned as an error so the service does not start with half a config.
pub fn load_dotenv() -> Result<(), dotenvy::Error> {
    match dotenvy::dotenv() {
        Ok(_) => Ok(()),
        Err(err) if err.not_found() => Ok(()),
        Err(err) => Err(err),
    }
}

/// Reads the process environment into `T`.
///
/// # Errors
///
/// Returns [`ConfigError`] when a required variable is missing or a value
/// cannot be parsed into the field type.
pub fn from_env<T: DeserializeOwned>() -> Result<T, ConfigError> {
    // `vars_os` instead of `vars`: an unrelated non-UTF-8 variable must not
    // panic the service; such variables are skipped.
    from_vars(
        std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?))),
    )
}

/// Reads the given key/value pairs into `T`, the same way as [`from_env`].
///
/// Useful in tests, where changing the real environment is not safe.
///
/// # Errors
///
/// Same as [`from_env`].
pub fn from_vars<T, I, K, V>(vars: I) -> Result<T, ConfigError>
where
    T: DeserializeOwned,
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    let vars: HashMap<String, String> = vars
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect();

    // `Environment` lowercases keys, so DATABASE_URL matches `database_url`.
    // No `try_parsing`: it would turn the string "0012345" into the number
    // 12345 and back, silently changing secrets. serde still parses "9000"
    // into a `u16` field without it.
    let source = config::Environment::default().source(Some(vars));

    let config = config::Config::builder().add_source(source).build()?;
    Ok(config.try_deserialize()?)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    struct TestConfig {
        database_url: String,
        #[serde(default = "default_port")]
        http_port: u16,
    }

    fn default_port() -> u16 {
        8081
    }

    #[test]
    fn reads_typed_values_from_uppercase_vars() {
        let config: TestConfig = from_vars([
            ("DATABASE_URL", "postgres://localhost/auth_db"),
            ("HTTP_PORT", "9000"),
        ])
        .unwrap();

        assert_eq!(config.database_url, "postgres://localhost/auth_db");
        assert_eq!(config.http_port, 9000);
    }

    #[test]
    fn optional_var_falls_back_to_default() {
        let config: TestConfig =
            from_vars([("DATABASE_URL", "postgres://localhost/auth_db")]).unwrap();

        assert_eq!(config.http_port, 8081);
    }

    #[test]
    fn missing_required_var_names_the_field() {
        let err = from_vars::<TestConfig, _, &str, &str>([]).unwrap_err();

        assert_eq!(
            err.to_string(),
            "missing required environment variable DATABASE_URL (config field `database_url`)"
        );
    }

    #[derive(Debug, Deserialize)]
    struct Secrets {
        secret: String,
        version: String,
    }

    #[test]
    fn numeric_looking_string_is_kept_verbatim() {
        let config: Secrets = from_vars([("SECRET", "0012345"), ("VERSION", "1.10")]).unwrap();

        assert_eq!(config.secret, "0012345");
        assert_eq!(config.version, "1.10");
    }

    #[test]
    fn unparsable_value_is_an_error() {
        let err = from_vars::<TestConfig, _, _, _>([
            ("DATABASE_URL", "postgres://localhost/auth_db"),
            ("HTTP_PORT", "not-a-number"),
        ])
        .unwrap_err();

        assert!(err.to_string().contains("http_port"), "got: {err}");
    }
}
