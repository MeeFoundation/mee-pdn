//! Entry point: serve one embedded runtime over HTTP.
//!
//! Environment: `PDN_DATA_DIR` (required — the runtime's storage directory;
//! the host offers no in-memory mode, and unset stops the start),
//! `PDN_HOST` (default `127.0.0.1`), `PDN_PORT` (default `3011`),
//! `PDN_CONNECTIVITY` (`direct` by default, `relays` or `product` for a peer
//! that is not on this network), `PDN_POD_RECONCILE_INTERVAL_SECS` (the
//! runtime's default unset, otherwise a positive whole number of seconds),
//! and `PDN_DEBUG` — `1` or `true` mounts the
//! scaffolding `/debug/` routes, unset, `0` or `false` leaves them absent,
//! and any other value stops the start. The binary is glue only — assembly
//! and authorization posture live in `pdn-node` (see the library crate docs).

use std::sync::Arc;

use pdn_node::Runtime;
use pdn_node_http::{
    any_origin_from_env, bind_addr_from_env, connectivity_from_env, data_dir_from_env,
    debug_enabled_from_env, pod_reconcile_interval_from_env, serve, spawn_options,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Held to the end: its drop writes out the lines still queued.
    let _log = install_log();

    // Parsed whole before anything is created, so a typo answers at once
    // with no directory made, no key minted, no lock taken.
    let data_dir = data_dir_from_env()?;
    let bind_addr = bind_addr_from_env()?;
    let debug_enabled = debug_enabled_from_env()?;
    let any_origin = any_origin_from_env()?;
    let connectivity = connectivity_from_env()?;
    let pod_reconcile_interval = pod_reconcile_interval_from_env()?;

    // Spawned before the listener binds: an unusable directory exits here,
    // serving nothing and never falling back to memory.
    let runtime = Arc::new(
        Runtime::spawn(spawn_options(
            data_dir,
            connectivity,
            pod_reconcile_interval,
        ))
        .await?,
    );
    // The two startup markers bracket the only silent stretch of a node's
    // life: without them a node stuck in `spawn` and one serving but
    // unreachable leave byte-identical logs.
    tracing::info!("runtime spawned");
    let result = run(&runtime, bind_addr, debug_enabled, any_origin).await;
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
    any_origin: bool,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    tracing::info!(addr = ?listener.local_addr()?, "HTTP listening");
    serve(runtime, listener, debug_enabled, any_origin, stop_signal()).await
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

/// The log, written by a thread of its own: a slow standard output — a file
/// behind a busy disk, a pipe nobody drains — would otherwise stall the
/// runtime worker that logs, the store's live actor among them. Past 128,000
/// queued lines a line is dropped rather than waited for. `RUST_LOG` filters
/// as `tracing_subscriber::fmt::init` reads it without its `env-filter`
/// feature, `info` when unset.
fn install_log() -> tracing_appender::non_blocking::WorkerGuard {
    use std::str::FromStr as _;

    use tracing_subscriber::{
        filter::Targets, layer::SubscriberExt as _, util::SubscriberInitExt as _,
    };

    let (writer, guard) = tracing_appender::non_blocking(std::io::stdout());
    let default = || Targets::new().with_default(tracing::Level::INFO);
    let (targets, refused) = match std::env::var("RUST_LOG") {
        Ok(var) => match Targets::from_str(&var) {
            Ok(targets) => (targets, None),
            Err(err) => (default(), Some(format!("ignoring RUST_LOG={var:?}: {err}"))),
        },
        Err(_) => (default(), None),
    };
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(writer))
        .with(targets)
        .init();
    if let Some(refused) = refused {
        tracing::warn!("{refused}");
    }
    guard
}
