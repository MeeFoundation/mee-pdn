//! Testcontainers harness for mee-demo integration tests.
//!
//! Provides [`MeeNode`] — a wrapper around a Docker container running
//! `mee-demo:dev`, with helpers for the HTTP API and node lifecycle
//! (stop / start).

// Test helper module — expect/unwrap/print are fine here.
#![allow(clippy::expect_used, clippy::print_stderr)]

use std::time::Duration;

use reqwest::Client;
use serde_json::Value;
use testcontainers::{
    core::ContainerPort, runners::AsyncRunner, ContainerAsync, GenericImage, ImageExt,
};

const IMAGE_NAME: &str = "mee-demo";
const IMAGE_TAG: &str = "dev";
const INTERNAL_PORT: u16 = 3000;
const READY_POLL_INTERVAL: Duration = Duration::from_millis(200);
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// A running mee-demo node inside a Docker container.
pub struct MeeNode {
    pub label: String,
    container: ContainerAsync<GenericImage>,
    host: String,
    /// Docker Desktop may reassign the host port after stop/start.
    host_port: std::sync::atomic::AtomicU16,
    client: Client,
}

impl MeeNode {
    /// Spawn a new node container on the given Docker network.
    ///
    /// Blocks until the `/live` health-check responds with 200 OK.
    pub async fn spawn(label: &str, network: &str) -> Self {
        Self::spawn_inner(label, network, &[("MEE_DISCOVERY", "disabled")]).await
    }

    /// Spawn a node with gossip discovery enabled and fast test timers.
    pub async fn spawn_with_gossip(label: &str, network: &str) -> Self {
        Self::spawn_inner(
            label,
            network,
            &[
                ("MEE_DISCOVERY", "gossip"),
                ("MEE_GOSSIP_REBROADCAST_SECS", "1"),
                ("MEE_GOSSIP_EVICTION_SECS", "3"),
            ],
        )
        .await
    }

    async fn spawn_inner(label: &str, network: &str, extra_env: &[(&str, &str)]) -> Self {
        let mut image = GenericImage::new(IMAGE_NAME, IMAGE_TAG)
            .with_exposed_port(ContainerPort::Tcp(INTERNAL_PORT))
            .with_env_var("MEE_HOST", "0.0.0.0")
            .with_env_var("MEE_PORT", INTERNAL_PORT.to_string())
            .with_env_var("MEE_DEBUG", "1")
            .with_network(network);

        for (k, v) in extra_env {
            image = image.with_env_var(*k, *v);
        }

        let container: ContainerAsync<GenericImage> = image.start().await.expect("start container");

        let host = container.get_host().await.expect("get host").to_string();

        let host_port = container
            .get_host_port_ipv4(INTERNAL_PORT)
            .await
            .expect("get host port");

        eprintln!("[{label}] container started: {host}:{host_port}");

        let node = Self {
            label: label.to_owned(),
            container,
            host,
            host_port: std::sync::atomic::AtomicU16::new(host_port),
            client: Client::new(),
        };

        node.wait_ready().await;
        node
    }

    /// Base URL reachable from the test host.
    pub fn url(&self) -> String {
        let port = self.host_port.load(std::sync::atomic::Ordering::Relaxed);
        format!("http://{}:{port}", self.host)
    }

    /// Poll `GET /live` until the node is ready.
    pub async fn wait_ready(&self) {
        let url = format!("{}/live", self.url());
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            match self
                .client
                .get(&url)
                .timeout(Duration::from_secs(2))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => return,
                Ok(resp) => {
                    eprintln!("[{}] /live returned {}", self.label, resp.status());
                }
                Err(e) => {
                    eprintln!("[{}] /live error: {e}", self.label);
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "[{}] node not ready at {url} after {READY_TIMEOUT:?}",
                self.label,
            );
            tokio::time::sleep(READY_POLL_INTERVAL).await;
        }
    }

    /// Stop the container (the node process exits, state is lost).
    pub async fn stop(&self) {
        self.container.stop().await.expect("stop container");
        eprintln!("[{}] stopped", self.label);
    }

    /// Restart the container and wait until the node is ready again.
    ///
    /// Since all state is in-memory, the restarted node has a fresh
    /// identity — a new invite/connect cycle is required.
    pub async fn start(&self) {
        self.container.start().await.expect("start container");

        // Docker Desktop may reassign the host port after stop/start.
        let new_port = self
            .container
            .get_host_port_ipv4(INTERNAL_PORT)
            .await
            .expect("get host port after restart");
        self.host_port
            .store(new_port, std::sync::atomic::Ordering::Relaxed);

        eprintln!(
            "[{}] restarted on port {new_port}, waiting for ready...",
            self.label
        );
        self.wait_ready().await;
    }

    /// Returns `true` if `GET /live` responds 200 within 2 seconds.
    pub async fn is_alive(&self) -> bool {
        self.client
            .get(format!("{}/live", self.url()))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }

    // ---- Public API helpers (/p2p/*) ----

    /// `GET /p2p/invite` — create an invite for this node.
    pub async fn get_invite(&self) -> Value {
        self.client
            .get(format!("{}/p2p/invite", self.url()))
            .send()
            .await
            .expect("get invite request")
            .json()
            .await
            .expect("parse invite json")
    }

    /// `POST /p2p/connect` — connect to a peer using their invite.
    pub async fn connect(&self, invite: &Value) {
        let body = serde_json::json!({ "invite": invite });
        let resp = self
            .client
            .post(format!("{}/p2p/connect", self.url()))
            .json(&body)
            .send()
            .await
            .expect("connect request");
        assert!(
            resp.status().is_success(),
            "[{}] connect failed: {}",
            self.label,
            resp.status(),
        );
    }

    /// `POST /p2p/connect` — returns response body ("connected" or "pending").
    pub async fn connect_status(&self, invite: &Value) -> String {
        let body = serde_json::json!({ "invite": invite });
        let resp = self
            .client
            .post(format!("{}/p2p/connect", self.url()))
            .json(&body)
            .send()
            .await
            .expect("connect request");
        assert!(
            resp.status().is_success(),
            "[{}] connect failed: {}",
            self.label,
            resp.status(),
        );
        resp.text().await.expect("read response body")
    }

    /// `GET /p2p/home-namespace` — returns the node's home namespace ID.
    pub async fn home_namespace(&self) -> String {
        let resp: Value = self
            .client
            .get(format!("{}/p2p/home-namespace", self.url()))
            .send()
            .await
            .expect("home-namespace request")
            .json()
            .await
            .expect("parse home-namespace json");
        resp["namespace"]
            .as_str()
            .expect("namespace field")
            .to_owned()
    }

    /// `POST /p2p/insert` — insert an entry into the given namespace.
    pub async fn insert(&self, namespace: &str, path: &str, body: &str) {
        let payload = serde_json::json!({
            "namespace": namespace,
            "path": path,
            "body": body,
        });
        let resp = self
            .client
            .post(format!("{}/p2p/insert", self.url()))
            .json(&payload)
            .send()
            .await
            .expect("insert request");
        assert!(
            resp.status().is_success(),
            "[{}] insert failed: {}",
            self.label,
            resp.status(),
        );
    }

    /// `POST /p2p/list` — list entries from a specific namespace.
    pub async fn list(&self, namespace: &str) -> Vec<Value> {
        self.client
            .post(format!("{}/p2p/list", self.url()))
            .json(&serde_json::json!({ "namespace": namespace }))
            .send()
            .await
            .expect("list request")
            .json()
            .await
            .expect("parse list json")
    }

    /// `GET /p2p/subspace-id` — returns the node's `SubspaceId` (hex).
    pub async fn subspace_id(&self) -> String {
        let resp: Value = self
            .client
            .get(format!("{}/p2p/subspace-id", self.url()))
            .send()
            .await
            .expect("subspace-id request")
            .json()
            .await
            .expect("parse subspace-id json");
        resp["subspace_id"]
            .as_str()
            .expect("subspace_id field")
            .to_owned()
    }

    /// `POST /p2p/ticket` — generate a `SyncTicket` for a subspace.
    pub async fn create_ticket(&self, to_subspace: &str) -> Value {
        self.client
            .post(format!("{}/p2p/ticket", self.url()))
            .json(&serde_json::json!({ "to_subspace": to_subspace }))
            .send()
            .await
            .expect("create ticket request")
            .json()
            .await
            .expect("parse ticket json")
    }

    // ---- Debug helpers (/debug/*) — requires MEE_DEBUG=1 ----

    /// `POST /debug/import` — import a `SyncTicket` (no direct connection).
    pub async fn import_ticket(&self, ticket: &Value) {
        let body = serde_json::json!({ "ticket": ticket });
        let resp = self
            .client
            .post(format!("{}/debug/import", self.url()))
            .json(&body)
            .send()
            .await
            .expect("import request");
        assert!(
            resp.status().is_success(),
            "[{}] import failed: {}",
            self.label,
            resp.status(),
        );
    }

    /// `GET /debug/gossip/peers` — returns cached peer advertisements.
    pub async fn gossip_peers(&self) -> Vec<Value> {
        self.client
            .get(format!("{}/debug/gossip/peers", self.url()))
            .send()
            .await
            .expect("gossip peers request")
            .json()
            .await
            .expect("parse gossip peers json")
    }
}

// ---- Docker network helpers ----

/// Create a Docker bridge network. Idempotent (ignores "already exists").
pub async fn create_network(name: &str) {
    let output = tokio::process::Command::new("docker")
        .args(["network", "create", name])
        .output()
        .await
        .expect("docker network create");
    // Ignore "already exists" errors
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("already exists"),
            "docker network create failed: {stderr}",
        );
    }
}

/// Remove a Docker network. Ignores errors (best-effort cleanup).
pub async fn remove_network(name: &str) {
    let _ = tokio::process::Command::new("docker")
        .args(["network", "rm", name])
        .output()
        .await;
}

// ---- Polling helpers ----

/// Poll `node.list()` until an entry with the given `key` appears.
pub async fn wait_for_entry(
    node: &MeeNode,
    namespace: &str,
    expected_key: &str,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let entries = node.list(namespace).await;
        if entries
            .iter()
            .any(|e| e.get("key").and_then(Value::as_str) == Some(expected_key))
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "[{}] timed out waiting for entry '{expected_key}' after {timeout:?}",
            node.label,
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Poll gossip peer cache until at least `min_count` peers appear.
pub async fn wait_for_gossip_peers(node: &MeeNode, min_count: usize, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let peers = node.gossip_peers().await;
        if peers.len() >= min_count {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "[{}] timed out waiting for {} gossip peers after {timeout:?}",
            node.label,
            min_count,
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
