# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Directory layout

Top of `/Users/theman/work/mee-pdn/`:

| Path        | What it is                                                              |
| ----------- | ----------------------------------------------------------------------- |
| `crates/`   | The rebuilt workspace — draft crates (see below).                       |
| `mia-docs/` | Sibling repo cloned in-place (gitignored) — UWill ADRs, openspec specs. |

## Project

PDN — a decentralized, local-first data platform in Rust focused on
privacy and user sovereignty. Built within the Mee organization for the
mia product, but not limited to it — org/product names stay out of the
code. Monorepo using Cargo workspaces.

Layers: `pdn-layer` (domain) / `data-layer` (sync) / iroh (bytes on the
wire). `pdn-layer` does NOT depend on `data-layer` — both see only
`pdn-types`; the `pdn-node` runtime glues them together (today it glues
only `data-layer`; `pdn-layer` joins in a later change).

### Crates

- [`crates/pdn-types`](crates/pdn-types/) — platform primitives (`define_byte_id!`, `PdnId`, `PdnIdentityProof`, `Aid`, `OperationalKey`, `ClaimId`, `NodeId`, `NonEmpty<T>`) plus the data vocabulary (`NamespaceId` = `(about, issued_by)`, `EntryPath`, `EntryInfo`, `NamespaceRole`, `NodeAddr`).
- [`crates/data-layer`](crates/data-layer/) — the data layer over the forked iroh-docs (git dependency on `github.com/MeeFoundation/pdn-store`; the fork's `validate_entry` hook per ADR-0008 stays available but uninstalled): the entries-only `DataLayer` trait, `SyncNode` stack assembly (one node hosts the store sets of any number of identities; access is bounded by ticket possession until subset-rbsr and UWill land; the ceremony slot — spawn-time registration of externally supplied protocols plus a narrow dial handle — is consumed by pdn-node's two handlers, pairing per ADR-0011 and linking per ADR-0012, and stays protocol-agnostic here because the ceremonies' semantics live in pdn-node), the device-replicated `PrivateMetadataStore` (the one directory of an identity's own state: its devices, the typed tickets to its other stores, and its connections records — plus the namespace accessor and the bounded catch-up wait the linking ceremony stands on), the cross-identity `ConnectionMetadataStore` (one replica per connection direction, issuer-written / counterparty-read per Invariant 3, carrying grants keyed by data-store issuer), and `forget_namespace` (the rollback counterpart of `import_namespace` — unregisters the issuer, not just drops the replica). Scenario tests in its `tests/`. Capability _semantics_ stay above: tokens are opaque payloads here. Invariants are numbered in `mia-docs/openspec/specs/components/pdn-node/invariants.md` (referenced by number, never name). To hack on the fork locally, `[patch]` it to `../pdn-store` (see the workspace `Cargo.toml` comment).
- [`crates/pdn-layer`](crates/pdn-layer/) — the platform surface products consume: domain model (`Claim`, `Attribute`, `Capability`, `Connection`, `Invite`), the `PdnOp` operation AST, and the `uwill` module (capability-token format, future chain validation). No iroh dependencies.
- [`crates/pdn-node`](crates/pdn-node/) — the embeddable runtime core: identity / connections / data / sync services as thin glue over `data-layer`, plus the runtime's two ceremonies. Pairing (ADR-0011): connections are produced by establishment — `invite` mints a one-time secret and a bearer-free payload, `establish` dials the pairing ALPN and runs the verify-and-burn dialogue, and the grant surface publishes/reads whole-store tickets over the connection's metadata pair (no manual recording). Device linking (ADR-0012, the `linking` module next to `pairing`): `linking_invite` mints a one-time secret and a bearer-free payload, `link` dials the linking ALPN — the inviter verifies-and-burns, registers the newcomer's device record itself (from the connection's authenticated peer id), and replies with fresh directory and data-namespace write tickets; `link` imports both and returns caught up, rolling both back on failure. Each `Runtime` is one running node hosting any number of identities, each added by an explicit act (create, or link from a payload); the runtime is the single owner of node assembly — both protocol handlers thread through `spawn` into data-layer's protocol slot — and of the hosted identities' store handles. Services are traits with one production implementation (`IdentityService` — a KERI-backed second implementation is the live prospect; the current one mints placeholder `PdnId`s). Whole-store ticket share/import in the data service is the interim access model for cross-identity namespaces, replaced when capability-scoped sharing lands. Scenario tests in its `tests/`. No host or HTTP dependencies.
- [`crates/pdn-node-http`](crates/pdn-node-http/) — the thin HTTP host for the demo stand: an axum binary embedding one runtime. `GET /live` always on; `/debug/` routes are demo scaffolding behind `PDN_DEBUG=1` (absent otherwise, shape unpinned). Env: `PDN_HOST` / `PDN_PORT` (default `127.0.0.1:3011`). Depends on `pdn-node` only — no direct `data-layer` dependency.

## Commands

Task runner is [just](https://github.com/casey/just). Key recipes:

- `just build` — `cargo build --workspace`
- `just test` — `cargo test --workspace --all-targets`
- `just check` — `cargo fmt --check` + `cargo clippy --workspace --all-targets` + `cargo check`
- `just check-fix` — auto-fix formatting/linting, then re-check
- `just precommit-check` — `check` + `test`
- `just precommit-fix` — `check-fix` + `test`
- `just setup-tooling` — `cargo install cargo-watch` + add wasm targets

Run a single crate's tests: `cargo test -p <crate-name>`.
Run a single test: `cargo test -p <crate-name> <test_name>`.

## Lint rules

Strict safety-first linting via workspace `Cargo.toml`. These are **denied**:

- `unwrap_used`, `panic`, `todo`, `unimplemented` — no panicking in non-test code
- `dbg_macro`, `print_stdout`, `print_stderr` — no debug output
- `unsafe_code`, `undocumented_unsafe_blocks`, `multiple_unsafe_ops_per_block`
- `exit` — no `process::exit`
- `mem_forget`, `string_to_string`, `infinite_loop`, `unused_must_use`,
  `non_ascii_idents`

Relaxed in test code via `clippy.toml` (allows unwrap/expect/print/dbg/indexing).

`expect_used`, `indexing_slicing`, `as_conversions`, `str_to_string`,
`unwrap_in_result`, `redundant_clone`, `large_futures` are **warnings** —
use `.get()` and `TryFrom`/`TryInto` where possible.

Formatting: `max_width = 100` (rustfmt.toml). Cognitive complexity threshold: 15.
Max lines per function: 80. Future-size threshold: 16384 (clippy's default;
iroh's `Endpoint::bind`/`spawn` futures are ~10KB structurally, so the
earlier 8192 threshold flagged every iroh await point once iroh came into
actual use in `iroh-docs-experiment`).

## Code practices

Cross-cutting practices live in `mia-docs/openspec/specs/code-practices/`:

- [`access-control-tests.md`](mia-docs/openspec/specs/code-practices/access-control-tests.md) — every test that asserts authorized access must, in the same place, assert the tightest unauthorized party is denied (read: an outsider, and a holder of the store's ticket but no read capability; write: a lower-level holder). A positive-only access test verifies nothing.
- [`flaky-tests.md`](mia-docs/openspec/specs/code-practices/flaky-tests.md) — every substantial change ends with a flaky-test stress pass, before anything is built on top. After landing a change that touches sync, linking, engine wiring, or bumps iroh/pdn-store, stress the affected scenario tests in a counted loop and treat any failure as a defect of that change, diagnosed in isolation from other work. Full discipline — reproduction sizing (hundreds of runs, rule of three), fix minimization, deterministic pinning — in the spec. This exists so we never again build a feature first and then debug the previous implementation's flaky tests through it.
