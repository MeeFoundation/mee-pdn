# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Directory layout

Top of `/Users/theman/work/mee-pdn/`:

| Path        | What it is                                                              |
| ----------- | ----------------------------------------------------------------------- |
| `crates/`   | The rebuilt workspace — draft crates (see below).                       |
| `mia-docs/` | Sibling repo cloned in-place (gitignored) — UWill ADRs, openspec specs. |

## Project (rebuilt workspace)

Mee PDN — a decentralized, local-first data platform in Rust focused on
privacy and user sovereignty. Monorepo using Cargo workspaces.

### Crates (drafts)

- [`crates/mee-types`](crates/mee-types/) — shared domain types: `define_byte_id!`, `NodeId`, `Aid`, `OperationalKey`, `MeeId`, `MeeIdentityProof`, `NonEmpty<T>`.
- [`crates/mee-sync-api`](crates/mee-sync-api/) — holds `NamespaceId`, `EntryPath`, `EntryInfo`, `NamespaceRole`.
- [`crates/uwill`](crates/uwill/) — UWill capability tokens: `UwillCapability`, `WillowCommand`, `CapabilityCid`, `ValidityWindow`; transport-independent, future home of chain validation.
- [`crates/mee-pdn-layer`](crates/mee-pdn-layer/) — draft of the PDN-layer AST.
- [`crates/mee-willow-layer`](crates/mee-willow-layer/) — draft of the middle layer between PDN and iroh: the `WillowLayer` trait; re-exports the `uwill` types it speaks in.
- [`crates/mee-sync-iroh-docs`](crates/mee-sync-iroh-docs/) — data-layer adapter over the forked iroh-docs (local checkout at `../iroh-docs`, capability-gated ingest per ADR-0008): `SyncNode` stack assembly, namespace registry, `IngestPolicy` gate. Everything Mee-specific lives here so the fork stays iroh-native and minimal.
- [`crates/iroh-docs-experiment`](crates/iroh-docs-experiment/) — scenario tests driving the public API of `mee-sync-iroh-docs`.

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
