//! An HTTP client over one host's router — the scenarios' only way to reach
//! a runtime.
//!
//! The router is driven in process, so no listener and no port are
//! involved; everything above the socket is the path a container's request
//! takes. Nothing here calls a runtime service directly: a scenario that
//! did would prove the service works and leave the surface it claims to
//! test unexercised.
// Each test binary includes this module and uses its own subset of the
// helpers; what one binary leaves unused is not dead code of the crate.
#![allow(dead_code)]

use std::{cell::RefCell, future::Future, sync::Arc, time::Duration};

use anyhow::{ensure, Context as _, Result};
use axum::{
    body::{Body, Bytes},
    http::{Request, StatusCode},
    Router,
};
use pdn_node::{PdnId, Runtime, SpawnOptions};
use pdn_node_http::{
    router,
    shapes::{CreatedIdentity, GrantPublication},
};
use serde::de::DeserializeOwned;
use tower::ServiceExt as _;

/// The reconcile cadence these scenarios inject. Convergence is waited for
/// by repeating a read — the only means the surface offers — and a
/// sub-second cadence keeps that wait short.
pub const RECONCILE: Duration = Duration::from_millis(500);

/// Ceiling on one poll. Generous, since a poll returns the moment its
/// condition holds; finite, so a real non-convergence fails with a named
/// assertion in tens of seconds instead of hanging.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// One answer from the surface: what a container-level assertion sees.
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

/// One host: a runtime with the debug surface mounted over it.
pub struct Host {
    runtime: Arc<Runtime>,
    app: Router,
}

impl Host {
    /// Spawn a host with the debug surface on — the flag's other side is
    /// asserted by the gate test.
    pub async fn spawn() -> Result<Self> {
        let runtime = Arc::new(
            Runtime::spawn_with(SpawnOptions {
                reconcile_interval: RECONCILE,
            })
            .await?,
        );
        let app = router(Arc::clone(&runtime), true);
        Ok(Self { runtime, app })
    }

    pub async fn get(&self, path: &str) -> Result<Answer> {
        self.send(Request::get(path).body(Body::empty())?).await
    }

    pub async fn post(&self, path: &str, body: impl Into<Bytes>) -> Result<Answer> {
        self.send(Request::post(path).body(Body::from(body.into()))?)
            .await
    }

    pub async fn put(&self, path: &str, body: impl Into<Bytes>) -> Result<Answer> {
        self.send(Request::put(path).body(Body::from(body.into()))?)
            .await
    }

    pub async fn delete(&self, path: &str) -> Result<Answer> {
        self.send(Request::delete(path).body(Body::empty())?).await
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

    pub async fn shutdown(&self) -> Result<()> {
        self.runtime.shutdown().await
    }

    async fn send(&self, request: Request<Body>) -> Result<Answer> {
        let response = self.app.clone().oneshot(request).await?;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        Ok(Answer { status, body })
    }
}

impl Drop for Host {
    /// A best-effort net for the panic or early-`?`-return path, not a
    /// guarantee: `Drop` is synchronous and shutdown is not, so this spawns
    /// a detached task that may never get polled if the test's own runtime
    /// tears down first — which is exactly what happens when the process
    /// itself is exiting. It works in practice under `cargo-nextest`
    /// because each test runs in its own process and the OS reclaims the
    /// endpoint and every task a hosted identity spawned at exit either
    /// way; a shared-process test runner (bare `cargo test`, several tests
    /// per binary) would leak for real. Harmless to run twice regardless,
    /// since `Runtime::shutdown` needs no exclusive ownership and is
    /// idempotent — prefer the explicit `shutdown().await` on every test's
    /// own exit path over relying on this.
    fn drop(&mut self) {
        let runtime = Arc::clone(&self.runtime);
        tokio::spawn(async move {
            let _ = runtime.shutdown().await;
        });
    }
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

/// Poll `check` every 100ms until it holds or [`TIMEOUT`] elapses; the
/// return says whether it was observed in time. Repeating the read is the
/// only wait the surface offers, by design — nothing here forces a
/// reconciliation.
pub async fn eventually<F, Fut>(mut check: F) -> Result<bool>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    let deadline = std::time::Instant::now() + TIMEOUT;
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
    let last = RefCell::new(None::<(StatusCode, String)>);
    let observed = eventually(|| async {
        let answer = host.get(&format!("/debug/data/{issuer}/{path}")).await?;
        let held = holds(&answer);
        *last.borrow_mut() = Some((answer.status, answer.text()));
        Ok(held)
    })
    .await?;
    match (observed, last.into_inner()) {
        (true, _) => Ok(()),
        (false, Some((status, body))) => Err(anyhow::anyhow!("last answer was {status}: {body}")),
        (false, None) => Err(anyhow::anyhow!("the read was never attempted")),
    }
}
