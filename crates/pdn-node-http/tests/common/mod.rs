//! The stand: nodes as containers on one network, reached over their
//! published HTTP ports.
//!
//! Each node is its own process in its own container, and the only thing
//! that crosses between a test and a node is HTTP. Between nodes nothing
//! HTTP travels: a container is never told another container's address, and
//! establishment, linking and replication run over the runtimes' own
//! protocols on the container network.
//!
//! Nothing here calls a runtime service directly — a test that did would
//! prove the service works and leave the surface it claims to test
//! unexercised. The one property this arrangement cannot reach is a runtime
//! held from inside its own process; that test builds its own runtime and
//! does not come through here.
//!
//! The image is not built here. `just test-docker` builds it first, and a
//! test that finds it missing says so rather than running against a stale
//! one silently.
// Each test binary includes this module and uses its own subset of the
// helpers; what one binary leaves unused is not dead code of the crate.
#![allow(dead_code)]

use std::{
    future::Future,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use anyhow::{ensure, Context as _, Result};
use axum::{body::Bytes, http::StatusCode};
use pdn_node::PdnId;
use pdn_node_http::shapes::{CreatedIdentity, GrantPublication};
use serde::de::DeserializeOwned;
use testcontainers::{
    core::{logs::LogFrame, ContainerPort, ExecCommand, WaitFor},
    runners::AsyncRunner as _,
    ContainerAsync, GenericImage, ImageExt as _,
};

/// The image `just test-docker` builds.
pub const IMAGE_NAME: &str = "pdn-node-http";
pub const IMAGE_TAG: &str = "dev";

/// The port the image serves on, published to the test host by the container
/// runtime. The runtime's own endpoint port is never published: everything
/// between nodes stays on the container network.
const HTTP_PORT_NUM: u16 = 3011;
const HTTP_PORT: ContainerPort = ContainerPort::Tcp(HTTP_PORT_NUM);

/// How long a node has to answer `/live` after its container starts. Not a
/// margin for slowness: a healthy node binds its listener within a
/// millisecond of its store opening, so this is what a container that will
/// never answer costs before it is replaced.
const READY_BUDGET: Duration = Duration::from_secs(20);
const READY_POLL: Duration = Duration::from_millis(250);

/// How many containers one node is worth. A published port that never
/// answers is an arrangement that failed, not a scenario that did, so the
/// container is replaced rather than reported.
const SPAWN_ATTEMPTS: usize = 2;

/// Where a replaced container is recorded. A replacement leaves no other
/// trace on a run that then passes: printing is not available here, and the
/// runner keeps a passing test's output to itself. Without this file a green
/// suite cannot be told apart from one whose failures were all absorbed —
/// which is the difference between a guard that works and a symptom that
/// went away on its own.
///
/// The path is absolute and fixed at build time. A relative one would follow
/// each test process's own directory, which is the package root rather than
/// the workspace, and scatter the evidence beside the crate.
const REPLACEMENT_LOG: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/stand-replacements.log");

/// Where every node's whole log is streamed, one file per container.
///
/// The tail carried into a failing wait covers the failures that are waits.
/// A plain assertion in a test body produces no tail at all, and a container
/// replaced after never answering takes its log with it — so the paths most
/// likely to need a post-mortem are the ones the tail does not reach. The
/// stream is written as it arrives and outlives the container.
///
/// The directory is cleared by whoever clears `target`: these are build
/// artifacts of a run, kept for reading a failure that already happened.
const NODE_LOG_DIR: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/stand-logs");

/// How long a wait for convergence may take. Wide enough to cover several of
/// the runtime's own reconciliation periods, since nothing here shortens that
/// cadence and a lost dial is retried by the periodic pass.
pub const CONVERGENCE_BUDGET: Duration = Duration::from_secs(120);

/// How much of a node's log a failing wait carries into its error.
const LOG_TAIL_BYTES: usize = 4 * 1024;

/// One test's container network. Every node of the test joins it, and it
/// exists only for that test: a container left behind by a failed run cannot
/// be dialed by the next one, where an isolation defect would read as a
/// pass. The network is created with the first node and removed with the
/// last, by the container client itself.
pub struct Stand {
    network: String,
}

impl Stand {
    /// A network of this test's own. The name carries the process id, so
    /// tests running side by side under the test runner — a process each —
    /// never share one.
    pub fn new() -> Self {
        static STANDS: AtomicUsize = AtomicUsize::new(0);
        let ordinal = STANDS.fetch_add(1, Ordering::Relaxed);
        Self {
            network: format!("pdn-stand-{}-{ordinal}", std::process::id()),
        }
    }

    /// Start a node with the debug surface on, and wait until it answers
    /// liveness.
    pub async fn spawn(&self, label: &str) -> Result<Host> {
        self.spawn_with_debug(label, true).await
    }

    /// Start a node with the debug surface off — the gate's other side, and
    /// the image's own default.
    pub async fn spawn_without_debug(&self, label: &str) -> Result<Host> {
        self.spawn_with_debug(label, false).await
    }

    async fn spawn_with_debug(&self, label: &str, debug: bool) -> Result<Host> {
        let mut failed = None;
        let mut replaced = String::new();
        for attempt in 1..=SPAWN_ATTEMPTS {
            // The name carries the attempt, so a replacement never collides
            // with a predecessor the daemon has not finished removing.
            let name = match attempt {
                1 => format!("{}-{label}", self.network),
                n => format!("{}-{label}-{n}", self.network),
            };
            let mut image = GenericImage::new(IMAGE_NAME, IMAGE_TAG)
                .with_exposed_port(HTTP_PORT)
                .with_wait_for(WaitFor::Nothing)
                // The environment is stated, never inherited: a bind value
                // meant for a node in a test's own process would have this
                // one publish an address no other container can reach. The
                // runtime's endpoint bind stays unset, so the endpoint binds
                // every interface and publishes the container's own address.
                .with_env_var("RUST_LOG", "info,pdn_node=debug,data_layer=debug")
                .with_network(&self.network)
                .with_log_consumer(stream_to_file(&name))
                .with_container_name(name);
            if debug {
                image = image.with_env_var("PDN_DEBUG", "1");
            }
            let container = image
                .start()
                .await
                .with_context(|| format!("starting {label}: is {IMAGE_NAME}:{IMAGE_TAG} built?"))?;
            let client = reqwest::Client::new();
            match wait_live(&container, label, &client).await {
                Ok(base) => {
                    return Ok(Host {
                        label: label.to_owned(),
                        base,
                        client,
                        container,
                        replaced,
                    })
                }
                Err(err) => {
                    // Recorded twice, for two different readers: on the node,
                    // where it surfaces in diagnostics if something fails
                    // afterwards, and in a file, which is the only trace a
                    // run that then passes leaves behind.
                    let note = format!(
                        "[{label}] container {attempt} of {SPAWN_ATTEMPTS} never answered and was \
                         replaced. {err:#}\n"
                    );
                    record_replacement(&note);
                    replaced.push_str(&note);
                    let _ = container.rm().await;
                    failed = Some(err);
                }
            }
        }
        Err(failed
            .unwrap_or_else(|| anyhow::anyhow!("{label} was never given a container to start")))
    }
}

impl Default for Stand {
    fn default() -> Self {
        Self::new()
    }
}

/// One answer from the surface: what an assertion sees.
pub struct Answer {
    pub status: StatusCode,
    pub body: Bytes,
}

impl Answer {
    /// The body of a successful answer; a refusal fails here, naming its
    /// status and the runtime's own text.
    pub fn ok(self) -> Result<Bytes> {
        ensure!(
            self.status.is_success(),
            "expected success, got {}: {}",
            self.status,
            self.text()
        );
        Ok(self.body)
    }

    /// The JSON body of a successful answer.
    pub fn json<T: DeserializeOwned>(self) -> Result<T> {
        let body = self.ok()?;
        serde_json::from_slice(&body)
            .with_context(|| format!("undecodable answer: {}", String::from_utf8_lossy(&body)))
    }

    /// The body as text, for assertions about what an answer does *not*
    /// carry.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// One node of the stand: a container and the client that reaches it.
pub struct Host {
    label: String,
    base: String,
    client: reqwest::Client,
    container: ContainerAsync<GenericImage>,
    /// What it took to get this node up, empty when it came up first try.
    replaced: String,
}

impl Host {
    pub async fn get(&self, path: &str) -> Result<Answer> {
        self.request(Method::Get, path, Bytes::new()).await
    }

    pub async fn post(&self, path: &str, body: impl Into<Bytes>) -> Result<Answer> {
        self.request(Method::Post, path, body.into()).await
    }

    pub async fn put(&self, path: &str, body: impl Into<Bytes>) -> Result<Answer> {
        self.request(Method::Put, path, body.into()).await
    }

    pub async fn delete(&self, path: &str) -> Result<Answer> {
        self.request(Method::Delete, path, Bytes::new()).await
    }

    /// Create an identity here and hand back what the surface named.
    pub async fn create_identity(&self) -> Result<PdnId> {
        let created: CreatedIdentity =
            self.post("/debug/identities", Bytes::new()).await?.json()?;
        Ok(created.identity)
    }

    /// Publish a grant of `issuer`'s data from `identity` toward `peer`.
    pub async fn publish_grant(
        &self,
        identity: PdnId,
        peer: PdnId,
        publication: &GrantPublication,
    ) -> Result<Answer> {
        self.post(
            &format!("/debug/identities/{identity}/grants/{peer}"),
            serde_json::to_vec(publication)?,
        )
        .await
    }

    /// Stop this node — the process ends and, with storage in memory, its
    /// state ends with it. Nothing restarts a stopped node: a restarted
    /// container is a different node with a new node id.
    pub async fn stop(&self) -> Result<()> {
        self.container
            .stop()
            .await
            .with_context(|| format!("stopping {}", self.label))
    }

    /// Whether this node's process is still running, as the daemon sees it.
    ///
    /// Asked of the daemon rather than of the network: a stopped container's
    /// published port is released, and the next container to start can be
    /// given it, so a request to the address this node used can be answered
    /// by a live node of another test.
    pub async fn is_running(&self) -> Result<bool> {
        self.container
            .is_running()
            .await
            .with_context(|| format!("asking whether {} still runs", self.label))
    }

    /// The tail of this node's log, for a wait that ran out of budget: the
    /// answer alone says the value never arrived, and the log says what the
    /// node was doing instead.
    pub async fn diagnostics(&self) -> String {
        format!(
            "{}{}",
            self.replaced,
            log_tail(&self.container, &self.label).await
        )
    }

    /// Send one request to this node's surface.
    pub async fn request(&self, method: Method, path: &str, body: Bytes) -> Result<Answer> {
        let method = reqwest::Method::from_bytes(method.as_str().as_bytes())?;
        let response = self
            .client
            .request(method, format!("{}{path}", self.base))
            .body(body)
            .send()
            .await
            .with_context(|| format!("{} did not answer {path}", self.label))?;
        let status = StatusCode::from_u16(response.status().as_u16())?;
        let body = response.bytes().await?;
        Ok(Answer { status, body })
    }
}

/// The tail of a container's log, for a wait that ran out of budget: the
/// answer alone says the value never arrived, and the log says what the node
/// was doing instead.
async fn log_tail(container: &ContainerAsync<GenericImage>, label: &str) -> String {
    let mut out = Vec::new();
    for stream in [
        container.stdout_to_vec().await,
        container.stderr_to_vec().await,
    ] {
        match stream {
            Ok(bytes) => out.extend_from_slice(&bytes),
            Err(err) => return format!("[{label}] log unavailable: {err}"),
        }
    }
    let tail = out.len().saturating_sub(LOG_TAIL_BYTES);
    let text = String::from_utf8_lossy(out.get(tail..).unwrap_or(&out)).into_owned();
    format!("[{label}] last {} bytes of log:\n{text}", text.len())
}

/// A consumer that writes one container's whole log to a file named after
/// it, under [`NODE_LOG_DIR`]. Best effort throughout: a log that cannot be
/// written must never decide the outcome of a scenario. The file is opened
/// once and held, rather than reopened per frame, because a node under
/// `debug` produces frames steadily for as long as it lives.
fn stream_to_file(container_name: &str) -> impl Fn(&LogFrame) + Send + Sync {
    let _ = std::fs::create_dir_all(NODE_LOG_DIR);
    let path = std::path::Path::new(NODE_LOG_DIR).join(format!("{container_name}.log"));
    let sink = std::sync::Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok(),
    );
    move |frame| {
        use std::io::Write as _;
        let (LogFrame::StdOut(bytes) | LogFrame::StdErr(bytes)) = frame;
        if let Ok(mut guard) = sink.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = file.write_all(bytes);
            }
        }
    }
}

/// Append one replacement to [`REPLACEMENT_LOG`], best effort: the file is
/// evidence about the harness, and failing to write it must never decide the
/// outcome of a scenario. Tests run a process each and append, so the line is
/// written whole in one call rather than built up across several.
fn record_replacement(note: &str) {
    use std::io::Write as _;
    let Some(dir) = std::path::Path::new(REPLACEMENT_LOG).parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(REPLACEMENT_LOG)
    {
        let _ = file.write_all(note.as_bytes());
    }
}

/// Whether the node accepts a connection on its own loopback, asked from
/// inside the container — the one thing an outside observer cannot see.
/// The log says the listener bound; a forward that never came up and an
/// accept loop that died look identical from the test host, and this tells
/// them apart. `bash` is in the image and `/dev/tcp` is one of its builtins,
/// so asking needs nothing installed.
async fn accepts_from_inside(container: &ContainerAsync<GenericImage>) -> String {
    let probe = ExecCommand::new([
        "bash",
        "-c",
        &format!("exec 3<>/dev/tcp/127.0.0.1/{HTTP_PORT_NUM}"),
    ]);
    match container.exec(probe).await {
        Ok(result) => match result.exit_code().await {
            Ok(Some(0)) => "yes".to_owned(),
            Ok(Some(code)) => format!("no, refused (exit {code})"),
            Ok(None) => "unknown, probe still running".to_owned(),
            Err(err) => format!("unknown, probe unreadable ({err})"),
        },
        Err(err) => format!("unknown, probe unrunnable ({err})"),
    }
}

/// Wait until this node answers liveness, and hand back the address that
/// answered.
///
/// The address is resolved on every attempt rather than once before the
/// loop. What the daemon reports at start is not always the mapping that
/// ends up serving, and an address fixed before the first probe leaves the
/// wait dialing a dead port for the whole budget with no way back — a node
/// that came up in under a millisecond then reads as one that never came up
/// at all. The failure names both addresses, so a mapping that moved is
/// visible in the error instead of having to be guessed at.
async fn wait_live(
    container: &ContainerAsync<GenericImage>,
    label: &str,
    client: &reqwest::Client,
) -> Result<String> {
    let deadline = std::time::Instant::now() + READY_BUDGET;
    // An address the daemon cannot state yet is waited for like any other
    // unready thing, inside the same budget. Asked once at the top, this
    // answers "does not expose port 3011/tcp" often enough to be seen in
    // fifty runs: the port state is not always settled by the time `start`
    // returns, and a container is a costly thing to throw away over it.
    let mut first = None;
    loop {
        let resolved = base_url(container).await;
        if first.is_none() {
            if let Ok(ref base) = resolved {
                first = Some(base.clone());
            }
        }
        if let Ok(ref base) = resolved {
            if let Ok(response) = client.get(format!("{base}/live")).send().await {
                if response.status().as_u16() == StatusCode::OK.as_u16() {
                    return Ok(base.clone());
                }
            }
        }
        if std::time::Instant::now() > deadline {
            let now = base_url(container)
                .await
                .unwrap_or_else(|err| format!("<unresolvable: {err}>"));
            let first = first.unwrap_or_else(|| "<never resolved>".to_owned());
            return Err(anyhow::anyhow!(
                "{label} never answered /live within {READY_BUDGET:?} \
                 (first dialed {first}, now resolves to {now}, \
                 accepts on its own loopback: {})\n{}",
                accepts_from_inside(container).await,
                log_tail(container, label).await
            ));
        }
        tokio::time::sleep(READY_POLL).await;
    }
}

/// The verbs the surface answers to.
#[derive(Clone, Copy)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

/// Where a test reaches a container. The address comes from the container
/// client's own host resolution rather than an assumed loopback, because the
/// two are not always the same host: a suite run inside the development
/// container talks to a sibling on the host's daemon, whose published port is
/// on the host, not on the development container.
///
/// The client resolves this itself, with nothing set by hand: over a socket
/// it answers loopback, unless it finds itself inside a container, and then
/// it answers the gateway of the daemon's `bridge` network — the host's
/// address on that bridge. That address carries the sibling's port only
/// because the daemon publishes it on every interface, so narrowing the
/// daemon's default publish address to loopback would cut the path from the
/// development container while leaving the one from the host intact.
async fn base_url(container: &ContainerAsync<GenericImage>) -> Result<String> {
    let host = container.get_host().await?.to_string();
    let port = container.get_host_port_ipv4(HTTP_PORT).await?;
    // A bare IPv6 address needs brackets before it can carry a port.
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    Ok(format!("http://{host}:{port}"))
}

/// The claim set covering exactly `path` of `issuer`'s namespace — read
/// always, write when `write`. Deriving a claim identity is arithmetic on
/// the issuer and the path, not a reach into the runtime.
pub fn claims_on(issuer: PdnId, path: &str, write: bool) -> Result<Vec<pdn_node::GrantedClaim>> {
    Ok(vec![pdn_node::GrantedClaim {
        claim: pdn_node::claim_id_of(&issuer, &pdn_node::EntryPath::new(path)?),
        write,
    }])
}

/// A grant publication of `issuer`'s own data on exactly `path`.
pub fn grant_on(issuer: PdnId, path: &str, write: bool) -> Result<GrantPublication> {
    Ok(GrantPublication {
        issuer,
        claims: claims_on(issuer, path, write)?,
    })
}

/// The body a caller must send to write nothing but bytes.
pub fn body(payload: &[u8]) -> Bytes {
    Bytes::copy_from_slice(payload)
}

/// Poll `check` every 100ms until it holds or `budget` elapses; the return
/// says whether it was observed in time. Repeating the read is the only wait
/// the surface offers, by design — nothing here forces a reconciliation.
pub async fn eventually<F, Fut>(budget: Duration, mut check: F) -> Result<bool>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    let deadline = std::time::Instant::now() + budget;
    loop {
        if check().await? {
            return Ok(true);
        }
        if std::time::Instant::now() > deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll until reading `path` under `issuer` on this host answers exactly
/// `expected`; the failure names the last answer, because "not yet" and
/// "refused" are the distinction this surface exists to preserve and a bare
/// timeout would collapse them.
pub async fn entry_reads(host: &Host, issuer: PdnId, path: &str, expected: &[u8]) -> Result<()> {
    poll_read(host, issuer, path, |answer| {
        answer.status == StatusCode::OK && answer.body == expected
    })
    .await
    .with_context(|| format!("{path} under {issuer} never read back as expected"))
}

/// Poll until reading `path` under `issuer` on this host answers `status` —
/// the wait for a read to stop working, as after a withdrawal.
pub async fn entry_answers(
    host: &Host,
    issuer: PdnId,
    path: &str,
    status: StatusCode,
) -> Result<()> {
    poll_read(host, issuer, path, |answer| answer.status == status)
        .await
        .with_context(|| format!("reading {path} under {issuer} never answered {status}"))
}

/// Repeat the read until `holds`, carrying the last answer into the error.
async fn poll_read(
    host: &Host,
    issuer: PdnId,
    path: &str,
    holds: impl Fn(&Answer) -> bool,
) -> Result<()> {
    let encoded = encode_path(path);
    let deadline = std::time::Instant::now() + CONVERGENCE_BUDGET;
    loop {
        let answer = host.get(&format!("/debug/data/{issuer}/{encoded}")).await?;
        if holds(&answer) {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err(anyhow::anyhow!(
                "last answer was {}: {}\n{}",
                answer.status,
                answer.text(),
                // The node that was read from, not the one written to: the
                // answer says the value never arrived, and this says what
                // this node was doing instead of receiving it.
                host.diagnostics().await
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// An entry path as a URL path: every component percent-encoded, the
/// separators kept.
pub fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|component| {
            percent_encoding::utf8_percent_encode(component, percent_encoding::NON_ALPHANUMERIC)
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("/")
}
