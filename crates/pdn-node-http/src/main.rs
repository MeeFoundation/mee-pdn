//! Entry point: serve one embedded runtime over HTTP.
//!
//! Environment: `PDN_HOST` (default `127.0.0.1`), `PDN_PORT` (default
//! `3011`), and `PDN_DEBUG=1` to mount the scaffolding `/debug/` routes
//! (absent otherwise). The binary is glue only — assembly and authorization
//! posture live in `pdn-node` (see the library crate docs).

use std::sync::Arc;

use pdn_node::Runtime;
use pdn_node_http::{bind_addr_from_env, debug_enabled_from_env, router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let runtime = Arc::new(Runtime::spawn().await?);

    let app = router(Arc::clone(&runtime), debug_enabled_from_env()?);

    let listener = tokio::net::TcpListener::bind(bind_addr_from_env()?).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(stop_signal())
        .await?;

    // Close the endpoint cleanly. `shutdown` needs no exclusive ownership —
    // skipping it anyway leaves the endpoint and every task a hosted
    // identity spawned running while the process tries to exit, which holds
    // the exit for minutes.
    runtime.shutdown().await
}

/// Whichever of Ctrl-C and SIGTERM arrives first. A container stop sends
/// SIGTERM, so on Ctrl-C alone the graceful path would never run in the one
/// deployment that has it.
async fn stop_signal() {
    let interrupt = async {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(
                    "failed to install Ctrl-C handler: {e}; waiting on other signals only"
                );
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                let _ = term.recv().await;
            }
            Err(e) => {
                tracing::warn!("failed to register SIGTERM handler: {e}; Ctrl-C only");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}
