//! HTTP server with graceful shutdown.
//!
//! On SIGTERM (Kubernetes stopping the pod) or Ctrl-C the server stops
//! accepting new connections and lets in-flight requests finish, for at most
//! [`DRAIN_TIMEOUT`], then returns.

use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// Longest time in-flight requests may take after shutdown starts.
///
/// Below Kubernetes' default `terminationGracePeriodSeconds` (30 s) minus the
/// `preStop` sleep, so the process exits by itself before it is killed.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(20);

/// Binds `0.0.0.0:<port>` and serves `app` until SIGTERM or Ctrl-C.
///
/// # Errors
///
/// Fails when the signal handlers cannot be installed (so the problem shows
/// at startup, not at the first shutdown), when the port cannot be bound, or
/// when the server stops with an I/O error.
pub async fn serve(app: Router, port: u16) -> io::Result<()> {
    let shutdown = shutdown_signal()?;
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    serve_with_shutdown(listener, app, shutdown, DRAIN_TIMEOUT).await
}

/// Serves `app` on `listener` until `shutdown` completes, then waits up to
/// `drain_timeout` for in-flight requests before dropping them.
///
/// [`serve`] passes [`shutdown_signal`] and [`DRAIN_TIMEOUT`]; tests pass
/// their own trigger and a short timeout.
///
/// # Errors
///
/// Fails when the server stops with an I/O error.
pub async fn serve_with_shutdown(
    listener: TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
    drain_timeout: Duration,
) -> io::Result<()> {
    let (draining_tx, draining_rx) = oneshot::channel::<()>();
    let signal = async move {
        shutdown.await;
        let _ = draining_tx.send(());
    };
    let server = async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(signal)
            .await
    };
    let deadline = async move {
        // The sender is dropped without sending if the server stops on its own.
        if draining_rx.await.is_ok() {
            tokio::time::sleep(drain_timeout).await;
        } else {
            std::future::pending::<()>().await;
        }
    };

    tokio::select! {
        result = server => {
            result?;
            tracing::info!("server stopped");
        }
        () = deadline => {
            tracing::warn!(
                timeout_ms = u64::try_from(drain_timeout.as_millis()).unwrap_or(u64::MAX),
                "drain timed out, dropping open connections"
            );
        }
    }
    Ok(())
}

/// Installs the SIGTERM handler now and returns a future that completes on
/// Ctrl-C (SIGINT) or, on Unix, SIGTERM.
///
/// # Errors
///
/// Fails when the SIGTERM handler cannot be installed.
pub fn shutdown_signal() -> io::Result<impl Future<Output = ()> + Send + 'static> {
    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    Ok(async move {
        let ctrl_c = async {
            if let Err(err) = tokio::signal::ctrl_c().await {
                tracing::error!(error = %err, "cannot listen for Ctrl-C");
                std::future::pending::<()>().await;
            }
        };
        #[cfg(unix)]
        let terminate = async move {
            sigterm.recv().await;
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            () = ctrl_c => tracing::info!(signal = "SIGINT", "shutting down"),
            () = terminate => tracing::info!(signal = "SIGTERM", "shutting down"),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::routing::get;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::sync::oneshot;

    use super::*;

    #[tokio::test]
    async fn in_flight_request_finishes_after_shutdown_starts() {
        let (started_tx, started_rx) = oneshot::channel::<()>();
        // Handlers must be `Clone`, so the one-shot sender is shared via `Arc`.
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let app = Router::new().route(
            "/slow",
            get(move || {
                if let Some(tx) = started_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                async {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    "done"
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_with_shutdown(
            listener,
            app,
            async {
                let _ = stop_rx.await;
            },
            Duration::from_secs(5),
        ));

        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"GET /slow HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            response
        });

        // Trigger shutdown while the handler is still sleeping.
        started_rx.await.unwrap();
        stop_tx.send(()).unwrap();

        let response = client.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "got: {response}");
        assert!(response.ends_with("done"), "got: {response}");

        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server stops after the request")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn idle_server_stops_right_after_shutdown() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = Router::new().route("/", get(|| async { "ok" }));

        let stopped = tokio::time::timeout(
            Duration::from_secs(5),
            serve_with_shutdown(listener, app, async {}, Duration::from_secs(5)),
        )
        .await;

        assert!(matches!(stopped, Ok(Ok(()))), "got: {stopped:?}");
    }

    #[tokio::test]
    async fn slow_request_is_dropped_after_drain_timeout() {
        let (started_tx, started_rx) = oneshot::channel::<()>();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let app = Router::new().route(
            "/stuck",
            get(move || {
                if let Some(tx) = started_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                async {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    "never"
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_with_shutdown(
            listener,
            app,
            async move {
                let _ = started_rx.await;
            },
            Duration::from_millis(100),
        ));

        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /stuck HTTP/1.1\r\nHost: test\r\n\r\n")
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server gives up on the stuck request")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_signal_installs_handlers() {
        assert!(shutdown_signal().is_ok());
    }
}
