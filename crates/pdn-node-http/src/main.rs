//! Entry point: serve one embedded runtime over HTTP.
//!
//! Environment: `PDN_HOST` (default `127.0.0.1`), `PDN_PORT` (default
//! `3011`), and `PDN_DEBUG=1` to mount the scaffolding `/debug/` routes
//! (absent otherwise). The binary is glue only — assembly and authorization
//! posture live in `pdn-node` (see the library crate docs).

use std::{sync::Arc, time::Duration};

use pdn_node::Runtime;
use pdn_node_http::{bind_addr_from_env, debug_enabled_from_env, router};

/// How long `main` waits for axum's graceful drain — in-flight requests,
/// e.g. a long-running `/debug/link` — before giving up on them and moving
/// on to `runtime.shutdown()` regardless. Independent of any ceremony's own
/// budget (which can run up to a day). Runtime shutdown has its own bounds.
const GRACEFUL_DRAIN_BUDGET: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let runtime = Arc::new(Runtime::spawn().await?);
    // The two startup markers below bracket the only silent stretch of a
    // node's life. Without them a node stuck in `Runtime::spawn` and one
    // serving happily but unreachable leave byte-identical logs, and the
    // difference between them is the difference between a defect here and a
    // defect in whatever publishes the port.
    tracing::info!("runtime spawned");
    let result = run(&runtime).await;
    // Always runs, on every outcome of `run` above, `?`-early-returns
    // included: skipping it leaves the endpoint and every task a hosted
    // identity spawned running while the process tries to exit, which holds
    // the exit for minutes. `Runtime::shutdown` takes `&self` and is
    // idempotent, so nothing here needs `runtime` back afterward.
    let shutdown = runtime.shutdown().await;
    match (result, shutdown) {
        (Err(primary), Err(shutdown)) => Err(anyhow::anyhow!(
            "{primary:#}; runtime shutdown also failed: {shutdown:#}"
        )),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(shutdown)) => Err(shutdown),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// The fallible part of serving: read configuration, bind, and serve until
/// a stop signal arrives, then drain for at most [`GRACEFUL_DRAIN_BUDGET`].
/// Split out of `main` so that every exit from here — the stop signal
/// followed by the drain budget, or any `?` on a configuration/bind
/// failure — still reaches `runtime.shutdown()` in `main`, unconditionally
/// and exactly once.
async fn run(runtime: &Arc<Runtime>) -> anyhow::Result<()> {
    let app = router(Arc::clone(runtime), debug_enabled_from_env()?);
    let listener = tokio::net::TcpListener::bind(bind_addr_from_env()?).await?;
    tracing::info!(addr = ?listener.local_addr()?, "HTTP listening");

    // The drain budget starts counting only once a stop signal actually
    // arrives — `stopped` stays pending, and `serve` runs unbounded, for as
    // long as no signal has come in.
    let stopped = Arc::new(tokio::sync::Notify::new());
    let signalled = Arc::clone(&stopped);
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        stop_signal().await;
        signalled.notify_one();
    });

    // Past this budget, whatever of the drain remains is dropped along with
    // the listener, and `main` moves on to `runtime.shutdown()` regardless.
    let drain_budget_spent = async {
        stopped.notified().await;
        tokio::time::sleep(GRACEFUL_DRAIN_BUDGET).await;
    };

    tokio::select! {
        served = serve => served.map_err(Into::into),
        () = drain_budget_spent => {
            tracing::warn!(?GRACEFUL_DRAIN_BUDGET, "HTTP graceful-drain budget expired");
            Ok(())
        },
    }
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
