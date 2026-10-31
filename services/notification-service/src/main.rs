//! Email and notifications for TiketKu.

use std::sync::Arc;

use axum::Router;
use common::health::{self, HealthCheck, TcpCheck};
use serde::Deserialize;

const SERVICE: &str = "notification-service";

/// Read from `NOTIFICATION_*` environment variables, e.g. `NOTIFICATION_HTTP_PORT`.
/// No `Debug` derive: the URLs contain passwords.
#[derive(Deserialize)]
struct Config {
    #[serde(default = "default_http_port")]
    http_port: u16,
    database_url: String,
    kafka_brokers: String,
    rabbitmq_url: String,
}

fn default_http_port() -> u16 {
    8085
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    common::config::load_dotenv()?;
    common::telemetry::init_tracing(SERVICE)?;
    let config: Config = common::config::from_env_prefixed("NOTIFICATION")?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");

    // API routes (added in later stories) get request IDs and request logs.
    // Health routes stay outside, so kubelet probes every few seconds do not
    // fill the logs (a 503 probe would log "response failed" each time).
    let api = Router::new();
    let app =
        common::telemetry::with_request_id(api).merge(health::router(readiness_checks(&config)?));
    common::server::serve(app, config.http_port).await?;
    Ok(())
}

/// Critical: postgres. Also reported, but never `503`: kafka and rabbitmq
/// (the outbox keeps the service working while brokers are down).
fn readiness_checks(config: &Config) -> anyhow::Result<Vec<Arc<dyn HealthCheck>>> {
    Ok(vec![
        TcpCheck::from_url("postgres", &config.database_url, 5432)
            .map_err(anyhow::Error::msg)?
            .into_check(),
        TcpCheck::from_broker_list("kafka", &config.kafka_brokers, 9092)
            .map_err(anyhow::Error::msg)?
            .non_critical()
            .into_check(),
        TcpCheck::from_url("rabbitmq", &config.rabbitmq_url, 5672)
            .map_err(anyhow::Error::msg)?
            .non_critical()
            .into_check(),
    ])
}
