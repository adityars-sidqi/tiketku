//! `AppError`: the single error type returned by HTTP handlers, rendered as
//! RFC 9457 Problem Details (`application/problem+json`).
//!
//! ```
//! use common::error::AppError;
//!
//! fn reserve(available: u32, quantity: u32) -> Result<(), AppError> {
//!     if quantity > available {
//!         return Err(AppError::conflict("SOLD_OUT", "The Festival category is sold out"));
//!     }
//!     Ok(())
//! }
//! # assert!(reserve(0, 1).is_err());
//! ```

use std::error::Error as StdError;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::telemetry::REQUEST_ID_HEADER;

/// Media type of every error response.
pub const PROBLEM_JSON: &str = "application/problem+json";

/// Base URI for the problem `type` field, e.g. `.../problems/sold-out`.
const PROBLEM_TYPE_BASE: &str = "https://tiketku.example/problems/";

/// Errors a handler can return. Each variant maps to one HTTP status.
///
/// `code` is a stable machine-readable `UPPER_SNAKE_CASE` value so clients
/// can show their own localized message; `detail` is an English fallback.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// 400: the request is malformed (bad JSON, wrong header format).
    #[error("bad request: {detail}")]
    BadRequest { detail: String },

    /// 401: the token is missing or invalid.
    #[error("unauthorized: {detail}")]
    Unauthorized { detail: String },

    /// 403: authenticated, but the role or scope is not enough.
    #[error("forbidden: {detail}")]
    Forbidden { detail: String },

    /// 404: the resource does not exist, or belongs to someone else.
    #[error("not found: {detail}")]
    NotFound { detail: String },

    /// 409: the request conflicts with the current state, with a domain code
    /// such as `SOLD_OUT`, `INVALID_STATE`, or `REQUEST_IN_PROGRESS`.
    #[error("conflict ({code}): {detail}")]
    Conflict { code: &'static str, detail: String },

    /// 422: one or more fields failed validation.
    #[error("validation failed on {} field(s)", errors.len())]
    Validation { errors: Vec<FieldError> },

    /// 422: well-formed but not processable, with a domain code such as
    /// `IDEMPOTENCY_KEY_REUSED`.
    #[error("unprocessable ({code}): {detail}")]
    Unprocessable { code: &'static str, detail: String },

    /// 429: the rate limit was hit; sent with a `Retry-After` header.
    #[error("rate limited, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    /// 503: a dependency (database, broker, other service) is unavailable.
    #[error("service unavailable: {detail}")]
    ServiceUnavailable { detail: String },

    /// 500: an unexpected failure. The cause is logged, never sent to the client.
    // The cause is only in `source`, not in the message, so error reporters
    // that walk the chain do not print it twice.
    #[error("internal error")]
    Internal(#[source] Box<dyn StdError + Send + Sync>),
}

/// One failed field inside a 422 validation error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldError {
    pub field: String,
    /// `REQUIRED`, `INVALID_FORMAT`, `TOO_SHORT`, `TOO_LONG`, `MIN`, `MAX`, ...
    pub code: String,
    pub message: String,
}

impl FieldError {
    pub fn new(
        field: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            field: field.into(),
            code: code.into(),
            message: message.into(),
        }
    }
}

impl AppError {
    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::BadRequest {
            detail: detail.into(),
        }
    }

    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self::Unauthorized {
            detail: detail.into(),
        }
    }

    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::Forbidden {
            detail: detail.into(),
        }
    }

    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::NotFound {
            detail: detail.into(),
        }
    }

    pub fn conflict(code: &'static str, detail: impl Into<String>) -> Self {
        Self::Conflict {
            code,
            detail: detail.into(),
        }
    }

    pub fn validation(errors: Vec<FieldError>) -> Self {
        Self::Validation { errors }
    }

    pub fn unprocessable(code: &'static str, detail: impl Into<String>) -> Self {
        Self::Unprocessable {
            code,
            detail: detail.into(),
        }
    }

    pub fn rate_limited(retry_after_secs: u64) -> Self {
        Self::RateLimited { retry_after_secs }
    }

    pub fn service_unavailable(detail: impl Into<String>) -> Self {
        Self::ServiceUnavailable {
            detail: detail.into(),
        }
    }

    pub fn internal(source: impl Into<Box<dyn StdError + Send + Sync>>) -> Self {
        Self::Internal(source.into())
    }

    /// HTTP status of this error.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest { .. } => StatusCode::BAD_REQUEST,
            Self::Unauthorized { .. } => StatusCode::UNAUTHORIZED,
            Self::Forbidden { .. } => StatusCode::FORBIDDEN,
            Self::NotFound { .. } => StatusCode::NOT_FOUND,
            Self::Conflict { .. } => StatusCode::CONFLICT,
            Self::Validation { .. } | Self::Unprocessable { .. } => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            Self::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::ServiceUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Machine-readable code sent in the `code` field.
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadRequest { .. } => "BAD_REQUEST",
            Self::Unauthorized { .. } => "UNAUTHORIZED",
            Self::Forbidden { .. } => "FORBIDDEN",
            Self::NotFound { .. } => "NOT_FOUND",
            Self::Conflict { code, .. } | Self::Unprocessable { code, .. } => code,
            Self::Validation { .. } => "VALIDATION_FAILED",
            Self::RateLimited { .. } => "RATE_LIMITED",
            Self::ServiceUnavailable { .. } => "SERVICE_UNAVAILABLE",
            Self::Internal(_) => "INTERNAL_ERROR",
        }
    }

    /// Turns the error into the response body, moving its data (no clones).
    fn into_problem(self) -> ProblemDetails {
        let status = self.status();
        let code = self.code();
        let (detail, errors) = match self {
            Self::BadRequest { detail }
            | Self::Unauthorized { detail }
            | Self::Forbidden { detail }
            | Self::NotFound { detail }
            | Self::Conflict { detail, .. }
            | Self::Unprocessable { detail, .. }
            | Self::ServiceUnavailable { detail } => (detail, None),
            Self::Validation { errors } => {
                ("One or more fields are invalid.".to_owned(), Some(errors))
            }
            Self::RateLimited { retry_after_secs } => (
                format!("Too many requests. Retry after {retry_after_secs} seconds."),
                None,
            ),
            // Never leak internal causes (SQL, stack details) to the client.
            Self::Internal(_) => ("An unexpected error occurred.".to_owned(), None),
        };
        ProblemDetails {
            problem_type: format!(
                "{PROBLEM_TYPE_BASE}{}",
                code.to_lowercase().replace('_', "-")
            ),
            title: status.canonical_reason().unwrap_or("Error").to_owned(),
            status: status.as_u16(),
            detail,
            instance: None,
            code,
            request_id: None,
            errors,
        }
    }
}

/// Every cause of `err`, outermost first: `"query failed: connection reset"`.
fn error_chain(err: &(dyn StdError + 'static)) -> String {
    std::iter::successors(Some(err), |e| (*e).source())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

/// RFC 9457 body.
///
/// `instance` and `request_id` come from the request, which `IntoResponse`
/// cannot see; [`add_problem_context`] fills them in afterwards.
#[derive(Debug, Clone, Serialize)]
struct ProblemDetails {
    #[serde(rename = "type")]
    problem_type: String,
    title: String,
    status: u16,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instance: Option<String>,
    code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    errors: Option<Vec<FieldError>>,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if let Self::Internal(source) = &self {
            tracing::error!(error = %error_chain(source.as_ref()), "internal error");
        }

        let status = self.status();
        let retry_after = match self {
            Self::RateLimited { retry_after_secs } => Some(retry_after_secs),
            _ => None,
        };
        let problem = self.into_problem();
        let body = match serde_json::to_vec(&problem) {
            Ok(body) => body,
            Err(err) => {
                tracing::error!(error = %err, "failed to serialize problem details");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };

        let mut response = (status, body).into_response();
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
        if let Some(secs) = retry_after {
            headers.insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        // Handed to `add_problem_context`, which adds `instance` and `request_id`.
        response.extensions_mut().insert(problem);
        response
    }
}

/// Middleware that completes problem+json bodies with `instance` (the request
/// path) and `request_id`. Installed by
/// [`with_request_id`](crate::telemetry::with_request_id).
pub(crate) async fn add_problem_context(request: Request, next: Next) -> Response {
    let instance = request.uri().path().to_owned();
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let mut response = next.run(request).await;
    let Some(mut problem) = response.extensions_mut().remove::<ProblemDetails>() else {
        return response;
    };
    // An inner layer already encoded the body (e.g. gzip); replacing it with
    // plain JSON would break the response, so keep it as is.
    if response.headers().contains_key(header::CONTENT_ENCODING) {
        return response;
    }
    problem.instance = Some(instance);
    problem.request_id = request_id;

    match serde_json::to_vec(&problem) {
        Ok(body) => {
            // The body grew, so any precomputed length is wrong now.
            response.headers_mut().remove(header::CONTENT_LENGTH);
            *response.body_mut() = Body::from(body);
        }
        Err(err) => tracing::error!(error = %err, "failed to serialize problem details"),
    }
    response
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use serde_json::{Value, json};

    use super::*;

    async fn render(err: AppError) -> (StatusCode, Option<String>, Value) {
        let response = err.into_response();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (
            status,
            content_type,
            serde_json::from_slice(&bytes).unwrap(),
        )
    }

    #[tokio::test]
    async fn every_variant_maps_to_status_and_code() {
        let cases = [
            (AppError::bad_request("bad json"), 400, "BAD_REQUEST"),
            (AppError::unauthorized("no token"), 401, "UNAUTHORIZED"),
            (AppError::forbidden("missing scope"), 403, "FORBIDDEN"),
            (AppError::not_found("order not found"), 404, "NOT_FOUND"),
            (AppError::conflict("SOLD_OUT", "sold out"), 409, "SOLD_OUT"),
            (
                AppError::validation(vec![FieldError::new("quantity", "MAX", "at most 4")]),
                422,
                "VALIDATION_FAILED",
            ),
            (
                AppError::unprocessable("IDEMPOTENCY_KEY_REUSED", "different body"),
                422,
                "IDEMPOTENCY_KEY_REUSED",
            ),
            (AppError::rate_limited(12), 429, "RATE_LIMITED"),
            (
                AppError::service_unavailable("database down"),
                503,
                "SERVICE_UNAVAILABLE",
            ),
            (AppError::internal("db pool closed"), 500, "INTERNAL_ERROR"),
        ];

        for (err, status, code) in cases {
            let (actual_status, content_type, body) = render(err).await;

            assert_eq!(actual_status.as_u16(), status, "status for {code}");
            assert_eq!(
                content_type.as_deref(),
                Some(PROBLEM_JSON),
                "content type for {code}"
            );
            assert_eq!(body["status"], status, "body status for {code}");
            assert_eq!(body["code"], code);
            assert!(body["title"].is_string(), "title for {code}");
            assert!(
                body["type"]
                    .as_str()
                    .unwrap()
                    .starts_with(PROBLEM_TYPE_BASE),
                "type for {code}"
            );
        }
    }

    #[tokio::test]
    async fn conflict_body_matches_rfc_9457_shape() {
        let (_, _, body) = render(AppError::conflict(
            "SOLD_OUT",
            "The Festival category is sold out",
        ))
        .await;

        assert_eq!(
            body,
            json!({
                "type": "https://tiketku.example/problems/sold-out",
                "title": "Conflict",
                "status": 409,
                "detail": "The Festival category is sold out",
                "code": "SOLD_OUT",
            })
        );
    }

    #[tokio::test]
    async fn validation_lists_field_errors() {
        let (_, _, body) = render(AppError::validation(vec![FieldError::new(
            "quantity",
            "MAX",
            "at most 4",
        )]))
        .await;

        assert_eq!(
            body["errors"],
            json!([{"field": "quantity", "code": "MAX", "message": "at most 4"}])
        );
    }

    #[tokio::test]
    async fn rate_limited_sets_retry_after_header() {
        let response = AppError::rate_limited(12).into_response();

        assert_eq!(response.headers()[header::RETRY_AFTER], "12");
    }

    #[derive(Debug, thiserror::Error)]
    #[error("query failed")]
    struct QueryError(#[source] std::io::Error);

    #[test]
    fn error_chain_lists_every_cause() {
        let err = QueryError(std::io::Error::other("connection reset"));

        assert_eq!(error_chain(&err), "query failed: connection reset");
    }

    #[tokio::test]
    async fn internal_error_does_not_leak_cause() {
        let (_, _, body) = render(AppError::internal("password=hunter2 in SQL")).await;

        let text = body.to_string();
        assert!(!text.contains("hunter2"), "leaked: {text}");
        assert_eq!(body["detail"], "An unexpected error occurred.");
    }
}
