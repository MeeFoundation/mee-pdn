//! The pod member `tests/peer_restart.rs` runs as a process of its own, so
//! the scenario can kill it with no goodbye. Started on a directory, it
//! creates an identity and a pod the first time and comes back hosting them
//! on every later start, prints `ready`, then answers one command per stdin
//! line: `invite` with `invite <hex>`, `place <hex payload>` with
//! `placed <hex>`, each answer a postcard encoding.

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use pdn_node::{
    IdentityService as _, PdnId, PodId, PodsService as _, RecordKind, Runtime, SpawnOptions,
    SyncService as _,
};

fn main() -> Result<()> {
    let dir: PathBuf = std::env::args()
        .nth(1)
        .context("usage: pdn-test-peer <directory>")?
        .into();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run(dir))
}

async fn run(dir: PathBuf) -> Result<()> {
    let runtime = Runtime::spawn(SpawnOptions::on_directory(dir)).await?;
    let (identity, pod) = hosted_pod(&runtime).await?;
    let (lines_tx, mut lines) = tokio::sync::mpsc::channel(8);
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else {
                return;
            };
            if lines_tx.blocking_send(line).is_err() {
                return;
            }
        }
    });
    say("ready")?;
    while let Some(line) = lines.recv().await {
        let mut words = line.split_whitespace();
        match (words.next(), words.next()) {
            (Some("invite"), None) => {
                let invite = runtime.pods().invite(identity, pod, None).await?;
                say(&format!("invite {}", hex(&postcard::to_stdvec(&invite)?)))?;
            }
            (Some("place"), Some(payload)) => {
                let payload = unhex(payload)?;
                let record = runtime
                    .pods()
                    .put_record(identity, pod, RecordKind::Claim, &payload)
                    .await?;
                say(&format!("placed {}", hex(&postcard::to_stdvec(&record)?)))?;
            }
            _ => anyhow::bail!("unknown command {line:?}"),
        }
    }
    Ok(())
}

/// The identity and the pod on this directory: created on a first start,
/// and on a later one hosted again by the runtime's recovery once the
/// armer has opened the pod.
async fn hosted_pod(runtime: &Runtime) -> Result<(PdnId, PodId)> {
    let Some(identity) = runtime.sync().hosted_identities().await?.first().copied() else {
        let identity = runtime.identity().create().await?;
        let pod = runtime.pods().create(identity).await?;
        return Ok((identity, pod));
    };
    loop {
        if let Some(info) = runtime.pods().list(identity).await?.first().copied() {
            if runtime.pods().members(identity, info.id).await.is_ok() {
                return Ok((identity, info.id));
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// One line to the scenario, flushed at once.
fn say(line: &str) -> Result<()> {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")?;
    out.flush()?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn unhex(text: &str) -> Result<Vec<u8>> {
    (0..text.len())
        .step_by(2)
        .map(|at| {
            let pair = text.get(at..at.saturating_add(2)).context("odd hex")?;
            u8::from_str_radix(pair, 16).context("not hex")
        })
        .collect()
}
