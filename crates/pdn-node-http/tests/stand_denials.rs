//! The surface's refusals, each next to the authorized act it is the
//! tightest denial of (`code-practices/access-control-tests.md`). An
//! unhosted identity is 409 rather than 404 on purpose: route names are
//! unpinned, so a test asserting absence would keep passing after a rename.

use anyhow::Result;
use axum::{body::Bytes, http::StatusCode};
use pdn_node::{PdnId, RecordId, RecordKind, RecordRef};
use pdn_node_http::shapes::{Act, HeldPod, HeldPods, HostedIdentities, Member};

mod common;
use common::{pod_route, record_route, Stand};

/// An identity no runtime in this test creates or links.
const UNHOSTED: PdnId = PdnId::from_bytes([0x77; 32]);

/// Each refusal of the surface arrives with a status of its own, beside the
/// accepted act it denies: an unhosted identity (409), a replayed pod
/// invite (403), a malformed identifier, an empty payload, and a mistyped
/// query parameter (400).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // the denials of one surface, kept beside their positives
async fn refusals_arrive_as_refusals() -> Result<()> {
    let stand = Stand::new();
    let inviter = stand.spawn("inviter").await?;
    let joiner = stand.spawn("joiner").await?;
    let replayer = stand.spawn("replayer").await?;

    let alice = inviter.create_identity().await?;
    let bob = joiner.create_identity().await?;
    let carol = replayer.create_identity().await?;
    let family = inviter.create_pod(alice).await?;

    // Denied (an unhosted identity). Beside it, the same route on a hosted
    // identity answers.
    let unhosted = inviter
        .get(&format!("/debug/identities/{UNHOSTED}/pods"))
        .await?;
    assert_eq!(
        unhosted.status,
        StatusCode::CONFLICT,
        "an unhosted identity must be refused, got {}: {}",
        unhosted.status,
        unhosted.text()
    );
    assert!(
        unhosted.text().contains(&UNHOSTED.to_string()),
        "the refusal must name what was addressed: {}",
        unhosted.text()
    );
    inviter
        .get(&format!("/debug/identities/{alice}/pods"))
        .await?
        .ok()?;

    // Denied (a replayed invite payload): the secret is burnt, and the pod
    // records no second joined event. Beside it, the first presentation
    // joins.
    let invite = inviter.invite_to_pod(alice, family).await?;
    let first: HeldPod = joiner.join_pod(bob, invite.clone()).await?.json()?;
    assert_eq!(first.pod, family);
    let replay = replayer.join_pod(carol, invite).await?;
    assert_eq!(
        replay.status,
        StatusCode::FORBIDDEN,
        "a replayed invite payload must be refused, got {}: {}",
        replay.status,
        replay.text()
    );
    let mut members = inviter.pod_members(alice, family).await?;
    members.sort_by_key(|member| member.id);
    let mut expected = vec![
        Member {
            id: alice,
            owner: true,
        },
        Member {
            id: bob,
            owner: false,
        },
    ];
    expected.sort_by_key(|member| member.id);
    assert_eq!(
        members, expected,
        "a refused join must leave no member behind"
    );
    let replayer_pods: HeldPods = replayer
        .get(&format!("/debug/identities/{carol}/pods"))
        .await?
        .json()?;
    assert_eq!(replayer_pods.pods, []);

    // Denied (a malformed identifier): the request itself is wrong, which is
    // neither a refusal nor an absence.
    let malformed = inviter.get("/debug/identities/not-hex/pods").await?;
    assert_eq!(
        malformed.status,
        StatusCode::BAD_REQUEST,
        "a malformed identifier is 400, got {}: {}",
        malformed.status,
        malformed.text()
    );

    // Denied (an empty payload): the engine keeps no zero-length entry, and
    // that is the request being wrong rather than the host not knowing what
    // happened. Beside it, one byte is accepted.
    let empty = inviter
        .post(
            &format!("{}/records?kind=claim", pod_route(alice, family)),
            Bytes::new(),
        )
        .await?;
    assert_eq!(
        empty.status,
        StatusCode::BAD_REQUEST,
        "an empty payload is 400, got {}: {}",
        empty.status,
        empty.text()
    );
    inviter
        .place_record(alice, family, RecordKind::Claim, b".")
        .await?;

    // Denied (a mistyped query parameter): refused rather than ignored, so a
    // lifetime nobody asked for cannot be minted quietly. Beside it, the
    // spelled parameter is accepted.
    let invite_route = format!("{}/invites", pod_route(alice, family));
    let mistyped = inviter
        .post(&format!("{invite_route}?lifetime_sec=60"), Bytes::new())
        .await?;
    assert_eq!(
        mistyped.status,
        StatusCode::BAD_REQUEST,
        "an unknown query parameter is 400, got {}: {}",
        mistyped.status,
        mistyped.text()
    );
    inviter
        .post(&format!("{invite_route}?lifetime_secs=60"), Bytes::new())
        .await?
        .ok()?;

    Ok(())
}

/// Denied (linking into an already-hosted identity): `link`'s pre-dial
/// guard refuses before ever dialing, so a linking-invite payload for an
/// identity already hosted right here on the target is refused without
/// needing a reachable address.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn linking_into_an_already_hosted_identity_is_refused_as_a_conflict() -> Result<()> {
    let stand = Stand::new();
    let host = stand.spawn("host").await?;
    let alice = host.create_identity().await?;

    let self_invite = host
        .post(
            &format!("/debug/identities/{alice}/linking-invite"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    let already_hosted = host.post("/debug/link", self_invite).await?;
    assert_eq!(
        already_hosted.status,
        StatusCode::CONFLICT,
        "linking into an already-hosted identity must be refused, got {}: {}",
        already_hosted.status,
        already_hosted.text()
    );

    Ok(())
}

/// Denied (a concurrent link toward the same not-yet-hosted identity): two
/// `link` attempts racing toward two independent invites of the same
/// identity let exactly one commit — the loser refuses as a conflict
/// (another link already in flight, or, if the winner finished first,
/// already hosted) instead of both racing to register the identity twice.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn concurrent_links_toward_the_same_identity_let_only_one_commit() -> Result<()> {
    let stand = Stand::new();
    let inviter = stand.spawn("inviter").await?;
    let linker = stand.spawn("linker").await?;
    let alice = inviter.create_identity().await?;

    let first_invite = inviter
        .post(
            &format!("/debug/identities/{alice}/linking-invite"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    let second_invite = inviter
        .post(
            &format!("/debug/identities/{alice}/linking-invite"),
            Bytes::new(),
        )
        .await?
        .ok()?;

    let (first, second) = tokio::join!(
        linker.post("/debug/link", first_invite),
        linker.post("/debug/link", second_invite)
    );
    let statuses = [first?.status, second?.status];
    assert!(
        statuses.contains(&StatusCode::NO_CONTENT) && statuses.contains(&StatusCode::CONFLICT),
        "exactly one of two concurrent links toward the same identity must succeed, got {statuses:?}"
    );
    let hosted: HostedIdentities = linker.get("/debug/identities").await?.json()?;
    assert_eq!(
        hosted
            .identities
            .iter()
            .filter(|identity| **identity == alice)
            .count(),
        1,
        "the concurrent loser must not leave duplicate or missing hosted state: {hosted:?}"
    );

    Ok(())
}

/// Each refusal of the pods routes arrives with a status of its own, beside
/// the accepted act it denies, on one node hosting a pod's owner and a
/// second identity: that identity while no member (409), its removal of the
/// owner as a plain member (403), and a read of a record the pod does not
/// hold (404). The non-member shares the owner's node, so the node holds the
/// pod and the refusal rests on the identity named alone.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // the denials of one surface, kept beside their positives
async fn pod_refusals_arrive_as_refusals() -> Result<()> {
    let stand = Stand::new();
    let tablet = stand.spawn("tablet").await?;
    let leisure = tablet.create_identity().await?;
    let work = tablet.create_identity().await?;
    let family = tablet.create_pod(leisure).await?;

    // Denied (a co-located identity that is no member). Beside it, the same
    // route answers for the owner.
    let outsider = tablet
        .get(&format!("/debug/identities/{work}/pods/{family}/members"))
        .await?;
    assert_eq!(
        outsider.status,
        StatusCode::CONFLICT,
        "a pod the identity is no member of must be refused, got {}: {}",
        outsider.status,
        outsider.text()
    );
    assert!(
        outsider.text().contains(&family.to_string()),
        "the refusal must name the pod: {}",
        outsider.text()
    );
    assert_eq!(
        tablet.pod_members(leisure, family).await?,
        vec![Member {
            id: leisure,
            owner: true,
        }]
    );

    let invite = tablet.invite_to_pod(leisure, family).await?;
    let joined: HeldPod = tablet.join_pod(work, invite).await?.json()?;
    assert_eq!(joined.pod, family);

    // Denied (a refusal by role): a plain member removes another member.
    // Beside it, the owner's removal of the plain member goes through, once
    // the record reads below are done.
    let by_member = tablet.pod_act(work, family, Act::Remove(leisure)).await?;
    assert_eq!(
        by_member.status,
        StatusCode::FORBIDDEN,
        "a plain member's removal must be refused, got {}: {}",
        by_member.status,
        by_member.text()
    );
    let after_refusal = tablet.pod_members(leisure, family).await?;
    assert!(
        after_refusal.contains(&Member {
            id: leisure,
            owner: true,
        }) && after_refusal.contains(&Member {
            id: work,
            owner: false,
        }),
        "the refused removal must leave both members: {after_refusal:?}"
    );

    // Denied (an absent record): 404, the status reserved for nothing being
    // there. Beside it, a placed claim reads back.
    let placed = tablet
        .place_record(leisure, family, RecordKind::Claim, b"alice@example.org")
        .await?;
    let read = tablet
        .get(&record_route(leisure, family, placed))
        .await?
        .ok()?;
    assert_eq!(read, Bytes::from_static(b"alice@example.org"));
    let absent = RecordRef {
        id: RecordId::from_bytes([0x77; 16]),
        ..placed
    };
    let absent = tablet.get(&record_route(leisure, family, absent)).await?;
    assert_eq!(
        absent.status,
        StatusCode::NOT_FOUND,
        "an absent record is 404, got {}: {}",
        absent.status,
        absent.text()
    );

    tablet
        .pod_act(leisure, family, Act::Remove(work))
        .await?
        .ok()?;
    assert_eq!(
        tablet.pod_members(leisure, family).await?,
        vec![Member {
            id: leisure,
            owner: true,
        }],
        "the owner's removal must take effect"
    );

    Ok(())
}
