# Upstream

What this crate took from n0's `iroh-docs`, and what was done here with each upstream commit and pull request read since the fork. Upstream is <https://github.com/n0-computer/iroh-docs>, branch `main`. The crate is the platform's own (ADR-0016): it holds what the platform uses, and no upstream patch is expected to apply to it as it is.

## Two duties

**A watch on fixes, at every bump of iroh, iroh-blobs or iroh-gossip in the workspace.** The next pass is due at the next such bump. It reads `git log <hash of the newest commit entry>..upstream/main` and upstream's open pull requests titled as fixes. Each one that touches code this crate still holds gets an entry with a verdict — adapted, already fixed here, not applicable, or confirmed and recorded as a change of its own — and one about code the crate no longer holds gets a one-line "not applicable". The pass then updates the section below.

**A look at upstream before a change that needs the store to do something new.** Such a change reads upstream's code and its open pull requests for an answer first, and its design says what it found.

## Last pass

2026-10-16. Upstream's `main` holds no commit past `8cfeacb`. Its open pull requests titled as fixes are #114, #112, #111, #106, #94, #93 and #88, each with an entry below.

## Pull requests

### #114 · anchpop · fix!: return the sync outcome from BobState::run

**Already fixed here.** Upstream's `BobState` holds its progress as an option, takes it out at the start of a round and puts it back only when the round succeeds, so `into_outcome` panics after a failed round. Here `BobState::progress` in `src/net/codec.rs` is a plain `SyncOutcome`, each round works on a clone of it, and a failed round leaves what the completed rounds accumulated; `a_failed_round_leaves_the_outcome_readable` and `a_failed_round_keeps_what_completed_rounds_accumulated` in the same file prove it. No action.

### #112 · vladimirlogachev · fix(sync): retry missing content downloads on every successful sync · iroh-docs-002

**Already here: this crate's fix, offered upstream.** Parked content is keyed by namespace and hash, and every successful session asks its peer for the content its namespace has parked, in `on_sync_finished` in `src/engine/live.rs`; `sync_fetches_parked_content_from_later_sync_peer` in `tests/sync.rs` proves it. No action.

### #111 · vladimirlogachev · fix(sync): clear connect state when the remote aborts a dial as already-syncing · iroh-docs-001

**Already here: this crate's fix, offered upstream.** `NamespaceStates::abort_connect` in `src/engine/state.rs` returns the pair to idle when the remote aborts the dial as already syncing, sparing an accept that took the slot over; `abort_connect_returns_rejected_dial_to_idle` and `abort_connect_spares_accept_takeover` there prove it. The crate also dials such a pair again, which the pull request does not. No action.

### #106 · cbenhagen · fix(store): keep records-by-key index in sync on prefix delete

**Not applicable:** prefix deletion is gone here, since an entry affects only its own key.

### #94 · emrul · fix(sync): bound entry key size in validate_entry to prevent replica exhaustion

**Already fixed here.** `validate_entry` in `src/sync.rs` refuses a key longer than `MAX_KEY_BYTES`, 8,192 bytes, before it checks any signature, on a local write and at ingest alike; `a_key_over_the_bound_is_refused_on_a_local_write` in `src/sync.rs` and `an_entry_with_a_key_over_the_bound_is_dropped_at_ingest` in `src/net/codec.rs` prove it. Upstream's bound is 4,096 bytes; the sizes differ and nothing on the wire does. No action.

### #93 · emrul · fix(store): bound MemPublicKeyStore cache to prevent memory exhaustion

**Confirmed here, recorded as the change `public-key-cache-bound`.** `MemPublicKeyStore` in `src/store/pubkeys.rs` caches every key that decodes, without a bound, for as long as the store lives. `SignedEntry::verify` parses an entry's namespace and author keys through it before checking either signature, and `validate_entry` runs the ingest verdict after that, so a modified node in a session the access provider admits — a member's device on a pod's stores — leaves one cached key behind for every entry it sends under a fresh author, whether the entry is then dropped as badly signed or as not admitted. That is memory spent without bound by what a modified node sends, which obliges a fix.

### #88 · cbenhagen · fix: retry incomplete blob downloads after sync

**Already fixed here, another way.** Of the pull request's four cases: content a peer lacked is asked for from the peer of every later successful session (#112 above); content a restart left missing is parked by a scan at the first `start_sync` of a namespace in the live actor's life (`content_a_restart_left_missing_is_fetched_from_the_first_sync_peer` in `tests/sync.rs`); a download that fails toward a gossip neighbor is tried again (the provider's series of pauses in `src/engine/live/fetches.rs`); and a change of download policy does not apply, the crate holding no download policies. Upstream's scan of the whole replica at the end of every session is not taken: the scan here runs once per namespace per start. No action.

## Commits

An upstream commit touching what this crate changed is adapted, not taken as it is; the crate's `CLAUDE.md` lists those changes under "Conventions & gotchas". The ordered subscription and the session id there are the ones a change to `live::Event`, `SyncEvent`, the live actor's handling of replica events or `connect_and_sync` meets.

A hash of this crate's own history dated before 2026-08-25 belongs to the former `MeeFoundation/pdn-store` repository, merged into this tree by `0b66bea` ("Implement pdn-store-in-tree spec"); it does not resolve here. An entry that cites one for a fix also names the code and the test that carry it, and those are the pointers to follow.

### 2026-08-19 · `8cfeacb` · Franz Heinzmann · fix: Improve API around untrusted range bounds (#119)

**One half not applicable, the other half adapted** on 2026-09-06. The commit does two things. It replaces upstream's `clamp_to_namespace` with a single `RecordsBounds::untrusted` constructor, which is moot here: `get_range` in `src/store/fs.rs` refuses a range whose boundary names another namespace instead of clamping it, so there is no untrusted-bounds constructor to narrow. It also pins `prefixes_of`, `prefixed_by` and `remove_prefix_filtered` to the session namespace rather than to the namespace of the caller-supplied record identifier. `prefixes_of` and `remove_prefix_filtered` do not exist here, since an entry affects only its own key; `entry_get`, which `put` reads through, and `prefixed_by` refuse such an identifier instead (`ensure_own_namespace` in `src/store/fs.rs`), as `get_range` refuses a foreign boundary: `put` only ever sees an entry `validate_entry` has passed, so a foreign identifier is a caller's defect rather than a request to redirect. `an_identifier_naming_another_namespace_is_refused` in `src/sync.rs` proves it, `put` included.

### 2026-08-19 · `1aca659` · Franz Heinzmann · fix(sync): reject record identifiers shorter than 64 bytes (#118)

**Already fixed here, independently and earlier.** `234f706` (2026-08-02) gave `RecordIdentifier` a hand-written `Deserialize` in `src/sync.rs` that refuses a buffer shorter than the fixed namespace-and-author head, closing the same panic in the sync actor thread; `a_short_record_identifier_is_refused_by_the_decoder` in the same file proves it. Ours covers both shapes serde's derive accepts for a newtype struct; upstream's covers the one postcard emits. The accepted wire encoding is identical, so nothing diverges. No action.

### 2026-08-19 · `c3c0ccd` · Jonas Wrede · fix: clamp remote sync ranges to the session namespace (#117)

**Same defect, already fixed here with a different resolution.** All namespaces share one records table, so a sync range naming a foreign namespace would read that document's entries and echo them back to the peer. `2e035d0` (2026-07-29) closed it three weeks before upstream by refusing such a range: reconciliation derives every boundary from a key of the replica under exchange, so a foreign boundary is a protocol violation, not a request to narrow. The check is in `get_range` in `src/store/fs.rs`; `a_sync_range_naming_another_namespace_is_refused` in `src/sync.rs` proves the refusal, and `a_failed_round_leaves_the_outcome_readable` in `src/net/codec.rs` shows what the peer gets — the round fails and nothing is sent. Upstream clamps instead and serves the intersection. The clamp is deliberately not adopted — it turns a violation into a silent empty result, and the refusal is the tighter contract. No action.

### 2026-08-17 · `f8d48e1` · Emil Sayahi · docs: fix `SyncEvent` property comments (#116)

**Adapted** on 2026-09-06. The doc comments on `SyncEvent::finished` and `SyncEvent::started` in `src/engine/live.rs` were swapped; the two lines now sit on the fields they describe, as upstream has them. The values were always assigned correctly — `finished` is taken at event construction, `started` comes from the sync state — so only rustdoc changes.

### 2026-07-30 · `ad80e69` · Floris Bruynooghe · chore: run scheduled CI jobs earlier (#115)

**Not applicable.** Upstream's own workflow files and lockfile. The crate carries no workflows in this workspace: the workspace's pipeline lints and tests it, and the nightly workflow repeats its integration tests with the workspace's scenarios.

### 2026-07-15 · `b53c317` · Franz Heinzmann · fix: don't abort receive loop on invalid message (#110)

**Adapted** on 2026-09-06. `receive_loop` in `src/engine/gossip.rs` decoded a gossip message with `?`, so one undecodable message ended the receive loop for that namespace, which was then logged and dropped from the active set; it now skips the message with a debug line, as upstream does. The swarm here is content-free, so what a peer could silence this way was `ContentReady` and `SyncReport` delivery, not entry flow — entries move over reconciliation. The regression test is `sync_continues_after_invalid_gossip_message` in `tests/sync.rs`, with one rearrangement: upstream sleeps 2 seconds so the malformed message lands before the next write, which proves nothing on a slow run; here the sender leaves the topic right after the message, and the subscriber's `NeighborDown` for it — ordered behind the message on the same connection — is asserted before the write.

### 2026-06-15 · `091e8ca` · dignifiedquire · chore: Release iroh-docs version 0.101.0

**The baseline.** Merged here on 2026-07-12 as `ba1d513` ("Merge remote-tracking branch 'upstream/main'"); the crate carried this version number until it took the workspace's. Upstream history at or below this commit is in the tree already.

**Two parts of the baseline are removed here**, on 2026-10-14. The first is the redb 2.x migration of `fc89461` (Ben Hagen, 2026-06-01, "fix(store): migrate redb 2.x tuple tables on open (#105)"): the `redb-v2-migration` feature, its `redb_v3` dependency, `src/store/fs/migrate_redb_v2_tuples.rs` and the two tests of that migration. It converts stores written by iroh-docs 0.94 to 0.98, and the workspace has locked iroh-docs only from 0.100 on, on redb 4, so no node of ours holds such a store; `Store::persistent` now opens the database without looking for that format, and an older store fails to open with redb's table type mismatch. The second is `src/store/fs/migrate_v1_v2.rs`, which no module has declared since `b257606` ("feat: upgrade redb to v3 compatible format (#69)") and no build compiles. An upstream commit touching either is not applicable here.

**What the platform does not call is removed here**, on 2026-10-16 (ADR-0016): the in-process RPC layer and its network variant, read capabilities, download policies, author listing, import, export and deletion, file import and export, the document calls no crate of the workspace makes, the query's sorting and offset, the schema migrations 001 to 004, the protect callback, `Docs` as a protocol handler of its own, the metrics no test reads, the feature flags and the wasm build. An upstream commit touching only these is not applicable here.
