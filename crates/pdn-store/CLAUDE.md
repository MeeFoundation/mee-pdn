# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project overview

`iroh-docs` implements multi-dimensional key-value *documents* (called **Replicas**) that synchronize between peers using **range-based set reconciliation** (Aljoscha Meyer's algorithm, [paper](https://arxiv.org/abs/2212.13567)).

Two non-obvious facts shape the whole design:

- **Documents store hashes, not content.** Each *Entry* maps a key to the BLAKE3 hash, size, and timestamp of some content — the content bytes themselves are never stored in or transferred through a replica. Actual blob transfer is delegated to `iroh-blobs`.
- **`Docs` is a "meta-protocol."** It composes `iroh-blobs` (content) and `iroh-gossip` (live notification) on top of an `iroh` endpoint. Setting up `Docs` requires wiring up `Blobs` and `Gossip` too (see `examples/setup.rs` and the README).

Entries are signed by two keypairs: a **Namespace** key (write capability; its public `NamespaceId` is the replica's unique id) and an **Author** key (proof of authorship; any number of `AuthorId`s, with app-specific meaning).

## Common commands

One crate of the `mee-pdn` workspace, at `crates/pdn-store`; every command runs from the workspace root through `just`. Cargo knows the package by its own name, `iroh-docs` — `-p iroh-docs` — while `pdn-store` is the workspace alias consumers write in their manifests.

- **Format**: `cargo fmt --all` under the workspace `rustfmt.toml`; `just check` verifies it. Imports stay grouped std / external / crate with one `use` per crate, as upstream keeps them — stable rustfmt preserves that grouping rather than imposing it, so a new import is placed in its group by hand.
- **Test** (nextest): `just test -p iroh-docs` runs the default feature set; `-E 'test(test_name)'` selects one test. `just test-store` adds `--all-features`, `--no-default-features`, and the doctests — nextest runs none, and the README example is one. Three tests carry `#[ignore = "flaky"]` and run only in the nightly workflow; `just stress -p iroh-docs --run-ignored ignored-only` runs them by hand.
- **Lint**: `just check` lints the workspace under default features; `just check-store` adds clippy on the other two feature sets and rustdoc, each with warnings denied.
- **wasm**: `just check-store` builds `wasm32-unknown-unknown` with `--no-default-features`, `getrandom_backend="wasm_js"` named through `RUSTFLAGS`.

## Pre-push checklist

Run all of this from the workspace root before a change here is committed — the pipeline's `store` job runs the same recipes, and every item is a blocking check:

1. `just check` — fmt, clippy, and type-check of the whole workspace under default features.
2. `just check-store` — clippy on `--all-features` and `--no-default-features` with warnings denied, rustdoc with warnings denied (an intra-doc link to a private item is one), the wasm32 build.
3. `just test` — the workspace's tests under default features, the doctests at the end.
4. `just test-store` — the tests under `--all-features` and `--no-default-features`, then the doctests under all features.
5. After a change to sync, the engine, or the wire format: the flaky-test stress pass the workspace `CLAUDE.md` asks for, over the scenarios in `crates/data-layer/tests` and `crates/pdn-node/tests` that reach the changed code.

Upstream's pipeline also runs cross builds (freebsd, i686-linux, android), `cargo-semver-checks`, an MSRV job, `cargo deny`, and codespell; none of them runs here.

## Feature flags

`default = ["metrics", "rpc", "fs-store", "redb-v2-migration"]`

- `rpc` — exposes the API over the network (via `noq` + `irpc/rpc`); without it, the API is in-process only.
- `fs-store` — persistent redb file storage; without it, only the in-memory backend is available.
- `redb-v2-migration` — pulls in `redb_v3` to migrate stores written by older redb major versions on open.
- `metrics` — `iroh-metrics` counters.

The test matrix exercises `all` / `none` / `default` feature sets — changes must compile and pass under all three.

## Architecture

Requests flow top-to-bottom; each layer is in its own module. The two key indirections are that **all store access is serialized through a dedicated actor thread**, and **live networking is coordinated by a separate async actor**.

```
Docs (protocol.rs)          ── iroh ProtocolHandler; entry point. Builder: Docs::memory()/persistent(path).spawn(endpoint, blobs, gossip)
  └─ DocsApi (api.rs)       ── irpc client API; derefs from Docs
       └─ RpcActor          ── tokio task; translates DocsProtocol messages → Engine calls (api/actor.rs)
            └─ Engine (engine.rs)        ── coordinates everything below; holds Endpoint, blob store, downloader, default author
                 ├─ SyncHandle (actor.rs)        ── store/replica operations
                 └─ LiveActor (engine/live.rs)   ── live sync coordination
```

**Data model & reconciliation core** (no I/O, no networking):
- `sync.rs` — the big one. `Replica`/`ReplicaInfo`, `SignedEntry`/`Entry`/`Record`/`RecordIdentifier`, `Capability`/`CapabilityKind`. `ProtocolMessage = ranger::Message<SignedEntry>` is what goes on the wire.
- `ranger.rs` — generic range-based set reconciliation. Defines the `RangeEntry`/`RangeKey`/`RangeValue`/`Store` traits and the `Message` exchange. The doc types in `sync.rs` implement these traits.
- `keys.rs` — `Author`/`AuthorId`, `NamespaceSecret`/`NamespaceId`, wrapping `iroh::SecretKey`/`PublicKey`.
- `heads.rs` — `AuthorHeads` (latest timestamp per author), used in sync reports for cheap "are we in sync?" checks.

**Storage** (`store.rs` + `store/fs/`):
- `store::Store` (re-export of `store::fs::Store`) is the *only* store implementation, always backed by [`redb`]. "In-memory" = redb on a `Vec<u8>` backend; "persistent" = redb on a single file. It implements `ranger::Store`, so reconciliation runs directly against redb.
- `store/fs/tables.rs` — redb table layout (records, the `records_by_key` index, namespaces, authors, `latest_per_author`, `namespace_peers`, `download_policy`).
- `store/fs/migrations.rs` runs in-place schema migrations (001–004) automatically on open. `migrate_v1_v2.rs` / `migrate_redb_v2_tuples.rs` handle redb *major-version* upgrades and are gated behind `redb-v2-migration`.
- `DownloadPolicy` / `FilterKind` decide which entries' blobs get downloaded.

**The sync actor** (`actor.rs`): `SyncHandle` is a cheaply-cloneable handle to a dedicated **`std::thread`** (`"sync-actor"`) that owns the `Store` and processes `Action` messages sequentially. It is a thread, not a tokio task, because **redb is blocking** — but on `wasm_browser` it falls back to a tokio task. All replica mutation goes through here; the last handle drop joins the thread. Prefer `SyncHandle::shutdown().await` over relying on drop to avoid blocking an async context.

**The live engine** (`engine/`): `Engine` ties the `SyncHandle`, a `LiveActor`, and a per-document gossip swarm together. `live.rs` is the coordinator — it accepts connections, drives syncs, and reacts to gossip `Op`s (`Put` / `ContentReady` / `SyncReport`) by triggering blob downloads. `state.rs` tracks per-namespace sync state (`Origin`, `SyncReason`); `gossip.rs` manages the swarm. The engine also installs a GC-protect callback so blobs referenced by docs aren't garbage-collected by `iroh-blobs`.

**Networking** (`net.rs` + `net/codec.rs`): ALPN is `/iroh-sync/1`. `connect_and_sync` is the initiator ("Alice"), `handle_connection` the responder ("Bob"); `codec.rs` holds the wire state machines (`run_alice`, `BobState`) that exchange `ranger::Message`s over an iroh QUIC bi-stream.

**RPC layer** (`api/`): `api/protocol.rs` defines `DocsProtocol` via the `irpc::rpc_requests` macro (each variant has `#[rpc(tx = ...)]` reply channels, gated by `rpc_feature = "rpc"`). The same `DocsApi` works in-process (`LocalSender`) or, with the `rpc` feature, over the network.

## Conventions & gotchas

- `#![deny(missing_docs, rustdoc::broken_intra_doc_links)]` at the crate root: every public item needs a doc comment and intra-doc links must resolve. Some internal modules opt out with `#![allow(missing_docs)]`. `missing_debug_implementations` is also warned.
- `EntrySignature` deliberately wraps `iroh::Signature` (not the raw `ed25519_dalek` type) to keep the on-wire `SignedEntry` format independent of upstream ed25519 serde changes — don't "simplify" this.
- `wasm_browser` is a `cfg` alias defined in `build.rs` (`all(target_family = "wasm", target_os = "unknown")`); use it to gate browser-specific code paths (notably the actor-as-task fallback).
- Property tests use `proptest` + `test-strategy`; regression seeds are checked in under `proptest-regressions/`.
