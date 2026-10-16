//! What one scoped session of a grantee delivers, with the reconcile pass
//! an hour away and every read unnudged, so no other session stands in.

use std::time::Duration;

use anyhow::{ensure, Context, Result};
use pdn_node::{
    ConnectionsService as _, DataService as _, GrantedClaim, IdentityService as _, Runtime,
    SpawnOptions,
};
use pdn_types::{EntryPath, NonEmpty, PdnId};
use test_utils::eventually;

mod common;
use common::establish_patiently;

const RECONCILE: Duration = Duration::from_secs(3600);

async fn spawn_runtime() -> Result<Runtime> {
    Runtime::spawn(SpawnOptions {
        reconcile_interval: RECONCILE,
        ..SpawnOptions::memory()
    })
    .await
}

/// `common::granted_patiently` with the namespace polled unnudged: a nudged
/// read leaves a scoped session in flight that races the writes below.
async fn granted_unnudged(
    gives: &Runtime,
    gives_id: PdnId,
    receives: &Runtime,
    receives_id: PdnId,
    claims: NonEmpty<GrantedClaim>,
) -> Result<()> {
    gives
        .connections()
        .publish_grant(gives_id, receives_id, gives_id, claims)
        .await?;
    let probe = EntryPath::new("probe")?;
    let held = eventually(|| async {
        Ok(receives
            .data()
            .read_unnudged(receives_id, gives_id, &probe)
            .await
            .is_ok())
    })
    .await?;
    ensure!(
        held,
        "the receiver never came to hold the issuer's namespace"
    );
    Ok(())
}

/// One scoped session delivers every covered entry the issuer wrote before
/// it, and the uncovered one written beside them stays absent — the paired
/// denial: the grantee holds the replica and a grant, but no claim on that
/// path. The pre-grant entry marks the import's own session: once it reads
/// on the grantee, the issuer's side of that session has frozen its view
/// (`connect_and_sync` in `pdn-store`), so the later writes can travel only
/// in a later session.
#[tokio::test(flavor = "multi_thread")]
async fn one_scoped_session_delivers_every_covered_entry_written_before_it() -> Result<()> {
    let rt_w = spawn_runtime().await?;
    let rt_r = spawn_runtime().await?;
    let x = rt_w.identity().create().await?;
    let y = rt_r.identity().create().await?;
    let invite = rt_w.connections().invite(x, None).await?;
    establish_patiently(&rt_r, y, &rt_w, x, invite).await?;

    let marker = EntryPath::new("c/pre")?;
    let covered = ["c/0", "c/1", "c/2"]
        .map(EntryPath::new)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    let uncovered = EntryPath::new("u/0")?;

    rt_w.data().write(x, x, &marker, b"pre").await?;
    let claims = NonEmpty::from_vec(
        std::iter::once(&marker)
            .chain(&covered)
            .map(|path| GrantedClaim {
                claim: pdn_node::claim_id_of(&x, path),
                write: false,
            })
            .collect(),
    )
    .context("the claim set is not empty")?;
    granted_unnudged(&rt_w, x, &rt_r, y, claims).await?;
    rt_r.connections().sweep_pair_now(y, x).await?;
    assert!(
        eventually(|| async {
            Ok(rt_r.data().read_unnudged(y, x, &marker).await?.as_deref() == Some(&b"pre"[..]))
        })
        .await?,
        "the import's own session did not deliver the pre-grant entry"
    );

    for path in &covered {
        rt_w.data()
            .write(x, x, path, path.as_str().as_bytes())
            .await?;
    }
    rt_w.data().write(x, x, &uncovered, b"uncovered").await?;

    let received = rt_r.data().sync_scoped_once(y, x).await?;
    assert_eq!(
        received,
        covered.len(),
        "the one session did not carry exactly the covered writes"
    );
    // Payloads download after the session; polling unnudged starts none.
    for path in &covered {
        assert!(
            eventually(|| async {
                Ok(rt_r.data().read_unnudged(y, x, path).await?.as_deref()
                    == Some(path.as_str().as_bytes()))
            })
            .await?,
            "{path} did not arrive in the one session"
        );
    }
    // Denied (claim not granted), after the session that proved delivery.
    assert!(
        rt_r.data().read_unnudged(y, x, &uncovered).await?.is_none(),
        "an entry outside the grant reached the grantee"
    );

    rt_w.shutdown().await?;
    rt_r.shutdown().await?;
    Ok(())
}
