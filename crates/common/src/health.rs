//! Health endpoints for Kubernetes probes.
//!
//! - `GET /health/live`: the process is up. Always `200`; never checks
//!   dependencies, so a slow database does not get the pod restarted.
//! - `GET /health/ready`: runs every [`HealthCheck`] concurrently. Answers
//!   `503` when a *critical* check fails, so the pod leaves the load balancer
//!   but is not restarted.
//!
//! Brokers (Kafka, RabbitMQ) are usually registered as non-critical: they are
//! still reported, but do not cause `503`. The transactional outbox keeps
//! orders working while Kafka is down, and a not-ready pod would block that.
//!
//! ```
//! use std::time::Duration;
//! use axum::Router;
//! use common::health::{self, TcpCheck};
//!
//! let checks = vec![
//!     TcpCheck::from_url("postgres", "postgres://user:pw@localhost:5432/order_db", 5432)
//!         .unwrap()
//!         .into_check(),
//!     TcpCheck::from_url("kafka", "localhost:9092", 9092)
//!         .unwrap()
//!         .non_critical()
//!         .into_check(),
//! ];
//! let app: Router = Router::new().merge(health::router(checks));
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use serde::Serialize;
use tokio::net::TcpStream;
use tokio::task::JoinSet;

/// How long one check may take before it counts as failed.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// Boxed future returned by [`HealthCheck::check`].
pub type CheckFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// One dependency that `/health/ready` checks.
///
/// The future is boxed so checks of different types can live in one
/// `Vec<Arc<dyn HealthCheck>>` (an `async fn` in a trait is not object safe).
pub trait HealthCheck: Send + Sync + 'static {
    /// Short name shown in the response, e.g. `postgres`.
    fn name(&self) -> &str;

    /// `false` when a failure should be reported but not cause `503`.
    fn critical(&self) -> bool {
        true
    }

    /// `Ok(())` when the dependency is usable, else a short reason.
    fn check(&self) -> CheckFuture<'_>;
}

/// Checks that a TCP connection to the dependency can be opened.
///
/// Enough for F0, where services have no database or broker clients yet.
/// Later stories can add checks that use the real client (`SELECT 1` on the
/// pool, Redis `PING`) by implementing [`HealthCheck`].
#[derive(Debug, Clone)]
pub struct TcpCheck {
    name: String,
    addrs: Vec<String>,
    critical: bool,
}

impl TcpCheck {
    /// Check `host:port`. Several comma-separated addresses (a Kafka broker
    /// list) pass when any one of them accepts a connection.
    pub fn new(name: impl Into<String>, addrs: Vec<String>) -> Self {
        Self {
            name: name.into(),
            addrs,
            critical: true,
        }
    }

    /// Takes the address from a URL (`postgres://u:p@host:5432/db`,
    /// `redis://host`, `amqp://host:5672/vhost`) or a broker list
    /// (`host1:9092,host2:9092`). `default_port` is used when none is given.
    ///
    /// # Errors
    ///
    /// Returns a message when no host can be found.
    pub fn from_url(name: impl Into<String>, url: &str, default_port: u16) -> Result<Self, String> {
        let name = name.into();
        let addrs = url
            .split(',')
            .map(|part| host_port(part.trim(), default_port))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| format!("no host in {name} address"))?;
        Ok(Self::new(name, addrs))
    }

    /// Report failures without answering `503`.
    #[must_use]
    pub fn non_critical(mut self) -> Self {
        self.critical = false;
        self
    }

    /// Boxes the check for [`router`].
    pub fn into_check(self) -> Arc<dyn HealthCheck> {
        Arc::new(self)
    }
}

/// `scheme://user:pw@host:port/path?query` (or `host:port`) -> `host:port`.
fn host_port(url: &str, default_port: u16) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?']).next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    if host_port.is_empty() {
        return None;
    }
    // `[::1]:5432` and `host:5432` have a port; `[::1]` and `host` do not.
    let has_port = match host_port.rsplit_once(':') {
        Some((host, port)) => {
            !host.is_empty() && !port.contains(']') && port.parse::<u16>().is_ok()
        }
        None => false,
    };
    Some(if has_port {
        host_port.to_owned()
    } else {
        format!("{host_port}:{default_port}")
    })
}

impl HealthCheck for TcpCheck {
    fn name(&self) -> &str {
        &self.name
    }

    fn critical(&self) -> bool {
        self.critical
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let mut last_error = String::from("no address configured");
            for addr in &self.addrs {
                match TcpStream::connect(addr.as_str()).await {
                    Ok(_) => return Ok(()),
                    Err(err) => last_error = format!("{addr}: {err}"),
                }
            }
            Err(last_error)
        })
    }
}

/// Routes `GET /health/live` and `GET /health/ready`.
pub fn router<S>(checks: Vec<Arc<dyn HealthCheck>>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let checks: Arc<[Arc<dyn HealthCheck>]> = checks.into();
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(move || ready(Arc::clone(&checks))))
}

async fn live() -> Json<Status> {
    Json(Status { status: "ok" })
}

#[derive(Serialize)]
struct Status {
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct Readiness {
    /// `ready` or `not_ready`.
    status: &'static str,
    checks: Vec<CheckResult>,
}

#[derive(Debug, Serialize)]
struct CheckResult {
    name: String,
    /// `up` or `down`.
    status: &'static str,
    critical: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn ready(checks: Arc<[Arc<dyn HealthCheck>]>) -> Response {
    let mut tasks = JoinSet::new();
    for (index, check) in checks.iter().enumerate() {
        let check = Arc::clone(check);
        tasks.spawn(async move {
            let outcome = match tokio::time::timeout(CHECK_TIMEOUT, check.check()).await {
                Ok(result) => result,
                Err(_) => Err(format!("timed out after {}s", CHECK_TIMEOUT.as_secs())),
            };
            (index, outcome)
        });
    }

    let mut outcomes: Vec<Option<Result<(), String>>> = vec![None; checks.len()];
    while let Some(joined) = tasks.join_next().await {
        if let Ok((index, outcome)) = joined {
            outcomes[index] = Some(outcome);
        }
    }

    // Keep the registration order so the response is stable.
    let results: Vec<CheckResult> = checks
        .iter()
        .zip(outcomes)
        .map(|(check, outcome)| {
            let outcome = outcome.unwrap_or_else(|| Err("check panicked".to_owned()));
            CheckResult {
                name: check.name().to_owned(),
                status: if outcome.is_ok() { "up" } else { "down" },
                critical: check.critical(),
                error: outcome.err(),
            }
        })
        .collect();

    let ready = results.iter().all(|r| !r.critical || r.error.is_none());
    if !ready {
        tracing::warn!(checks = ?results, "not ready");
    }
    let (status, label) = if ready {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not_ready")
    };
    (
        status,
        Json(Readiness {
            status: label,
            checks: results,
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use serde_json::Value;
    use tokio::net::TcpListener;
    use tower::ServiceExt;

    use super::*;

    /// A check with a fixed result.
    struct Fixed {
        name: &'static str,
        critical: bool,
        ok: bool,
    }

    impl HealthCheck for Fixed {
        fn name(&self) -> &str {
            self.name
        }

        fn critical(&self) -> bool {
            self.critical
        }

        fn check(&self) -> CheckFuture<'_> {
            let ok = self.ok;
            Box::pin(async move {
                if ok {
                    Ok(())
                } else {
                    Err("refused".to_owned())
                }
            })
        }
    }

    fn fixed(name: &'static str, critical: bool, ok: bool) -> Arc<dyn HealthCheck> {
        Arc::new(Fixed { name, critical, ok })
    }

    async fn get_json(app: Router, path: &str) -> (StatusCode, Value) {
        let response = app
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn live_is_ok_even_when_dependencies_are_down() {
        let app = router(vec![fixed("postgres", true, false)]);

        let (status, body) = get_json(app, "/health/live").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
    }

    #[tokio::test]
    async fn ready_when_all_checks_pass() {
        let app = router(vec![
            fixed("postgres", true, true),
            fixed("redis", true, true),
        ]);

        let (status, body) = get_json(app, "/health/ready").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ready");
        assert_eq!(body["checks"][0]["name"], "postgres");
        assert_eq!(body["checks"][1]["status"], "up");
    }

    #[tokio::test]
    async fn critical_failure_answers_503() {
        let app = router(vec![
            fixed("postgres", true, false),
            fixed("redis", true, true),
        ]);

        let (status, body) = get_json(app, "/health/ready").await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "not_ready");
        assert_eq!(body["checks"][0]["status"], "down");
        assert_eq!(body["checks"][0]["error"], "refused");
    }

    #[tokio::test]
    async fn non_critical_failure_is_reported_but_still_ready() {
        let app = router(vec![
            fixed("postgres", true, true),
            fixed("kafka", false, false),
        ]);

        let (status, body) = get_json(app, "/health/ready").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["checks"][1]["status"], "down");
        assert_eq!(body["checks"][1]["critical"], false);
    }

    #[tokio::test]
    async fn tcp_check_reports_up_and_down() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap().to_string();
        // Bind and drop to get a port that is (almost certainly) closed.
        let closed = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .to_string();

        assert!(
            TcpCheck::new("up", vec![open.clone()])
                .check()
                .await
                .is_ok()
        );
        assert!(
            TcpCheck::new("down", vec![closed.clone()])
                .check()
                .await
                .is_err()
        );
        assert!(
            TcpCheck::new("any", vec![closed, open])
                .check()
                .await
                .is_ok()
        );
    }

    #[test]
    fn host_port_from_urls() {
        let cases = [
            ("postgres://u:p@db:5432/order_db", 5432, "db:5432"),
            ("postgres://u:p@db/order_db", 5432, "db:5432"),
            ("redis://:secret@cache:6380", 6379, "cache:6380"),
            ("redis://cache", 6379, "cache:6379"),
            ("amqp://u:p@mq:5672/tiketku", 5672, "mq:5672"),
            ("localhost:9092", 9092, "localhost:9092"),
            ("postgres://u:p@[::1]:5433/db", 5432, "[::1]:5433"),
            ("postgres://u:p@[::1]/db", 5432, "[::1]:5432"),
            ("postgres://u:p@db:5432/x?sslmode=disable", 5432, "db:5432"),
        ];
        for (url, default_port, expected) in cases {
            assert_eq!(
                host_port(url, default_port).as_deref(),
                Some(expected),
                "{url}"
            );
        }
        assert_eq!(host_port("postgres://", 5432), None);
    }

    #[test]
    fn broker_list_becomes_several_addresses() {
        let check = TcpCheck::from_url("kafka", "k1:9092, k2", 9092).unwrap();

        assert_eq!(check.addrs, ["k1:9092", "k2:9092"]);
    }
}
