//! What a modified member device forges into a pod's stores: held on every
//! member device, read by nothing there, and leaving the member devices'
//! replicas equal. The forgeries arrive by store-level writes, the tickets
//! by hand.

use std::{collections::BTreeSet, time::Duration};

use anyhow::{Context as _, Result};
use data_layer::{
    identity_of, AuthorId, Contact, EventKind, MemberState, MembershipKey, OpId, PodStore,
    PodVerdicts, RecordKey, Seq, SpawnOptions, SyncNode, Verdict,
};
use pdn_types::{PdnId, PodId, RecordId};
use test_utils::{pod as c, TIMEOUT};
use tokio::sync::mpsc::UnboundedReceiver;

/// Out of every scenario's reach: no pass opens a session a scenario did
/// not name.
const QUIET: Duration = Duration::from_secs(3600);

const OWNER: MemberState = MemberState {
    member: true,
    owner: true,
};
const PLAIN: MemberState = MemberState {
    member: true,
    owner: false,
};

async fn quiet_node() -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval: QUIET,
        pod_reconcile_interval: QUIET,
        ..SpawnOptions::memory()
    })
    .await
}

/// A dial of one of `pod`'s stores from `holder`'s replica on `from` to
/// `callee`'s on `to`, as a drawn contact is dialed.
async fn dial(
    from: &SyncNode,
    holder: PdnId,
    pod: PodId,
    store: PodStore,
    to: &SyncNode,
    callee: PdnId,
) -> Result<()> {
    let contact = Contact::new(to.dial_handle().addr(), identity_of(callee));
    from.sync_pod_with_for_test(holder, pod, store, contact)
        .await
}

/// Every entry `holder`'s replica on `node` holds in either store, by key
/// and author, beside what it counts for: the membership store's from the
/// fold a read reports on `reports`.
async fn held(
    node: &SyncNode,
    reports: &mut UnboundedReceiver<PodVerdicts>,
    holder: PdnId,
    pod: PodId,
) -> Result<Vec<(PodStore, Vec<u8>, AuthorId, Verdict)>> {
    node.pod_membership(holder, pod).await?;
    let mut membership = None;
    while let Ok(report) = reports.try_recv() {
        if report.identity == holder && report.pod == pod {
            membership = Some(report.verdicts);
        }
    }
    let mut held: Vec<_> = membership
        .context("the read reported no fold")?
        .into_iter()
        .map(|(key, author, verdict)| (PodStore::Membership, key, author, verdict))
        .collect();
    held.extend(
        node.pod_record_view(holder, pod)
            .await?
            .verdicts()
            .map(|(entry, verdict)| (PodStore::Records, entry.key.clone(), entry.author, verdict)),
    );
    Ok(held)
}

/// One entry a device forges: who writes it, as which author, into which
/// store, at which key.
struct Forgery<'a> {
    phone: &'a SyncNode,
    writer: PdnId,
    author: AuthorId,
    store: PodStore,
    key: Vec<u8>,
    payload: Vec<u8>,
}

/// Whether `holder`'s replica on `node` comes to hold every one of
/// `forged`, counting none of them.
async fn holds_every_forgery_unread(
    node: &SyncNode,
    reports: &mut UnboundedReceiver<PodVerdicts>,
    holder: PdnId,
    pod: PodId,
    forged: &[Forgery<'_>],
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let held = held(node, reports, holder, pod).await?;
        let all = forged.iter().all(|forgery| {
            held.iter().any(|(store, key, author, verdict)| {
                *store == forgery.store
                    && *key == forgery.key
                    && *author == forgery.author
                    && *verdict != Verdict::Counted
            })
        });
        if all {
            return Ok(true);
        }
        if tokio::time::Instant::now() > deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Every entry a modified member device forges — under another member's
/// name, under no member's key, naming a point before its writer's chain,
/// as an act its actor's role or point does not allow, as a statement or a
/// join under a wrong key — is held on every member device and read by
/// nothing there, and a later session between two member devices finds no
/// difference. Paired: the genuine records, roles and devices beside them
/// read as before, on every member device.
#[allow(clippy::too_many_lines)] // one scenario: each forgery beside what it forges
#[tokio::test(flavor = "multi_thread")]
async fn every_forgery_is_held_on_every_member_device_and_read_by_nothing() -> Result<()> {
    let phones = [
        quiet_node().await?,
        quiet_node().await?,
        quiet_node().await?,
    ];
    let [alice_phone, bob_phone, carol_phone] = &phones;
    let mut reports = Vec::new();
    for phone in &phones {
        reports.push(phone.take_pod_verdicts().context("taken once")?);
    }
    let (alice, _) = c::host(alice_phone).await?;
    let (bob, _) = c::host(bob_phone).await?;
    let (carol, _) = c::host(carol_phone).await?;
    let pod = c::create(alice_phone, &alice).await?;
    for (phone, member) in [(bob_phone, &bob), (carol_phone, &carol)] {
        c::invite(
            alice_phone,
            &alice,
            pod,
            member,
            vec![c::device_of(phone, member)?],
        )
        .await?;
    }
    let tickets = c::tickets(alice_phone, &alice, pod).await?;
    for (phone, holder) in [(bob_phone, &bob), (carol_phone, &carol)] {
        phone.import_pod(holder.id, pod, tickets.clone()).await?;
    }
    for (phone, holder) in [(bob_phone, &bob), (carol_phone, &carol)] {
        for (other, member) in [
            (alice_phone, &alice),
            (carol_phone, &carol),
            (bob_phone, &bob),
        ] {
            let device = c::device_of(other, member)?;
            assert!(c::lists_device(phone, holder.id, pod, member.id, device).await?);
        }
    }

    let (alice_author, bob_author) = (
        alice_phone.default_author(alice.id)?,
        bob_phone.default_author(bob.id)?,
    );
    let claim = c::place_claim(alice_phone, &alice, pod, 1).await?;
    let scan = RecordKey::ImmutableDocument {
        member: bob.id,
        id: RecordId::from_bytes([2; 16]),
        mseq: Seq::FIRST,
    };
    let note = RecordKey::Operation {
        member: alice.id,
        id: RecordId::from_bytes([3; 16]),
        op: OpId {
            writer: alice.id,
            author: alice_author,
            mseq: Seq::FIRST,
            op_seq: 1,
        },
    };
    bob_phone
        .write_pod_entry(bob.id, pod, PodStore::Records, &scan.to_bytes(), b"scan")
        .await?;
    alice_phone
        .write_pod_entry(alice.id, pod, PodStore::Records, &note.to_bytes(), b"milk")
        .await?;

    let stranger = bob_phone.create_author(bob.id).await?;
    let (erin, mallory) = (c::Person::generate(), c::Person::generate());
    let event = |subject: PdnId, seq: u64, kind, actor: PdnId| MembershipKey::Event {
        subject,
        seq: Seq::new(seq),
        kind,
        actor,
        actor_seq: Seq::FIRST,
    };
    let operation = |author: AuthorId, mseq: u64| RecordKey::Operation {
        member: alice.id,
        id: RecordId::from_bytes([3; 16]),
        op: OpId {
            writer: bob.id,
            author,
            mseq: Seq::new(mseq),
            op_seq: 1,
        },
    };
    let forgery = |phone, writer, author, store, key: Vec<u8>, payload: Vec<u8>| Forgery {
        phone,
        writer,
        author,
        store,
        key,
        payload,
    };
    let (records, membership) = (PodStore::Records, PodStore::Membership);
    let forged = vec![
        // Under another member's claim.
        forgery(
            bob_phone,
            bob.id,
            bob_author,
            records,
            claim_key(&claim),
            b"forged".to_vec(),
        ),
        // An owner's entry at another member's immutable-document.
        forgery(
            alice_phone,
            alice.id,
            alice_author,
            records,
            scan.to_bytes(),
            b"forged".to_vec(),
        ),
        // An operation under no member's key.
        forgery(
            bob_phone,
            bob.id,
            stranger,
            records,
            operation(stranger, 1).to_bytes(),
            b"eggs".to_vec(),
        ),
        // An operation naming a point before its writer's chain.
        forgery(
            bob_phone,
            bob.id,
            bob_author,
            records,
            operation(bob_author, 0).to_bytes(),
            b"eggs".to_vec(),
        ),
        // A plain member's promotion of itself, and its removal of another.
        forgery(
            bob_phone,
            bob.id,
            bob_author,
            membership,
            event(bob.id, 2, EventKind::Promoted, bob.id).to_bytes(),
            vec![0],
        ),
        forgery(
            bob_phone,
            bob.id,
            bob_author,
            membership,
            event(carol.id, 2, EventKind::Removed, bob.id).to_bytes(),
            vec![0],
        ),
        // An owner's removal and demotion of itself.
        forgery(
            alice_phone,
            alice.id,
            alice_author,
            membership,
            event(alice.id, 2, EventKind::Removed, alice.id).to_bytes(),
            vec![0],
        ),
        forgery(
            alice_phone,
            alice.id,
            alice_author,
            membership,
            event(alice.id, 2, EventKind::Demoted, alice.id).to_bytes(),
            vec![0],
        ),
        // Another member's device statement under the writer's key.
        forgery(
            bob_phone,
            bob.id,
            bob_author,
            membership,
            MembershipKey::Devices {
                member: carol.id,
                version: 2,
            }
            .to_bytes(),
            bob.keys
                .device_statement(
                    2,
                    vec![
                        c::device_of(carol_phone, &carol)?,
                        c::device_of(bob_phone, &bob)?,
                    ],
                )
                .encode(),
        ),
        // A join under a key that does not derive its member's `PdnId`.
        forgery(
            bob_phone,
            bob.id,
            bob_author,
            membership,
            event(erin.id, 1, EventKind::Joined, bob.id).to_bytes(),
            mallory.keys.join_statement(&pod, Seq::FIRST).encode(),
        ),
    ];
    for forgery in &forged {
        forgery
            .phone
            .write_pod_entry_as_for_test(
                forgery.writer,
                pod,
                forgery.store,
                forgery.author,
                &forgery.key,
                &forgery.payload,
            )
            .await?;
    }

    let members = [
        (alice_phone, &alice),
        (bob_phone, &bob),
        (carol_phone, &carol),
    ];
    for (from, holder) in members {
        for (to, callee) in members {
            if holder.id == callee.id {
                continue;
            }
            for store in [PodStore::Membership, PodStore::Records] {
                dial(from, holder.id, pod, store, to, callee.id).await?;
            }
        }
    }
    for ((phone, holder), reports) in members.into_iter().zip(&mut reports) {
        assert!(
            holds_every_forgery_unread(phone, reports, holder.id, pod, &forged).await?,
            "a member device does not hold every forgery, or counts one"
        );
        // The wait above takes a forgery whose payload is still on its way,
        // so the genuine records' payloads may be too.
        for record in [claim, scan.record()] {
            assert!(c::reads(phone, holder.id, pod, record).await?);
        }
        assert!(
            test_utils::eventually(|| async {
                Ok(!phone
                    .read_pod_operations(holder.id, pod, &note.record())
                    .await?
                    .is_empty())
            })
            .await?
        );
        assert_eq!(
            phone
                .read_pod_record(holder.id, pod, &claim)
                .await?
                .as_deref(),
            Some(&b"claim"[..])
        );
        assert_eq!(
            phone
                .read_pod_record(holder.id, pod, &scan.record())
                .await?
                .as_deref(),
            Some(&b"scan"[..])
        );
        let writers: Vec<PdnId> = phone
            .read_pod_operations(holder.id, pod, &note.record())
            .await?
            .into_iter()
            .map(|op| op.id.writer)
            .collect();
        assert_eq!(writers, [alice.id]);
        for (member, want) in [(&alice, OWNER), (&bob, PLAIN), (&carol, PLAIN)] {
            assert_eq!(c::state_on(phone, holder.id, pod, member.id).await, want);
        }
        assert_eq!(
            c::state_on(phone, holder.id, pod, erin.id).await,
            MemberState::default()
        );
        let carols: BTreeSet<_> = phone
            .pod_membership(holder.id, pod)
            .await?
            .member(&carol.id)
            .map(|member| member.devices.clone())
            .unwrap_or_default();
        assert_eq!(carols, BTreeSet::from([c::device_of(carol_phone, &carol)?]));
    }

    // A later session, by the forger and by a device that only received.
    for (from, holder, to, callee) in [
        (bob_phone, &bob, alice_phone, &alice),
        (alice_phone, &alice, carol_phone, &carol),
    ] {
        for store in [PodStore::Membership, PodStore::Records] {
            let mut sessions = from.watch_pod_sessions(holder.id, pod, store).await?;
            dial(from, holder.id, pod, store, to, callee.id).await?;
            let session = sessions.next_with(to.node_id(), true, TIMEOUT).await?;
            assert!(
                session.is_some_and(|session| session.exchanged == Ok((0, 0))),
                "a later session between member devices found a difference"
            );
        }
    }

    for phone in phones {
        phone.shutdown().await?;
    }
    Ok(())
}

/// The key of a claim's entry at its member's first sequence.
fn claim_key(claim: &pdn_types::RecordRef) -> Vec<u8> {
    RecordKey::Claim {
        member: claim.member,
        id: claim.id,
        mseq: Seq::FIRST,
    }
    .to_bytes()
}
