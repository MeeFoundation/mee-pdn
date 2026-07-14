//! Entry point: serve one embedded runtime over HTTP.
//!
//! Environment: `PDN_HOST` (default `127.0.0.1`), `PDN_PORT` (default
//! `3011`), and `PDN_DEBUG=1` to mount the demo-scaffolding `/debug/`
//! routes (absent otherwise). The binary is glue only — assembly and
//! authorization posture live in `pdn-node` (see the library crate docs).

use std::net::SocketAddr;
use std::sync::Arc;

use pdn_node::Runtime;
use pdn_node_http::router;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let runtime = Arc::new(Runtime::spawn().await?);

    let debug = std::env::var("PDN_DEBUG").is_ok_and(|v| v == "1" || v == "true");
    let app = router(Arc::clone(&runtime), debug);

    let host = std::env::var("PDN_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port: u16 = std::env::var("PDN_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3011);
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;

    // Serve has returned and every handler with it, so the runtime is ours
    // again; close the endpoint cleanly.
    if let Ok(runtime) = Arc::try_unwrap(runtime) {
        runtime.shutdown().await?;
    }
    Ok(())
}
