//! The stand: nodes as containers on one network, reached over their
//! published HTTP ports and never told each other's address. Nothing here
//! calls a runtime service directly. The image is not built here: `just
//! test-docker` builds it, resolves it to a content id, and hands that
//! over.
// Each test binary uses its own subset of the helpers.
#![allow(dead_code)]

use std::{
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{ensure, Context as _, Result};
use axum::{body::Bytes, http::StatusCode};
use pdn_node::{
    observation::{ListedEntry, PodRecord},
    PdnId, PodId, RecordKind, RecordRef,
};
use pdn_node_http::shapes::{Act, CreatedIdentity, HeldPod, HeldPods, Member, Members, Operations};
use serde::de::DeserializeOwned;
use testcontainers::{
    core::{logs::LogFrame, ContainerPort, ExecCommand, Mount, WaitFor},
    runners::AsyncRunner as _,
    ContainerAsync, GenericImage, ImageExt as _,
};

/// The image `just test-docker` builds.
pub const IMAGE_NAME: &str = "pdn-node-http";
pub const IMAGE_TAG: &str = "dev";

/// Where the recipe hands over the identity of the image this run tests.
const IMAGE_ENV: &str = "PDN_STAND_IMAGE";

/// The same for the image built with the observation routes.
const OBSERVATION_IMAGE_ENV: &str = "PDN_STAND_OBSERVATION_IMAGE";
const OBSERVATION_IMAGE_TAG: &str = "observation";

/// Which of the run's two images a node starts from.
#[derive(Debug, Clone, Copy)]
pub enum Image {
    Stand,
    Observation,
}

/// The image every container of this run starts from: a content id the
/// recipe resolved once, since a tag can move mid-run from another worktree
/// sharing the daemon. Unset, the tag — a run started by hand still works.
fn image_ref(image: Image) -> String {
    let (env, tag) = match image {
        Image::Stand => (IMAGE_ENV, IMAGE_TAG),
        Image::Observation => (OBSERVATION_IMAGE_ENV, OBSERVATION_IMAGE_TAG),
    };
    std::env::var(env).unwrap_or_else(|_| format!("{IMAGE_NAME}:{tag}"))
}

/// Split the way the container client wants it; a content id (`sha256:…`)
/// splits like a tag does.
fn split_ref(image: &str) -> (&str, &str) {
    image.rsplit_once(':').unwrap_or((image, "latest"))
}

/// The runtime's own endpoint port is never published.
const HTTP_PORT_NUM: u16 = 3011;
const HTTP_PORT: ContainerPort = ContainerPort::Tcp(HTTP_PORT_NUM);

/// Not a margin for slowness — a healthy node binds within a millisecond
/// of its store opening — but what a container that will never answer
/// costs before it is replaced.
const READY_BUDGET: Duration = Duration::from_secs(20);
const READY_POLL: Duration = Duration::from_millis(250);

/// Wider than [`READY_BUDGET`]: a restarted node recovers its hosted
/// identities before the listener binds, and a restart cannot be answered
/// by replacement — the state directory is the subject. The narrow budget
/// was measured failing under the stress pass.
const RESTART_BUDGET: Duration = Duration::from_mins(1);

/// A probe that connects and never answers would otherwise hold the wait
/// open past any budget.
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Against the hang, not slowness: a container torn down mid-request can
/// leave the daemon's port-forwarder black-holing the connection. Sized
/// above every request this suite makes (the ceremony routes bound
/// themselves by `timeout_secs`, 60 seconds at most).
const REQUEST_BUDGET: Duration = Duration::from_mins(2);

/// A published port that never answers is an arrangement that failed, so
/// the container is replaced rather than reported.
const SPAWN_ATTEMPTS: usize = 2;

/// A replacement leaves no other trace on a run that then passes, and a
/// green suite must be told apart from one whose failures were absorbed.
/// Absolute: a relative path would follow each test process's own
/// directory, the package root.
const REPLACEMENT_LOG: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/stand-replacements.log");

/// Every node's whole log, one file per container: the tail carried into a
/// failing wait misses plain assertions and replaced containers.
const NODE_LOG_DIR: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/stand-logs");

/// Several of the runtime's reconciliation periods, since nothing here
/// shortens that cadence.
pub const CONVERGENCE_BUDGET: Duration = Duration::from_mins(2);

const LOG_TAIL_BYTES: usize = 4 * 1024;

/// One test's container network, so a container left behind by a failed
/// run cannot be dialed by the next one. Created with the first node and
/// removed with the last, by the container client itself.
pub struct Stand {
    network: String,
}

impl Stand {
    /// The name carries the process id: the runner gives each test a
    /// process.
    pub fn new() -> Self {
        static STANDS: AtomicUsize = AtomicUsize::new(0);
        let ordinal = STANDS.fetch_add(1, Ordering::Relaxed);
        Self {
            network: format!("pdn-stand-{}-{ordinal}", std::process::id()),
        }
    }

    /// Debug surface on; waits until the node answers liveness.
    pub async fn spawn(&self, label: &str) -> Result<Host> {
        self.spawn_configured(label, Image::Stand, true, None, &[])
            .await
    }

    /// Debug surface off — the image's own default.
    pub async fn spawn_without_debug(&self, label: &str) -> Result<Host> {
        self.spawn_configured(label, Image::Stand, false, None, &[])
            .await
    }

    /// A tmpfs of `state_bytes` over the image's state directory, so the
    /// store meets a disk that fills.
    pub async fn spawn_with_bounded_state(&self, label: &str, state_bytes: i64) -> Result<Host> {
        self.spawn_configured(label, Image::Stand, true, Some(state_bytes), &[])
            .await
    }

    /// From `image`, with `PDN_DEBUG=1` when `debug` and `env` beside it.
    pub async fn spawn_from(
        &self,
        label: &str,
        image: Image,
        debug: bool,
        env: &[(&str, &str)],
    ) -> Result<Host> {
        self.spawn_configured(label, image, debug, None, env).await
    }

    /// A node from the stand's image with `env` that must stop its start:
    /// the exit code and the log, once the process has exited.
    pub async fn refused_start(&self, label: &str, env: &[(&str, &str)]) -> Result<(i64, String)> {
        let reference = image_ref(Image::Stand);
        let (image_name, image_tag) = split_ref(&reference);
        let name = format!("{}-{label}", self.network);
        let mut image = GenericImage::new(image_name, image_tag)
            .with_exposed_port(HTTP_PORT)
            .with_wait_for(WaitFor::Nothing)
            .with_env_var("PDN_DEBUG", "1")
            .with_network(&self.network)
            .with_log_consumer(stream_to_file(&name))
            .with_container_name(name);
        for (key, value) in env {
            image = image.with_env_var(*key, *value);
        }
        let container = image
            .start()
            .await
            .with_context(|| format!("starting {label}: is {reference} built?"))?;
        let deadline = tokio::time::Instant::now() + READY_BUDGET;
        while container.is_running().await? {
            ensure!(
                tokio::time::Instant::now() < deadline,
                "{label} still runs {READY_BUDGET:?} after its start"
            );
            tokio::time::sleep(READY_POLL).await;
        }
        let output = std::process::Command::new("docker")
            .args(["inspect", "-f", "{{.State.ExitCode}}", container.id()])
            .output()
            .with_context(|| format!("reading the exit code of {label}"))?;
        let code = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .with_context(|| format!("the exit code of {label} is not a number"))?;
        Ok((code, log_tail(&container, label).await))
    }

    async fn spawn_configured(
        &self,
        label: &str,
        image: Image,
        debug: bool,
        state_bytes: Option<i64>,
        env: &[(&str, &str)],
    ) -> Result<Host> {
        let mut failed = None;
        let mut replaced = String::new();
        let reference = image_ref(image);
        let (image_name, image_tag) = split_ref(&reference);
        for attempt in 1..=SPAWN_ATTEMPTS {
            // A replacement must not collide with a predecessor the daemon
            // has not finished removing.
            let name = match attempt {
                1 => format!("{}-{label}", self.network),
                n => format!("{}-{label}-{n}", self.network),
            };
            let mut image = GenericImage::new(image_name, image_tag)
                .with_exposed_port(HTTP_PORT)
                .with_wait_for(WaitFor::Nothing)
                // The environment is stated, never inherited: the runtime's
                // endpoint bind stays unset so it publishes the container's
                // own address.
                .with_env_var("RUST_LOG", "info,pdn_node=debug,data_layer=debug")
                .with_network(&self.network)
                .with_log_consumer(stream_to_file(&name))
                .with_container_name(name.clone());
            if debug {
                image = image.with_env_var("PDN_DEBUG", "1");
            }
            for (key, value) in env {
                image = image.with_env_var(*key, *value);
            }
            if let Some(state_bytes) = state_bytes {
                // Mode 1777: the tmpfs arrives root-owned and the binary
                // runs as the image's non-root user.
                image = image.with_mount(
                    Mount::tmpfs_mount("/var/lib/pdn")
                        .with_size_bytes(state_bytes)
                        .with_mode(0o1777),
                );
            }
            let container = image
                .start()
                .await
                .with_context(|| format!("starting {label}: is {reference} built?"))?;
            let client = stand_client()?;
            match wait_live(&container, label, &client, READY_BUDGET).await {
                Ok(base) => {
                    return Ok(Host {
                        label: label.to_owned(),
                        name,
                        base: std::sync::Mutex::new(base),
                        client,
                        container,
                        replaced,
                    })
                }
                Err(err) => {
                    // On the node for the diagnostics of a later failure, and
                    // in a file as the only trace a passing run leaves.
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

pub struct Answer {
    pub status: StatusCode,
    pub body: Bytes,
}

impl Answer {
    /// A refusal fails here, naming its status and the runtime's own text.
    pub fn ok(self) -> Result<Bytes> {
        ensure!(
            self.status.is_success(),
            "expected success, got {}: {}",
            self.status,
            self.text()
        );
        Ok(self.body)
    }

    pub fn json<T: DeserializeOwned>(self) -> Result<T> {
        let body = self.ok()?;
        serde_json::from_slice(&body)
            .with_context(|| format!("undecodable answer: {}", String::from_utf8_lossy(&body)))
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// One node of the stand: a container and the client that reaches it.
pub struct Host {
    label: String,
    /// Also the log file's name, needed to re-attach capture after a restart.
    name: String,
    /// Behind a lock because a restart moves it: the port a stopped
    /// container comes back on is a different one.
    base: std::sync::Mutex<String>,
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

    pub async fn create_identity(&self) -> Result<PdnId> {
        let created: CreatedIdentity =
            self.post("/debug/identities", Bytes::new()).await?.json()?;
        Ok(created.identity)
    }

    pub async fn create_pod(&self, identity: PdnId) -> Result<PodId> {
        let created: HeldPod = self
            .post(&format!("/debug/identities/{identity}/pods"), Bytes::new())
            .await?
            .json()?;
        Ok(created.pod)
    }

    pub async fn pod_members(&self, identity: PdnId, pod: PodId) -> Result<Vec<Member>> {
        let members: Members = self
            .get(&format!("{}/members", pod_route(identity, pod)))
            .await?
            .json()?;
        Ok(members.members)
    }

    /// The payload carries a live one-time secret.
    pub async fn invite_to_pod(&self, identity: PdnId, pod: PodId) -> Result<Bytes> {
        self.post(
            &format!("{}/invites", pod_route(identity, pod)),
            Bytes::new(),
        )
        .await?
        .ok()
    }

    pub async fn join_pod(&self, identity: PdnId, invite: Bytes) -> Result<Answer> {
        self.post(&format!("/debug/identities/{identity}/pods/join"), invite)
            .await
    }

    pub async fn pod_act(&self, identity: PdnId, pod: PodId, act: Act) -> Result<Answer> {
        self.post(
            &format!("{}/acts", pod_route(identity, pod)),
            serde_json::to_vec(&act)?,
        )
        .await
    }

    pub async fn place_record(
        &self,
        identity: PdnId,
        pod: PodId,
        kind: RecordKind,
        payload: &[u8],
    ) -> Result<RecordRef> {
        self.post(
            &format!("{}/records?kind={kind}", pod_route(identity, pod)),
            body(payload),
        )
        .await?
        .json()
    }

    pub async fn append_op(
        &self,
        identity: PdnId,
        pod: PodId,
        record: RecordRef,
        op: &[u8],
    ) -> Result<Answer> {
        self.post(
            &format!("{}/ops", record_route(identity, pod, record)),
            body(op),
        )
        .await
    }

    /// This node's endpoint id, from the status page's `node` line.
    pub async fn node_id(&self) -> Result<String> {
        let status = self.get("/debug/status").await?.ok()?;
        String::from_utf8_lossy(&status)
            .lines()
            .find_map(|line| line.strip_prefix("node "))
            .map(str::to_owned)
            .context("the status page carries no node line")
    }

    pub async fn listing(&self, identity: PdnId, pod: PodId) -> Result<Vec<ListedEntry>> {
        self.get(&format!("{}/listing", pod_route(identity, pod)))
            .await?
            .json()
    }

    /// The event route of `identity` on `pod`.
    pub async fn events(&self, identity: PdnId, pod: PodId) -> Result<RecordStream> {
        self.stream(
            Method::Get,
            &format!("{}/events", pod_route(identity, pod)),
            Bytes::new(),
        )
        .await?
        .map_err(|refused| {
            anyhow::anyhow!(
                "{} refused the event route: {}: {}",
                self.label,
                refused.status,
                refused.text()
            )
        })
    }

    /// The observed join route: the stream, or the refusal it answered.
    pub async fn join_pod_observed(
        &self,
        identity: PdnId,
        invite: Bytes,
    ) -> Result<std::result::Result<RecordStream, Answer>> {
        self.stream(
            Method::Post,
            &format!("/debug/identities/{identity}/pods/join-observed"),
            invite,
        )
        .await
    }

    /// A response read as it arrives, by a client with no overall timeout:
    /// a stream outlives [`REQUEST_BUDGET`]. A non-success answer is read
    /// whole.
    async fn stream(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
    ) -> Result<std::result::Result<RecordStream, Answer>> {
        let method = reqwest::Method::from_bytes(method.as_str().as_bytes())?;
        let base = self
            .base
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("base-url lock poisoned"))?
            .clone();
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(plain_http_tls()?)
            .connect_timeout(READY_PROBE_TIMEOUT)
            .build()
            .context("building the stream client")?;
        let response = tokio::time::timeout(
            REQUEST_BUDGET,
            client
                .request(method, format!("{base}{path}"))
                .body(body)
                .send(),
        )
        .await
        .with_context(|| format!("{} sent no headers for {path}", self.label))?
        .with_context(|| format!("{} did not answer {path}", self.label))?;
        let status = StatusCode::from_u16(response.status().as_u16())?;
        if !status.is_success() {
            let body = response.bytes().await?;
            return Ok(Err(Answer { status, body }));
        }
        let pod = response
            .headers()
            .get(pdn_node_http::POD_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Ok(Ok(RecordStream {
            label: self.label.clone(),
            pod,
            response,
            pending: Vec::new(),
        }))
    }

    /// The container's filesystem stays for a later [`start`](Self::start).
    pub async fn stop(&self) -> Result<()> {
        self.container
            .stop()
            .await
            .with_context(|| format!("stopping {}", self.label))
    }

    /// `SIGKILL`, through the CLI on purpose: the client's own
    /// stop-with-zero-timeout still leads with the stop signal.
    pub fn kill(&self) -> Result<()> {
        let output = std::process::Command::new("docker")
            .args(["kill", self.container.id()])
            .output()
            .with_context(|| format!("killing {}", self.label))?;
        ensure!(
            output.status.success(),
            "killing {} failed: {}",
            self.label,
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    /// Start the stopped container and re-read its address from the daemon.
    /// Two attempts: the daemon has been seen re-using the previous host
    /// port while its forwarder black-holes it, and a second stop and start
    /// re-rolls the mapping. Recorded like a replacement.
    pub async fn start(&self) -> Result<()> {
        let mut last = None;
        for attempt in 1..=SPAWN_ATTEMPTS {
            self.container
                .start()
                .await
                .with_context(|| format!("starting {}", self.label))?;
            match wait_live(&self.container, &self.label, &self.client, RESTART_BUDGET).await {
                Ok(base) => {
                    follow_logs_into_file(&self.container, &self.name);
                    *self
                        .base
                        .lock()
                        .map_err(|_poisoned| anyhow::anyhow!("base-url lock poisoned"))? = base;
                    return Ok(());
                }
                Err(err) => {
                    if attempt < SPAWN_ATTEMPTS {
                        record_replacement(&format!(
                            "[{}] restarted container's published port never answered; \
                             stopped and started again. {err:#}\n",
                            self.label
                        ));
                        self.container
                            .stop()
                            .await
                            .with_context(|| format!("re-stopping {}", self.label))?;
                    }
                    last = Some(err);
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("{} was never started", self.label)))
    }

    /// Tells a process that left by its own shutdown path (0) from one the
    /// daemon killed when the grace ran out (137).
    pub fn exit_code(&self) -> Result<i64> {
        let output = std::process::Command::new("docker")
            .args(["inspect", "-f", "{{.State.ExitCode}}", self.container.id()])
            .output()
            .with_context(|| format!("reading the exit code of {}", self.label))?;
        ensure!(
            output.status.success(),
            "reading the exit code of {} failed: {}",
            self.label,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .with_context(|| format!("the exit code of {} is not a number", self.label))
    }

    /// Asked of the daemon rather than the network: a released port can be
    /// answered by a live node of another test.
    pub async fn is_running(&self) -> Result<bool> {
        self.container
            .is_running()
            .await
            .with_context(|| format!("asking whether {} still runs", self.label))
    }

    /// For a wait that ran out of budget: what the node was doing instead.
    pub async fn diagnostics(&self) -> String {
        format!(
            "{}{}",
            self.replaced,
            log_tail(&self.container, &self.label).await
        )
    }

    pub async fn request(&self, method: Method, path: &str, body: Bytes) -> Result<Answer> {
        let method = reqwest::Method::from_bytes(method.as_str().as_bytes())?;
        let base = self
            .base
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("base-url lock poisoned"))?
            .clone();
        let response = self
            .client
            .request(method, format!("{base}{path}"))
            .body(body)
            .send()
            .await
            .with_context(|| format!("{} did not answer {path}", self.label))?;
        let status = StatusCode::from_u16(response.status().as_u16())?;
        let body = response.bytes().await?;
        Ok(Answer { status, body })
    }
}

/// An event route's body, one [`PodRecord`] per line.
pub struct RecordStream {
    label: String,
    /// The observed join's pod header.
    pub pod: Option<String>,
    response: reqwest::Response,
    pending: Vec<u8>,
}

pub enum Read {
    Record(Box<PodRecord>),
    /// The body ended.
    Ended,
    /// Nothing arrived within the wait.
    Quiet,
}

impl RecordStream {
    pub async fn read(&mut self, wait: Duration) -> Result<Read> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=end).collect();
                let record = serde_json::from_slice(line.get(..end).unwrap_or_default())
                    .with_context(|| {
                        format!(
                            "{} wrote a line that is no record: {}",
                            self.label,
                            String::from_utf8_lossy(&line)
                        )
                    })?;
                return Ok(Read::Record(Box::new(record)));
            }
            let Ok(chunk) = tokio::time::timeout_at(deadline, self.response.chunk()).await else {
                return Ok(Read::Quiet);
            };
            let Some(chunk) = chunk.with_context(|| format!("{}'s stream broke", self.label))?
            else {
                ensure!(
                    self.pending.is_empty(),
                    "{}'s stream ended mid-line",
                    self.label
                );
                return Ok(Read::Ended);
            };
            self.pending.extend_from_slice(&chunk);
        }
    }

    /// Reads up to and including the first record `until` accepts, within
    /// `budget`; a stream that ends or stays quiet first fails, naming
    /// `what`.
    pub async fn read_until(
        &mut self,
        budget: Duration,
        what: &str,
        mut until: impl FnMut(&PodRecord) -> bool,
    ) -> Result<Vec<PodRecord>> {
        let deadline = tokio::time::Instant::now() + budget;
        let mut read = Vec::new();
        loop {
            let wait = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self.read(wait).await? {
                Read::Record(record) => {
                    let done = until(&record);
                    read.push(*record);
                    if done {
                        return Ok(read);
                    }
                }
                Read::Ended => anyhow::bail!(
                    "{}'s stream ended before {what}, after {} records",
                    self.label,
                    read.len()
                ),
                Read::Quiet => anyhow::bail!(
                    "{}'s stream yielded no {what} within {budget:?}, after {} records",
                    self.label,
                    read.len()
                ),
            }
        }
    }
}

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

/// One client per node with a bounded per-request timeout; see
/// [`REQUEST_BUDGET`].
fn stand_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .tls_backend_preconfigured(plain_http_tls()?)
        .timeout(REQUEST_BUDGET)
        .build()
        .context("building the stand's HTTP client")
}

/// iroh 1.3 turns on reqwest's `rustls-no-provider`, and cargo unifies it into the stand's client:
/// without a TLS configuration of its own the client panics, plain HTTP included, although `ring`
/// is the one provider in the build — under that feature reqwest takes only a provider installed
/// process-wide, never the one rustls's own features name. iroh keeps the feature on purpose and
/// configures each of its own clients instead, and its maintainers decline to change that:
/// <https://github.com/n0-computer/iroh/issues/4442#issuecomment-5089223911>. This configuration
/// is the one iroh's relay tests build; its empty root store trusts no server.
fn plain_http_tls() -> Result<rustls::ClientConfig> {
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("ring's default protocol versions")?
    .with_root_certificates(rustls::RootCertStore::empty())
    .with_no_client_auth())
}

/// Re-attach log capture after a restart: the capture installed at creation
/// ends with the first run of the process. The file is rewritten whole from
/// the container's first frame, so it stays one history in order. Best
/// effort: a log that cannot be written must never decide a scenario.
fn follow_logs_into_file(container: &ContainerAsync<GenericImage>, container_name: &str) {
    use tokio::io::AsyncReadExt as _;

    let _ = std::fs::create_dir_all(NODE_LOG_DIR);
    let path = std::path::Path::new(NODE_LOG_DIR).join(format!("{container_name}.log"));
    let Ok(file) = std::fs::File::create(&path) else {
        return;
    };
    let sink = Arc::new(std::sync::Mutex::new(file));
    for mut stream in [container.stdout(true), container.stderr(true)] {
        let sink = Arc::clone(&sink);
        let _detached = tokio::spawn(async move {
            let mut buffer = [0u8; 8 * 1024];
            loop {
                let read = match stream.read(&mut buffer).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => read,
                };
                let Some(frame) = buffer.get(..read) else {
                    return;
                };
                if let Ok(mut file) = sink.lock() {
                    use std::io::Write as _;
                    let _ = file.write_all(frame);
                }
            }
        });
    }
}

/// Stream one container's log to a file under [`NODE_LOG_DIR`], best
/// effort. Opened once and held: a node under `debug` produces frames
/// steadily.
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

/// Append one replacement to [`REPLACEMENT_LOG`], best effort. Every line
/// stands alone, so an interleaving of processes costs legibility only.
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

/// Whether the node accepts on its own loopback, asked from inside the
/// container: a forward that never came up and an accept loop that died
/// look identical from the test host. `/dev/tcp` is a `bash` builtin, so
/// asking needs nothing installed.
async fn accepts_from_inside(container: &ContainerAsync<GenericImage>) -> String {
    let probe = ExecCommand::new([
        "bash",
        "-c",
        &format!("exec 3<>/dev/tcp/127.0.0.1/{HTTP_PORT_NUM}"),
    ]);
    match container.exec(probe).await {
        Ok(mut result) => {
            // The daemon reports an exit code only once the exec's output
            // streams are consumed.
            use tokio::io::AsyncReadExt as _;
            let mut drained = Vec::new();
            let _ = result.stdout().read_to_end(&mut drained).await;
            let _ = result.stderr().read_to_end(&mut drained).await;
            match result.exit_code().await {
                Ok(Some(0)) => "yes".to_owned(),
                Ok(Some(code)) => format!("no, refused (exit {code})"),
                Ok(None) => "unknown, probe still running".to_owned(),
                Err(err) => format!("unknown, probe unreadable ({err})"),
            }
        }
        Err(err) => format!("unknown, probe unrunnable ({err})"),
    }
}

/// Wait until the node answers liveness, and hand back the address that
/// answered. Resolved on every attempt: what the daemon reports at start is
/// not always the mapping that ends up serving, and the failure names both
/// addresses so a mapping that moved is visible.
async fn wait_live(
    container: &ContainerAsync<GenericImage>,
    label: &str,
    client: &reqwest::Client,
    budget: Duration,
) -> Result<String> {
    let deadline = std::time::Instant::now() + budget;
    // An address the daemon cannot state yet is waited for inside the same
    // budget: the port state is not always settled when `start` returns.
    let mut first = None;
    loop {
        let resolved = base_url(container).await;
        if first.is_none() {
            if let Ok(ref base) = resolved {
                first = Some(base.clone());
            }
        }
        if let Ok(ref base) = resolved {
            if let Ok(response) = client
                .get(format!("{base}/live"))
                .timeout(READY_PROBE_TIMEOUT)
                .send()
                .await
            {
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
                "{label} never answered /live within {budget:?} \
                 (first dialed {first}, now resolves to {now}, \
                 accepts on its own loopback: {})\n{}",
                accepts_from_inside(container).await,
                log_tail(container, label).await
            ));
        }
        tokio::time::sleep(READY_POLL).await;
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

/// The container client's own host resolution rather than an assumed
/// loopback: a suite run inside the development container reaches a
/// sibling through the gateway of the daemon's `bridge` network, which
/// carries the port only because the daemon publishes on every interface.
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

pub fn body(payload: &[u8]) -> Bytes {
    Bytes::copy_from_slice(payload)
}

/// Poll `check` every 100ms until it yields a value or `budget` elapses.
/// The value comes out of the poll: a read taken afterwards is a second
/// observation of a moving replica. The budget bounds each observation as
/// well as the wait, so a check still in flight at the deadline is dropped
/// — reads only; a mutating call would be dropped mid-request.
pub async fn eventually<F, Fut, T>(budget: Duration, mut check: F) -> Result<Option<T>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        match tokio::time::timeout_at(deadline, check()).await {
            Ok(result) => {
                if let Some(value) = result? {
                    return Ok(Some(value));
                }
            }
            Err(_elapsed) => return Ok(None),
        }
        if tokio::time::Instant::now() > deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub fn pod_route(identity: PdnId, pod: PodId) -> String {
    format!("/debug/identities/{identity}/pods/{pod}")
}

pub fn record_route(identity: PdnId, pod: PodId, record: RecordRef) -> String {
    format!(
        "{}/records/{}/{}/{}",
        pod_route(identity, pod),
        record.member,
        record.kind,
        record.id
    )
}

/// Poll until `identity`'s replica lists exactly `expected` as the pod's
/// members, in any order.
pub async fn members_read(
    host: &Host,
    identity: PdnId,
    pod: PodId,
    expected: &[Member],
) -> Result<()> {
    let mut expected = expected.to_vec();
    expected.sort_by_key(|member| member.id);
    let route = format!("{}/members", pod_route(identity, pod));
    poll_route(host, &route, |answer| {
        answer.status == StatusCode::OK
            && serde_json::from_slice::<Members>(&answer.body).is_ok_and(|mut listed| {
                listed.members.sort_by_key(|member| member.id);
                listed.members == expected
            })
    })
    .await
    .with_context(|| format!("{pod} never listed {expected:?} as its members for {identity}"))
}

pub async fn record_reads(
    host: &Host,
    identity: PdnId,
    pod: PodId,
    record: RecordRef,
    expected: &[u8],
) -> Result<()> {
    record_reads_within(host, identity, pod, record, expected, CONVERGENCE_BUDGET).await
}

/// [`record_reads`] polled for `budget`.
pub async fn record_reads_within(
    host: &Host,
    identity: PdnId,
    pod: PodId,
    record: RecordRef,
    expected: &[u8],
    budget: Duration,
) -> Result<()> {
    poll_route_within(
        host,
        &record_route(identity, pod, record),
        budget,
        |answer| answer.status == StatusCode::OK && answer.body == expected,
    )
    .await
    .with_context(|| format!("{record:?} never read back as expected for {identity}"))
}

/// Poll until the document's operations are exactly `expected`, each a
/// writer and its payload, in any order.
pub async fn ops_read(
    host: &Host,
    identity: PdnId,
    pod: PodId,
    record: RecordRef,
    expected: &[(PdnId, &[u8])],
) -> Result<()> {
    let mut expected: Vec<(PdnId, Vec<u8>)> = expected
        .iter()
        .map(|(writer, payload)| (*writer, payload.to_vec()))
        .collect();
    expected.sort();
    let route = format!("{}/ops", record_route(identity, pod, record));
    poll_route(host, &route, |answer| {
        answer.status == StatusCode::OK
            && serde_json::from_slice::<Operations>(&answer.body).is_ok_and(|read| {
                let mut read: Vec<(PdnId, Vec<u8>)> = read
                    .operations
                    .into_iter()
                    .map(|op| (op.writer, op.payload))
                    .collect();
                read.sort();
                read == expected
            })
    })
    .await
    .with_context(|| format!("the operations of {record:?} never read as expected for {identity}"))
}

/// Poll until `identity` lists `pod` when `held`, or lists it no more.
pub async fn pod_listed(host: &Host, identity: PdnId, pod: PodId, held: bool) -> Result<()> {
    let route = format!("/debug/identities/{identity}/pods");
    poll_route(host, &route, |answer| {
        answer.status == StatusCode::OK
            && serde_json::from_slice::<HeldPods>(&answer.body)
                .is_ok_and(|listed| listed.pods.contains(&pod) == held)
    })
    .await
    .with_context(|| format!("{identity} never listed {pod} as held: {held}"))
}

/// Repeat a read of `route` until `holds`, carrying the last answer into the
/// error: "no answer at all" and "the wrong answer" are different diagnoses.
async fn poll_route(host: &Host, route: &str, holds: impl Fn(&Answer) -> bool) -> Result<()> {
    poll_route_within(host, route, CONVERGENCE_BUDGET, holds).await
}

async fn poll_route_within(
    host: &Host,
    route: &str,
    budget: Duration,
    holds: impl Fn(&Answer) -> bool,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + budget;
    let mut last: Option<Answer> = None;
    loop {
        let cut_mid_request = match tokio::time::timeout_at(deadline, host.get(route)).await {
            Ok(answer) => {
                let answer = answer?;
                if holds(&answer) {
                    return Ok(());
                }
                last = Some(answer);
                false
            }
            Err(_elapsed) => true,
        };
        if cut_mid_request || tokio::time::Instant::now() > deadline {
            let diagnostics = host.diagnostics().await;
            return Err(match last {
                Some(answer) => anyhow::anyhow!(
                    "last answer was {}: {}\n{diagnostics}",
                    answer.status,
                    answer.text()
                ),
                None => {
                    anyhow::anyhow!("no answer at all within {budget:?}\n{diagnostics}")
                }
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
