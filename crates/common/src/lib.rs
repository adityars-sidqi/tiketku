//! Shared infrastructure for all TiketKu services.
//!
//! This crate only holds infrastructure (config, tracing, problem+json errors,
//! health, shutdown), never domain models, so services stay decoupled.
//! It is filled in gradually starting with stories #3 and #4.
//!
//! - [`config`]: typed config from environment variables.
//! - [`telemetry`]: JSON logs with `service` and `request_id`, request ID middleware.
//! - [`error`]: [`AppError`], rendered as RFC 9457 `application/problem+json`.

pub mod config;
pub mod error;
pub mod telemetry;

pub use error::{AppError, FieldError};

/// Message printed when a service starts, e.g. `auth-service v0.1.0 started`.
pub fn startup_message(service: &str, version: &str) -> String {
    format!("{service} v{version} started")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_message_contains_name_and_version() {
        assert_eq!(
            startup_message("auth-service", "0.1.0"),
            "auth-service v0.1.0 started"
        );
    }
}
