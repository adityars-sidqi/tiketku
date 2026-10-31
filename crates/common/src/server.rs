//! HTTP server with graceful shutdown.
//!
//! On SIGTERM (Kubernetes stopping the pod) or Ctrl-C the server stops
//! accepting new connections, lets in-flight requests finish, then returns.

use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};

use axum::Router;
use tokio::net::TcpListener;

/// Binds `0.0.0.0:<port>` and serves `app` until SIGTERM or Ctrl-C.
///
/// # Errors
///
/// Fails when the port cannot be bound or the server stops with an I/O error.
pub async fn serve(app: Router, port: u16) -> io::Result<()> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    serve_with_shutdown(listener, app, shutdown_signal()).await
}

/// Serves `app` on `listener` until `shutdown` completes, then waits for
/// in-flight requests. [`serve`] passes [`shutdown_signal`]; tests pass
/// their own trigger.
///
/// # Errors
///
/// Fails when the server stops with an I/O error.
pub async fn serve_with_shutdown(
    listener: TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    tracing::info!("server stopped");
    Ok(())
}

/// Completes on Ctrl-C (SIGINT) or, on Unix, SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %err, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(err) => {
                tracing::error!(error = %err, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => tracing::info!(signal = "SIGINT", "shutting down"),
        () = terminate => tracing::info!(signal = "SIGTERM", "shutting down"),
    }
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
        let server = tokio::spawn(serve_with_shutdown(listener, app, async {
            let _ = stop_rx.await;
        }));

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
    async fn new_connections_are_refused_after_shutdown() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/", get(|| async { "ok" }));

        serve_with_shutdown(listener, app, async {}).await.unwrap();

        assert!(TcpStream::connect(addr).await.is_err());
    }
}
