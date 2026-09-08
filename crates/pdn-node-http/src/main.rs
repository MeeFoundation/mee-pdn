//! Entry point: serve one embedded runtime over HTTP.
//!
//! Environment: `PDN_DATA_DIR` (required — the runtime's storage directory;
//! the host offers no in-memory mode, and unset stops the start),
//! `PDN_HOST` (default `127.0.0.1`), `PDN_PORT` (default `3011`),
//! `PDN_CONNECTIVITY` (`direct` by default, `relays` or `product` for a peer
//! that is not on this network), and `PDN_DEBUG=1` to mount the scaffolding
//! `/debug/` routes (absent otherwise). The binary is glue only — assembly and authorization
//! posture live in `pdn-node` (see the library crate docs).

use std::{sync::Arc, time::Duration};

use pdn_node::{Runtime, SpawnOptions};
use pdn_node_http::{
    bind_addr_from_env, connectivity_from_env, data_dir_from_env, debug_enabled_from_env, router,
};

/// How long `main` waits for axum's graceful drain before moving on to
/// `runtime.shutdown()`. This budget and that shutdown together have to fit
/// inside a container runtime's default grace — SIGTERM, then SIGKILL about
/// 10 seconds later — or the drain is the first thing lost.
const GRACEFUL_DRAIN_BUDGET: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Parsed whole before anything is created, so a typo answers at once
    // with no directory made, no key minted, no lock taken.
    let data_dir = data_dir_from_env()?;
    let bind_addr = bind_addr_from_env()?;
    let debug_enabled = debug_enabled_from_env()?;
    let connectivity = connectivity_from_env()?;

    // Spawned before the listener binds: an unusable directory exits here,
    // serving nothing and never falling back to memory.
    let runtime = Arc::new(
        Runtime::spawn(SpawnOptions {
            connectivity,
            ..SpawnOptions::on_directory(data_dir)
        })
        .await?,
    );
    // The two startup markers bracket the only silent stretch of a node's
    // life: without them a node stuck in `spawn` and one serving but
    // unreachable leave byte-identical logs.
    tracing::info!("runtime spawned");
    let result = run(&runtime, bind_addr, debug_enabled).await;
    // On every outcome of `run`: skipping it leaves the endpoint and every
    // hosted identity's tasks running while the process tries to exit.
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

/// Split out of `main` so every exit — the drained stop, or a `?` on a
/// bind failure — reaches `runtime.shutdown()` exactly once.
async fn run(
    runtime: &Arc<Runtime>,
    bind_addr: std::net::SocketAddr,
    debug_enabled: bool,
) -> anyhow::Result<()> {
    let app = router(Arc::clone(runtime), debug_enabled);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    tracing::info!(addr = ?listener.local_addr()?, "HTTP listening");

    // The drain budget starts counting only once a stop signal arrives.
    let stopped = Arc::new(tokio::sync::Notify::new());
    let signalled = Arc::clone(&stopped);
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        stop_signal().await;
        signalled.notify_one();
    });

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

/// Ctrl-C or SIGTERM: a container stop sends the latter, so on Ctrl-C alone
/// the graceful path would never run in the one deployment that has it.
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
