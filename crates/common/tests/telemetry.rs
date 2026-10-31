//! Log output tests. Every test here installs its own subscriber (see the
//! note in `src/telemetry.rs` tests about tracing's callsite interest cache).

use std::io;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::routing::get;
use common::telemetry::{REQUEST_ID_HEADER, json_layer, with_request_id};
use serde_json::Value;
use tower::ServiceExt;
use tracing::Subscriber;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;

/// Collects log output in memory so tests can read it back.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn lines(&self) -> Vec<Value> {
        let bytes = self.0.lock().unwrap();
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is JSON"))
            .collect()
    }
}

impl io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn subscriber(out: &Captured) -> impl Subscriber + Send + Sync {
    tracing_subscriber::registry().with(json_layer("test-service", out.clone()))
}

#[test]
fn log_line_is_json_with_service_and_span_fields() {
    let out = Captured::default();

    tracing::subscriber::with_default(subscriber(&out), || {
        let span = tracing::info_span!("request", request_id = "req-123");
        let _guard = span.enter();
        tracing::info!(order_id = 42, "order created");
    });

    let lines = out.lines();
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(line["service"], "test-service");
    assert_eq!(line["request_id"], "req-123");
    assert_eq!(line["level"], "INFO");
    assert_eq!(line["message"], "order created");
    assert_eq!(line["order_id"], 42);
    assert!(line["timestamp"].is_string());
}

#[test]
fn core_fields_are_not_overwritten_by_span_or_event_fields() {
    let out = Captured::default();

    tracing::subscriber::with_default(subscriber(&out), || {
        let span = tracing::info_span!("call_catalog", service = "catalog-service", level = "x");
        let _guard = span.enter();
        tracing::info!(target = "spoofed", "calling catalog");
    });

    let line = &out.lines()[0];
    assert_eq!(line["service"], "test-service");
    assert_eq!(line["level"], "INFO");
    assert_ne!(line["target"], "spoofed");
    assert_eq!(line["fields.service"], "catalog-service");
    assert_eq!(line["fields.target"], "spoofed");
}

#[tokio::test]
async fn request_without_id_gets_uuid_v7_logged_and_returned() {
    let out = Captured::default();
    let _default = tracing::subscriber::set_default(subscriber(&out));
    let app = with_request_id(Router::new().route(
        "/ping",
        get(|| async {
            tracing::info!("handling ping");
            "pong"
        }),
    ));

    let response = app
        .oneshot(Request::get("/ping").body(Body::empty()).unwrap())
        .await
        .unwrap();

    let returned = response.headers()[REQUEST_ID_HEADER].to_str().unwrap();
    let parsed = uuid::Uuid::parse_str(returned).expect("request id is a UUID");
    assert_eq!(parsed.get_version_num(), 7);

    let logged = out
        .lines()
        .into_iter()
        .find(|line| line["message"] == "handling ping")
        .expect("handler log line");
    assert_eq!(logged["request_id"], returned);
    assert_eq!(logged["path"], "/ping");
}
