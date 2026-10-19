//! The test build's surface, driven over HTTP on loopback as the sync tests'
//! coordinator drives it through the cable's tunnel. Every node here comes up
//! through the surface's own reset, on direct paths.
#![cfg(feature = "observation")]

use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, Result};
use pdn_mobile::{PdnError, PdnNode};
use serde_json::Value;
use tempfile::TempDir;
use test_utils::eventually;

/// A pod interval no test waits through: what reaches a node here travels on
/// the sessions a write or a join prompts.
const POD_INTERVAL_SECS: u64 = 300;

/// One handle serving its surface on a loopback port of its own.
struct Phone {
    node: Arc<PdnNode>,
    base: String,
    http: reqwest::Client,
    /// Held so the directory outlives the nodes the resets bring up on it.
    _dir: TempDir,
}

impl Phone {
    async fn serve() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let node = PdnNode::new()?;
        let port = node
            .serve_test_surface(dir.path().to_string_lossy().into_owned(), 0)
            .await?;
        Ok(Self {
            node,
            base: format!("http://127.0.0.1:{port}"),
            http: reqwest::Client::new(),
            _dir: dir,
        })
    }

    /// A reset on direct paths, answering the new identity.
    async fn reset(&self) -> Result<String> {
        let (status, body) = self
            .post(
                "/debug/agent/reset",
                &format!(r#"{{"pod_interval_secs":{POD_INTERVAL_SECS},"connectivity":"direct"}}"#),
            )
            .await?;
        anyhow::ensure!(status == 200, "reset answered {status}: {body}");
        field(&body, "identity")
    }

    async fn post(&self, path: &str, body: &str) -> Result<(u16, String)> {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/json")
            .body(body.to_owned())
            .send()
            .await?;
        Ok((response.status().as_u16(), response.text().await?))
    }

    async fn get(&self, path: &str) -> Result<(u16, String)> {
        let response = self.http.get(format!("{}{path}", self.base)).send().await?;
        Ok((response.status().as_u16(), response.text().await?))
    }

    async fn create_pod(&self, identity: &str) -> Result<String> {
        let (status, body) = self
            .post(&format!("/debug/identities/{identity}/pods"), "")
            .await?;
        anyhow::ensure!(status == 200, "create answered {status}: {body}");
        field(&body, "pod")
    }

    async fn invite(&self, identity: &str, pod: &str) -> Result<String> {
        let (status, body) = self
            .post(
                &format!("/debug/identities/{identity}/pods/{pod}/invites"),
                "",
            )
            .await?;
        anyhow::ensure!(status == 200, "invite answered {status}: {body}");
        Ok(body)
    }

    async fn place(&self, identity: &str, pod: &str, payload: &str) -> Result<String> {
        let response = self
            .http
            .post(format!(
                "{}/debug/identities/{identity}/pods/{pod}/records?kind=immutable-document",
                self.base
            ))
            .body(payload.to_owned())
            .send()
            .await?;
        let status = response.status().as_u16();
        let body = response.text().await?;
        anyhow::ensure!(status == 200, "placement answered {status}: {body}");
        field(&body, "id")
    }

    /// The records a stream yields until `done` says so, read line by line.
    async fn read_stream(
        response: reqwest::Response,
        done: impl Fn(&[Value]) -> bool,
    ) -> Result<Vec<Value>> {
        let mut response = response;
        let mut records = Vec::new();
        let mut pending = String::new();
        let read = async {
            while let Some(chunk) = response.chunk().await? {
                pending.push_str(std::str::from_utf8(&chunk)?);
                while let Some(end) = pending.find('\n') {
                    let line: String = pending.drain(..=end).collect();
                    records.push(serde_json::from_str::<Value>(line.trim())?);
                    if done(&records) {
                        return anyhow::Ok(());
                    }
                }
            }
            Err(anyhow!("the stream ended first"))
        };
        tokio::time::timeout(test_utils::TIMEOUT, read)
            .await
            .map_err(|_| anyhow!("no such record within the wait"))??;
        Ok(records)
    }
}

fn field(body: &str, name: &str) -> Result<String> {
    serde_json::from_str::<Value>(body)?
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("no {name} in {body}"))
}

/// The entry of record `id` in a listing's body.
fn listed(body: &str, id: &str) -> Result<Value> {
    serde_json::from_str::<Vec<Value>>(body)?
        .into_iter()
        .find(|entry| entry["key"].as_str().is_some_and(|key| key.contains(id)))
        .ok_or_else(|| anyhow!("no record {id} in the listing: {body}"))
}

fn is_insert_of(record: &Value, id: &str) -> bool {
    record["kind"] == "insert_remote"
        && record["store"] == "records"
        && record["key"].as_str().is_some_and(|key| key.contains(id))
}

/// A record placed on one node reaches another node's event route and its
/// listing through the test surface, in the laptop host's record form, and a
/// listing asked without payloads holds it counted and present with none.
/// Paired denial: a third node, a member of no pod, is refused that pod's
/// stream and its listing. Fails with the surface's fallback answering no
/// node, or with the listing's `payloads` ignored.
#[tokio::test(flavor = "multi_thread")]
async fn the_surface_yields_the_runtimes_records() -> Result<()> {
    let (writer, reader, outsider) = (
        Phone::serve().await?,
        Phone::serve().await?,
        Phone::serve().await?,
    );
    let (w, r, o) = (
        writer.reset().await?,
        reader.reset().await?,
        outsider.reset().await?,
    );
    let pod = writer.create_pod(&w).await?;
    let invite = writer.invite(&w, &pod).await?;
    let (status, body) = reader
        .post(&format!("/debug/identities/{r}/pods/join"), &invite)
        .await?;
    anyhow::ensure!(status == 200, "join answered {status}: {body}");

    let stream = reader
        .http
        .get(format!(
            "{}/debug/identities/{r}/pods/{pod}/events",
            reader.base
        ))
        .send()
        .await?;
    anyhow::ensure!(
        stream.status() == 200,
        "events answered {}",
        stream.status()
    );
    let id = writer.place(&w, &pod, "a record").await?;
    let records = Phone::read_stream(stream, |records| {
        records.iter().any(|record| is_insert_of(record, &id))
    })
    .await?;
    let insert = records
        .iter()
        .find(|record| is_insert_of(record, &id))
        .ok_or_else(|| anyhow!("no insert"))?;
    assert!(
        insert["seq"].is_u64() && insert["t"].is_u64(),
        "the record is the runtime's form: {insert}"
    );

    let counted = eventually(|| async {
        let (_, listing) = reader
            .get(&format!("/debug/identities/{r}/pods/{pod}/listing"))
            .await?;
        Ok(listed(&listing, &id)
            .is_ok_and(|entry| entry["verdict"] == "counted" && entry["present"] == true))
    })
    .await?;
    assert!(
        counted,
        "the reader's listing holds the record counted with its payload"
    );
    let (status, bare) = reader
        .get(&format!(
            "/debug/identities/{r}/pods/{pod}/listing?payloads=false"
        ))
        .await?;
    let entry = listed(&bare, &id)?;
    assert!(
        status == 200
            && entry["verdict"] == "counted"
            && entry["present"] == true
            && entry.get("payload").is_none(),
        "the listing without payloads: {entry}"
    );

    // Denied: a node whose identity is a member of no pod.
    let (status, _) = outsider
        .get(&format!("/debug/identities/{o}/pods/{pod}/events"))
        .await?;
    assert_ne!(status, 200, "a non-member opened the pod's stream");
    let (status, _) = outsider
        .get(&format!("/debug/identities/{o}/pods/{pod}/listing"))
        .await?;
    assert_ne!(status, 200, "a non-member read the pod's listing");
    Ok(())
}

/// A node joining through the observed join route receives, from `seq` 0, the
/// record the pod held before the record-store session that carried it ends.
/// Paired denial: an observed join on the consumed invite is refused and
/// opens no stream. Fails with the laptop host's router built without
/// its observation routes.
#[tokio::test(flavor = "multi_thread")]
async fn an_observed_join_through_the_surface_yields_the_first_session() -> Result<()> {
    let (writer, joiner, late) = (
        Phone::serve().await?,
        Phone::serve().await?,
        Phone::serve().await?,
    );
    let (w, j, l) = (
        writer.reset().await?,
        joiner.reset().await?,
        late.reset().await?,
    );
    let pod = writer.create_pod(&w).await?;
    let id = writer.place(&w, &pod, "held before the join").await?;
    let invite = writer.invite(&w, &pod).await?;

    let response = joiner
        .http
        .post(format!(
            "{}/debug/identities/{j}/pods/join-observed",
            joiner.base
        ))
        .body(invite.clone())
        .send()
        .await?;
    anyhow::ensure!(
        response.status() == 200,
        "observed join answered {}",
        response.status()
    );
    let pod_header = response
        .headers()
        .get(pdn_node_http::POD_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    assert_eq!(
        pod_header.as_deref(),
        Some(pod.as_str()),
        "the response names the pod joined"
    );
    let records = Phone::read_stream(response, |records| {
        records.iter().any(|record| {
            record["kind"] == "sync_finished"
                && record["store"] == "records"
                && record["received"].as_u64().unwrap_or(0) > 0
        })
    })
    .await?;
    assert_eq!(
        records.first().and_then(|record| record["seq"].as_u64()),
        Some(0),
        "the stream starts at seq 0"
    );
    let insert = records.iter().position(|record| is_insert_of(record, &id));
    let finished = records.iter().position(|record| {
        record["kind"] == "sync_finished"
            && record["store"] == "records"
            && record["received"].as_u64().unwrap_or(0) > 0
    });
    assert!(
        insert.is_some() && insert < finished,
        "the record's insert precedes its session's end: {records:?}"
    );

    // Denied: the invite is consumed.
    let (status, _) = late
        .post(
            &format!("/debug/identities/{l}/pods/join-observed"),
            &invite,
        )
        .await?;
    assert_ne!(
        status, 200,
        "an observed join on a consumed invite was answered"
    );
    let (_, pods) = late.get(&format!("/debug/identities/{l}/pods")).await?;
    assert!(
        !pods.contains(&pod),
        "the refused joiner holds the pod: {pods}"
    );
    Ok(())
}

/// A reset after the node created a pod brings up a node with a new node id and
/// an identity that holds no pod; before any reset no node is up. Paired denial: a reset naming
/// a pod interval of 0 is refused and brings no node up. Fails with the
/// reset's check of the interval removed.
#[tokio::test(flavor = "multi_thread")]
async fn a_reset_starts_from_an_empty_directory() -> Result<()> {
    let phone = Phone::serve().await?;
    let (status, _) = phone.get("/debug/status").await?;
    assert_eq!(status, 503, "a node answered before any reset");

    // Denied: a pod interval of 0.
    let (status, _) = phone
        .post(
            "/debug/agent/reset",
            r#"{"pod_interval_secs":0,"connectivity":"direct"}"#,
        )
        .await?;
    assert_eq!(status, 400, "a reset at pod interval 0 was taken");
    let (status, _) = phone.get("/debug/status").await?;
    assert_eq!(status, 503, "a refused reset brought a node up");

    let first = phone.reset().await?;
    let (_, before) = phone.get("/debug/status").await?;
    let pod = phone.create_pod(&first).await?;
    let second = phone.reset().await?;
    let (_, after) = phone.get("/debug/status").await?;
    assert_ne!(first, second, "the reset kept the identity");
    assert_ne!(
        before.lines().next(),
        after.lines().next(),
        "the reset kept the node id"
    );
    let (_, pods) = phone
        .get(&format!("/debug/identities/{second}/pods"))
        .await?;
    assert!(
        !pods.contains(&pod),
        "the new node holds the old pod: {pods}"
    );
    assert!(
        after.contains("observation on"),
        "the status names the observation build: {after}"
    );
    Ok(())
}

/// The surface's port answers at loopback and is refused at the machine's
/// network address. The paired denial is the network address itself. Fails
/// with the surface bound to every interface.
#[tokio::test(flavor = "multi_thread")]
async fn the_surface_answers_on_loopback_alone() -> Result<()> {
    let phone = Phone::serve().await?;
    let port: u16 = phone
        .base
        .rsplit(':')
        .next()
        .ok_or_else(|| anyhow!("no port"))?
        .parse()?;
    let (status, _) = phone.get("/debug/agent/clock").await?;
    assert_eq!(status, 200, "loopback was not answered");

    // A datagram socket "connected" to a documentation address names the
    // interface the machine would route through, and sends nothing.
    let probe = std::net::UdpSocket::bind("0.0.0.0:0")?;
    probe.connect("192.0.2.1:9")?;
    let network = probe.local_addr()?.ip();
    anyhow::ensure!(
        !network.is_loopback() && !network.is_unspecified(),
        "this machine has no network address to try"
    );
    // Denied: the same port at the network address.
    let refused = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect((network, port)),
    )
    .await;
    assert!(
        !matches!(refused, Ok(Ok(_))),
        "the surface answered at {network}"
    );
    Ok(())
}

/// Device samples handed to the surface are answered in arrival order after
/// the sequence number asked for. Paired denial: a sample that is not JSON is
/// refused as malformed and not kept. Fails with the samples answered from
/// the first whatever the sequence number asked for.
#[tokio::test(flavor = "multi_thread")]
async fn device_samples_are_answered_in_order() -> Result<()> {
    let phone = Phone::serve().await?;
    for second in 0..3 {
        phone
            .node
            .report_device_sample(&format!(r#"{{"kind":"cpu","t":{second}}}"#))?;
    }
    // Denied: not JSON.
    let refused = phone.node.report_device_sample("not json");
    assert!(
        matches!(refused, Err(PdnError::MalformedInput { .. })),
        "{refused:?}"
    );

    let (status, body) = phone.get("/debug/agent/samples?after=0").await?;
    assert_eq!(status, 200);
    let lines: Vec<Value> = body
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let seqs: Vec<u64> = lines
        .iter()
        .filter_map(|line| line["seq"].as_u64())
        .collect();
    assert_eq!(
        seqs,
        [1, 2],
        "the samples after 0, in order, and nothing refused"
    );
    assert_eq!(
        lines[1]["sample"]["t"], 2,
        "a sample is answered as it was handed over"
    );

    // A read after 0 released sample 0; a sample written over several lines
    // is answered on one.
    phone
        .node
        .report_device_sample("{\n  \"kind\": \"state\",\n  \"t\": 3\n}")?;
    let (_, body) = phone.get("/debug/agent/samples").await?;
    let seqs: Vec<u64> = body
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .filter_map(|line| line["seq"].as_u64())
        .collect();
    assert_eq!(
        seqs,
        [1, 2, 3],
        "the samples a read has passed are released"
    );
    Ok(())
}

/// A reset whose client hangs up before its answer still brings its node up,
/// and the reset after it is taken. Fails with the reset run inside the
/// request's own future, which the hang-up drops between the stop and the
/// bring-up.
#[tokio::test(flavor = "multi_thread")]
async fn a_reset_outlives_its_client() -> Result<()> {
    let phone = Phone::serve().await?;
    let first = phone.reset().await?;
    // Shorter than stopping the first node and bringing up the next.
    let hasty = reqwest::Client::builder()
        .timeout(Duration::from_millis(20))
        .build()?;
    let gone = hasty
        .post(format!("{}/debug/agent/reset", phone.base))
        .header("content-type", "application/json")
        .body(format!(
            r#"{{"pod_interval_secs":{POD_INTERVAL_SECS},"connectivity":"direct"}}"#
        ))
        .send()
        .await;
    assert!(gone.is_err(), "the reset answered before the hang-up");

    let up = eventually(|| async { Ok(phone.get("/debug/status").await?.0 == 200) }).await?;
    assert!(up, "the abandoned reset left no node up");
    let next = phone.reset().await?;
    assert_ne!(
        first, next,
        "the reset after it brought up the old identity"
    );
    Ok(())
}
