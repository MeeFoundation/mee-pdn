//! A pod member killed and started again at a new address, reached there.
//! The member runs as a process of its own (`tests/support/test_peer.rs`): a
//! node stopped inside the test's process closes its connections, and the
//! connections a kill leaves open on the other member are the case.

use std::{
    io::{BufRead as _, Write as _},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result};
use pdn_node::{
    IdentityService as _, PodInvite, PodsService as _, RecordRef, Runtime, SpawnOptions,
};

/// What a member waits at most for another member's write.
const CONVERGENCE: Duration = Duration::from_secs(120);

/// Connections b1 holds to a1's first process: more than a1 opens at its
/// restart, so its new address does not outnumber its dead one.
const HELD: usize = 8;

/// A member's device in a process of its own, on a directory.
struct Peer {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
}

impl Peer {
    fn spawn(dir: &Path) -> Result<Self> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_pdn-test-peer"))
            .arg(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().context("the peer has no stdin")?;
        let stdout = child.stdout.take().context("the peer has no stdout")?;
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    return;
                };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines,
        })
    }

    /// The rest of the next line the peer prints that starts with `prefix`.
    fn expect(&self, prefix: &str) -> Result<String> {
        tokio::task::block_in_place(|| {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let line = self
                    .lines
                    .recv_timeout(left)
                    .with_context(|| format!("the peer printed no {prefix:?}"))?;
                if let Some(rest) = line.strip_prefix(prefix) {
                    return Ok(rest.trim().to_owned());
                }
            }
        })
    }

    fn send(&mut self, command: &str) -> Result<()> {
        writeln!(self.stdin, "{command}")?;
        Ok(())
    }

    /// SIGKILL: the process ends with nothing sent on any connection.
    fn kill(&mut self) -> Result<()> {
        self.child.kill()?;
        self.child.wait()?;
        Ok(())
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A member killed and started again at a new address is reached there
/// within the convergence budget, though the other member held more
/// connections to its dead address than the restarted one opens.
///
/// The other member's path selector is asked again when a connection opens,
/// not when one closes, so once its connections to the dead address time
/// out its own dials go there until the restarted member opens a connection
/// to it. The kill waits for the restarted member's address book to hold
/// the other member, which a node saves every 10 s.
#[tokio::test(flavor = "multi_thread")]
async fn a_killed_member_restarted_at_a_new_address_is_reached_there() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut a1 = Peer::spawn(dir.path())?;
    a1.expect("ready")?;
    a1.send("invite")?;
    let invite: PodInvite = postcard::from_bytes(&unhex(&a1.expect("invite")?)?)?;
    let (pod, a1_addr) = (invite.pod, invite.inviter_addr.clone());

    let b1 = Runtime::spawn(SpawnOptions::memory()).await?;
    let bob = b1.identity().create().await?;
    b1.pods().join(bob, invite).await?;
    let joined = SystemTime::now();
    let blobs_alpn = data_layer::BUILT_IN_ALPNS
        .first()
        .copied()
        .context("no blob protocol")?;
    let dial = b1.dial_handle_for_test().await;
    let mut held = Vec::new();
    for _ in 0..HELD {
        held.push(dial.connect(a1_addr.clone(), blobs_alpn).await?);
    }
    book_saved_since(dir.path(), joined)?;

    a1.kill()?;
    let mut a1 = Peer::spawn(dir.path())?;
    a1.expect("ready")?;
    let payload = b"placed after the restart";
    a1.send(&format!("place {}", hex(payload)))?;
    let record: RecordRef = postcard::from_bytes(&unhex(&a1.expect("placed")?)?)?;

    let deadline = Instant::now() + CONVERGENCE;
    while b1.pods().read(bob, pod, record).await?.as_deref() != Some(payload.as_slice()) {
        assert!(
            Instant::now() < deadline,
            "the other member never read the restarted member's record"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    drop(held);
    b1.shutdown().await?;
    Ok(())
}

/// Waits for the address book under `dir` to be written after `since`.
fn book_saved_since(dir: &Path, since: SystemTime) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(modified) = std::fs::metadata(dir.join("peers")).and_then(|meta| meta.modified())
        {
            if modified > since {
                return Ok(());
            }
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the address book was never saved"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
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
