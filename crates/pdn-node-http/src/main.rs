//! Entry point: serve one embedded runtime over HTTP.
//!
//! Environment: `PDN_HOST` (default `127.0.0.1`), `PDN_PORT` (default
//! `3011`), and `PDN_DEBUG=1` to mount the scaffolding `/debug/` routes
//! (absent otherwise). The binary is glue only — assembly and authorization
//! posture live in `pdn-node` (see the library crate docs).

use std::sync::Arc;

use anyhow::anyhow;
use pdn_node::Runtime;
use pdn_node_http::{bind_addr_from_env, router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let runtime = Arc::new(Runtime::spawn().await?);

    let debug = std::env::var("PDN_DEBUG").is_ok_and(|v| v == "1" || v == "true");
    let app = router(Arc::clone(&runtime), debug);

    let listener = tokio::net::TcpListener::bind(bind_addr_from_env()?).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(stop_signal())
        .await?;

    // Serve has returned and every handler with it, so the runtime is ours
    // again; close the endpoint cleanly. Sole ownership is required, not
    // hoped for: skipping the shutdown leaves the endpoint and every task a
    // hosted identity spawned running while the process tries to exit,
    // which holds the exit for minutes.
    Arc::try_unwrap(runtime)
        .map_err(|_| anyhow!("a handler still holds the runtime"))?
        .shutdown()
        .await
}

/// Whichever of Ctrl-C and SIGTERM arrives first. A container stop sends
/// SIGTERM, so on Ctrl-C alone the graceful path would never run in the one
/// deployment that has it.
async fn stop_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                let _ = term.recv().await;
            }
            // Nothing to listen on; the interrupt half still answers.
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}
