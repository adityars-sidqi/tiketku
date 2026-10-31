//! Readiness logging tests. Like `tests/telemetry.rs`, every test here
//! installs its own subscriber, so tracing's callsite cache cannot hide logs.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::health::{self, CheckFuture, HealthCheck};
use common::telemetry::json_layer;
use serde_json::Value;
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn messages(&self) -> Vec<(String, String)> {
        let bytes = self.0.lock().unwrap();
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .map(|line| {
                (
                    line["level"].as_str().unwrap_or_default().to_owned(),
                    line["message"].as_str().unwrap_or_default().to_owned(),
                )
            })
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

/// A critical check whose result the test can flip.
struct Switch(Arc<AtomicBool>);

impl HealthCheck for Switch {
    fn name(&self) -> &str {
        "postgres"
    }

    fn check(&self) -> CheckFuture<'_> {
        let up = self.0.load(Ordering::Relaxed);
        Box::pin(async move {
            if up {
                Ok(())
            } else {
                Err("refused".to_owned())
            }
        })
    }
}

async fn probe(app: &Router) -> StatusCode {
    app.clone()
        .oneshot(Request::get("/health/ready").body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn readiness_changes_are_logged_once() {
    let out = Captured::default();
    let _default = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(json_layer("test-service", out.clone())),
    );
    let up = Arc::new(AtomicBool::new(false));
    let app: Router = health::router(vec![Arc::new(Switch(Arc::clone(&up)))]);

    assert_eq!(probe(&app).await, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(probe(&app).await, StatusCode::SERVICE_UNAVAILABLE);
    up.store(true, Ordering::Relaxed);
    assert_eq!(probe(&app).await, StatusCode::OK);
    assert_eq!(probe(&app).await, StatusCode::OK);

    assert_eq!(
        out.messages(),
        [
            ("WARN".to_owned(), "not ready".to_owned()),
            ("INFO".to_owned(), "ready again".to_owned()),
        ]
    );
}
