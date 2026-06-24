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
`pdn-types`; the future `pdn-node` runtime glues them together.

### Crates

- [`crates/pdn-types`](crates/pdn-types/) — platform primitives (`define_byte_id!`, `PdnId`, `PdnIdentityProof`, `Aid`, `OperationalKey`, `ClaimId`, `NodeId`, `NonEmpty<T>`) plus the data vocabulary (`NamespaceId` = `(about, issued_by)`, `EntryPath`, `EntryInfo`, `NamespaceRole`, `NodeAddr`).
- [`crates/data-layer`](crates/data-layer/) — the data layer over the forked iroh-docs (git dependency on `github.com/MeeFoundation/pdn-store`, capability-gated ingest per ADR-0008): the entries-only `DataLayer` trait, `SyncNode` stack assembly, the `Binding` registry (`Data` namespaces vs the device-shared `Connections` / `PrivateMetadata` stores), the `IngestPolicy` gate (`SelfOwned` = Invariant 1, the "admit my own identity's replicas" rule + `AnyOf` + the naive in-memory `ConnectionsPolicy`), and the device-replicated `ConnectionsStore` (an identity's connections) and `PrivateMetadataStore` (its devices + the tickets to its other stores — the bootstrap directory). Scenario tests in its `tests/`. Capability _semantics_ stay above: tokens are opaque payloads here, policies are injected. Invariants are numbered in `mia-docs/openspec/specs/architecture/invariants.md` (referenced by number, never name). To hack on the fork locally, `[patch]` it to `../pdn-store` (see the workspace `Cargo.toml` comment).
- [`crates/pdn-layer`](crates/pdn-layer/) — the platform surface products consume: domain model (`Claim`, `Attribute`, `Capability`, `Connection`, `Invite`), the `PdnOp` operation AST, and the `uwill` module (capability-token format, future chain validation). No iroh dependencies.

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
