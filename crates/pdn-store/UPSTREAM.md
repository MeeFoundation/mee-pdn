# Upstream commits

What has landed in n0's `iroh-docs` since this crate was forked from it, and what was done here with each commit. Upstream is <https://github.com/n0-computer/iroh-docs>, branch `main`. Newest commit first; the bottom entry is the last upstream commit merged into this crate, so everything below it is upstream history already in the tree.

Refresh the list by fetching upstream and reading `git log <bottom hash>..upstream/main`. Every new commit gets an entry with a verdict, including the ones that need no action — an absent entry means the commit was never looked at, which is what this file exists to make visible.

A hash of this crate's own history dated before 2026-08-25 belongs to the former `MeeFoundation/pdn-store` repository, folded into this tree by `0b66bea` ("Implement pdn-store-in-tree spec"); it does not resolve here. An entry that cites one for a fix also names the code and the test that carry it, and those are the pointers to follow.

## 2026-08-19 · `8cfeacb` · Franz Heinzmann · fix: Improve API around untrusted range bounds (#119)

**One half not applicable, the other half adapted** on 2026-09-06. The commit does two things. It replaces upstream's `clamp_to_namespace` with a single `RecordsBounds::untrusted` constructor, which is moot here: `get_range` in `src/store/fs.rs` refuses a range whose boundary names another namespace instead of clamping it, so there is no untrusted-bounds constructor to narrow. It also pins `prefixes_of`, `prefixed_by` and `remove_prefix_filtered` to the session namespace rather than to the namespace of the caller-supplied record identifier. Here the three refuse such an identifier instead (`ensure_own_namespace` in `src/store/fs.rs`), as `get_range` refuses a foreign boundary: `put` only ever sees an entry `validate_entry` has passed, so a foreign identifier is a caller's defect rather than a request to redirect, and pinning would have `remove_prefix_filtered` delete this namespace's rows under another entry's key. `an_identifier_naming_another_namespace_is_refused` in `src/sync.rs` proves it, `put` included.

## 2026-08-19 · `1aca659` · Franz Heinzmann · fix(sync): reject record identifiers shorter than 64 bytes (#118)

**Already fixed here, independently and earlier.** `234f706` (2026-08-02) gave `RecordIdentifier` a hand-written `Deserialize` in `src/sync.rs` that refuses a buffer shorter than the fixed namespace-and-author head, closing the same panic in the sync actor thread; `a_short_record_identifier_is_refused_by_the_decoder` in the same file proves it. Ours covers both shapes serde's derive accepts for a newtype struct; upstream's covers the one postcard emits. The accepted wire encoding is identical, so nothing diverges. No action.

## 2026-08-19 · `c3c0ccd` · Jonas Wrede · fix: clamp remote sync ranges to the session namespace (#117)

**Same defect, already fixed here with a different resolution.** All namespaces share one records table, so a sync range naming a foreign namespace would read that document's entries and echo them back to the peer. `2e035d0` (2026-07-29) closed it three weeks before upstream by refusing such a range: reconciliation derives every boundary from a key of the replica under exchange, so a foreign boundary is a protocol violation, not a request to narrow. The check is in `get_range` in `src/store/fs.rs`; `a_sync_range_naming_another_namespace_is_refused` in `src/sync.rs` proves the refusal, and `a_failed_round_leaves_the_outcome_readable` in `src/net/codec.rs` shows what the peer gets — the round fails and nothing is sent. Upstream clamps instead and serves the intersection. The clamp is deliberately not adopted — it turns a violation into a silent empty result, and the refusal is the tighter contract. No action.

## 2026-08-17 · `f8d48e1` · Emil Sayahi · docs: fix `SyncEvent` property comments (#116)

**Adapted** on 2026-09-06. The doc comments on `SyncEvent::finished` and `SyncEvent::started` in `src/engine/live.rs` were swapped; the two lines now sit on the fields they describe, as upstream has them. The values were always assigned correctly — `finished` is taken at event construction, `started` comes from the sync state — so only rustdoc changes.

## 2026-07-30 · `ad80e69` · Floris Bruynooghe · chore: run scheduled CI jobs earlier (#115)

**Not applicable.** Upstream's own workflow files and lockfile. The crate carries no workflows in this workspace — the pipeline's `store` job runs its checks, and the nightly workflow runs its tests marked `#[ignore = "flaky"]`.

## 2026-07-15 · `b53c317` · Franz Heinzmann · fix: don't abort receive loop on invalid message (#110)

**Adapted** on 2026-09-06. `receive_loop` in `src/engine/gossip.rs` decoded a gossip message with `?`, so one undecodable message ended the receive loop for that namespace, which was then logged and dropped from the active set; it now skips the message with a debug line, as upstream does. The swarm here is content-free, so what a peer could silence this way was `ContentReady` and `SyncReport` delivery, not entry flow — entries move over reconciliation. The regression test is `sync_continues_after_invalid_gossip_message` in `tests/sync.rs`, with one rearrangement: upstream sleeps 2 seconds so the malformed message lands before the next write, which proves nothing on a slow run; here the sender leaves the topic right after the message, and the subscriber's `NeighborDown` for it — ordered behind the message on the same connection — is asserted before the write.

## 2026-06-15 · `091e8ca` · dignifiedquire · chore: Release iroh-docs version 0.101.0

**The baseline.** Merged here on 2026-07-12 as `ba1d513` ("Merge remote-tracking branch 'upstream/main'"), and the version this crate carries. Upstream history at or below this commit is in the tree already.
