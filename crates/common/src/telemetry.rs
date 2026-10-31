//! JSON logging and request IDs.
//!
//! Every log line is one JSON object with `timestamp`, `level`, `target`,
//! `service`, the event fields (`message`, ...), and the fields of the spans
//! it runs in. Requests run inside a `request` span that carries
//! `request_id`, so every log written while handling a request has it.
//!
//! ```no_run
//! use axum::{Router, routing::get};
//!
//! common::telemetry::init_tracing("order-service").expect("tracing already set");
//! let routes: Router = Router::new().route("/", get(|| async { "ok" }));
//! let app = common::telemetry::with_request_id(routes);
//! ```

use std::fmt;

use axum::Router;
use axum::http::{HeaderName, Request};
use serde_json::{Map, Value};
use tower_http::request_id::{
    MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
};
use tower_http::trace::TraceLayer;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_log::NormalizeEvent;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::{JsonFields, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormattedFields, MakeWriter};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::{SubscriberInitExt, TryInitError};

/// Header that carries the request ID, created by the gateway or by us.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Installs the global JSON logger for `service`.
///
/// The level comes from `RUST_LOG` (e.g. `info,order_service=debug`) and
/// defaults to `info`.
///
/// # Errors
///
/// Fails if a global subscriber was already installed.
pub fn init_tracing(service: &'static str) -> Result<(), TryInitError> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(json_layer(service, std::io::stdout))
        .try_init()
}

/// The JSON log layer used by [`init_tracing`], writing to `writer`
/// (`std::io::stdout` in services). Public so a service can combine it with
/// other layers (e.g. OpenTelemetry) and tests can capture its output.
pub fn json_layer<S, W>(service: &'static str, writer: W) -> impl tracing_subscriber::Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    tracing_subscriber::fmt::layer()
        .fmt_fields(JsonFields::new())
        .event_format(JsonFormat { service })
        .with_writer(writer)
}

/// Longest incoming request ID we accept; anything longer is replaced.
const MAX_REQUEST_ID_LEN: usize = 128;

/// Adds request ID handling and a `request` span to every route:
///
/// 1. Drop an incoming `x-request-id` that is empty, longer than 128 bytes,
///    has characters outside `[A-Za-z0-9_-]`, or is sent more than once
///    (keeps logs and forwarded headers clean).
/// 2. Keep a valid incoming `x-request-id`, or create a UUID v7 if missing.
/// 3. Open a `request` span with `request_id`, `method`, and `path`.
/// 4. Copy `x-request-id` to the response.
/// 5. Add `instance` and `request_id` to problem+json error bodies.
///
/// Call this **last**, after every other `layer` (auth, timeout, ...):
/// errors from layers added after it get no `request_id` span, no
/// `instance`/`request_id` in their body, and no echoed header. Layers that
/// rewrite bodies, such as compression, belong inside (before this call).
pub fn with_request_id<S>(router: Router<S>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let header = HeaderName::from_static(REQUEST_ID_HEADER);
    // Layers wrap from the bottom up, so the last `layer` call runs first.
    router
        .layer(axum::middleware::from_fn(crate::error::add_problem_context))
        .layer(PropagateRequestIdLayer::new(header.clone()))
        .layer(TraceLayer::new_for_http().make_span_with(request_span))
        .layer(SetRequestIdLayer::new(header, MakeRequestUuidV7))
        .layer(axum::middleware::map_request(drop_invalid_request_id))
}

async fn drop_invalid_request_id<B>(mut request: Request<B>) -> Request<B> {
    let values = request.headers().get_all(REQUEST_ID_HEADER);
    let valid = values.iter().count() <= 1
        && values
            .iter()
            .all(|value| is_valid_request_id(value.as_bytes()));
    if !valid {
        request.headers_mut().remove(REQUEST_ID_HEADER);
    }
    request
}

fn is_valid_request_id(id: &[u8]) -> bool {
    (1..=MAX_REQUEST_ID_LEN).contains(&id.len())
        && id
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
}

fn request_span<B>(request: &Request<B>) -> tracing::Span {
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    tracing::info_span!(
        "request",
        request_id = %request_id,
        method = %request.method(),
        path = %request.uri().path(),
    )
}

/// Creates time-ordered UUID v7 request IDs, like the rest of TiketKu's IDs.
#[derive(Clone, Copy, Default)]
struct MakeRequestUuidV7;

impl MakeRequestId for MakeRequestUuidV7 {
    fn make_request_id<B>(&mut self, _request: &Request<B>) -> Option<RequestId> {
        let id = uuid::Uuid::now_v7().to_string();
        Some(RequestId::new(id.parse().ok()?))
    }
}

/// Writes each event as one flat JSON object (see the module docs).
struct JsonFormat {
    service: &'static str,
}

impl<S> FormatEvent<S, JsonFields> for JsonFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, JsonFields>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        // Events bridged from the `log` crate (e.g. rdkafka) carry their real
        // target in `log.*` fields; normalize so `target` is e.g. `rdkafka::client`.
        let normalized = event.normalized_metadata();
        let meta = normalized.as_ref().unwrap_or_else(|| event.metadata());
        let mut line = Map::new();

        // Span fields, outermost first, so an inner span wins on equal names.
        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                let extensions = span.extensions();
                let Some(fields) = extensions.get::<FormattedFields<JsonFields>>() else {
                    continue;
                };
                if let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(fields) {
                    line.extend(fields);
                }
            }
        }

        event.record(&mut JsonVisitor(&mut line));

        // Core fields go in last so a span or event field with the same name
        // (e.g. `service = "catalog-service"` on an outgoing call span) cannot
        // overwrite them; such a value is kept under `fields.<name>`.
        let core: [(&str, Value); 4] = [
            (
                "timestamp",
                chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                    .into(),
            ),
            ("level", meta.level().as_str().into()),
            ("target", meta.target().into()),
            ("service", self.service.into()),
        ];
        for (name, value) in core {
            if let Some(shadowed) = line.insert(name.into(), value) {
                line.insert(format!("fields.{name}"), shadowed);
            }
        }

        writeln!(writer, "{}", Value::Object(line))
    }
}

/// Collects event fields (`message`, `error`, ...) into a JSON map.
struct JsonVisitor<'a>(&'a mut Map<String, Value>);

impl JsonVisitor<'_> {
    fn put(&mut self, field: &Field, value: Value) {
        // `log.target`, `log.file`, ... are already folded into the metadata.
        if !field.name().starts_with("log.") {
            self.0.insert(field.name().into(), value);
        }
    }
}

impl Visit for JsonVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, value.into());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, value.into());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, value.into());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, value.into());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, value.into());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.put(field, format!("{value:?}").into());
    }
}

#[cfg(test)]
mod tests {
    // Tests that read log output live in `tests/telemetry.rs`: a test binary
    // where every test installs a subscriber. Mixed with tests that run
    // without one, tracing's global per-callsite interest cache can disable
    // the `request` span and make those tests flaky.
    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;

    use axum::body::to_bytes;
    use serde_json::Value;

    use crate::AppError;

    async fn call(app: Router, request_id: Option<&str>, path: &str) -> axum::response::Response {
        let mut request = Request::get(path);
        if let Some(id) = request_id {
            request = request.header(REQUEST_ID_HEADER, id);
        }
        app.oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn assert_new_uuid_v7(response: &axum::response::Response) {
        let id = response.headers()[REQUEST_ID_HEADER].to_str().unwrap();
        let parsed = uuid::Uuid::parse_str(id).expect("replaced with a UUID");
        assert_eq!(parsed.get_version_num(), 7);
    }

    #[tokio::test]
    async fn invalid_incoming_request_id_is_replaced() {
        let too_long = "a".repeat(MAX_REQUEST_ID_LEN + 1);
        for bad in ["has spaces", "line\tbreak", "", too_long.as_str()] {
            let app = with_request_id(Router::new().route("/ping", get(|| async { "pong" })));

            let response = call(app, Some(bad), "/ping").await;

            assert_new_uuid_v7(&response);
        }
    }

    #[test]
    fn request_id_validation_rules() {
        assert!(is_valid_request_id(b"0192f3a1-7c2e-7b10-9d4e-3f5a6b7c8d9e"));
        assert!(is_valid_request_id(b"req_123"));
        assert!(is_valid_request_id(
            "a".repeat(MAX_REQUEST_ID_LEN).as_bytes()
        ));
        assert!(!is_valid_request_id(b""));
        assert!(!is_valid_request_id(b"a b"));
        assert!(!is_valid_request_id(b"{\"x\":1}"));
    }

    #[tokio::test]
    async fn problem_body_gets_instance_and_request_id() {
        let app = with_request_id(Router::new().route(
            "/orders/{id}",
            get(|| async { Err::<(), _>(AppError::not_found("order not found")) }),
        ));

        let response = call(app, Some("req-42"), "/orders/7").await;
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(body["instance"], "/orders/7");
        assert_eq!(body["request_id"], "req-42");
        assert_eq!(body["code"], "NOT_FOUND");
    }

    #[tokio::test]
    async fn duplicate_request_id_headers_are_replaced() {
        let app = with_request_id(Router::new().route("/ping", get(|| async { "pong" })));
        let request = Request::get("/ping")
            .header(REQUEST_ID_HEADER, "good")
            .header(REQUEST_ID_HEADER, "bad value {}")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(
            response.headers().get_all(REQUEST_ID_HEADER).iter().count(),
            1
        );
        assert_new_uuid_v7(&response);
    }

    #[tokio::test]
    async fn encoded_problem_body_is_left_untouched() {
        // Stands in for a compression layer inside `with_request_id`.
        let mark_encoded =
            axum::middleware::map_response(|mut response: axum::response::Response| async {
                response.headers_mut().insert(
                    axum::http::header::CONTENT_ENCODING,
                    axum::http::HeaderValue::from_static("gzip"),
                );
                response
            });
        let app = with_request_id(
            Router::new()
                .route(
                    "/orders/{id}",
                    get(|| async { Err::<(), _>(AppError::not_found("order not found")) }),
                )
                .layer(mark_encoded),
        );

        let response = call(app, Some("req-42"), "/orders/7").await;
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();

        assert!(body.get("instance").is_none(), "body was rewritten: {body}");
        assert_eq!(body["code"], "NOT_FOUND");
    }

    #[tokio::test]
    async fn incoming_request_id_is_kept() {
        let app = with_request_id(Router::new().route("/ping", get(|| async { "pong" })));

        let response = app
            .oneshot(
                Request::get("/ping")
                    .header(REQUEST_ID_HEADER, "from-gateway")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.headers()[REQUEST_ID_HEADER], "from-gateway");
    }
}
