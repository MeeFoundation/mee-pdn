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
- [`crates/data-layer`](crates/data-layer/) — the data layer over the forked iroh-docs (git dependency on `github.com/MeeFoundation/pdn-store`; the fork's `validate_entry` hook per ADR-0008 stays available but uninstalled): the entries-only `DataLayer` trait, `SyncNode` stack assembly (one node hosts the store sets of any number of identities; access is bounded by ticket possession until subset-rbsr and UWill land; the ceremony slot — spawn-time registration of externally supplied protocols plus a narrow dial handle — is consumed by pdn-node's two handlers, pairing per ADR-0011 and linking per ADR-0012, and stays protocol-agnostic here because the ceremonies' semantics live in pdn-node), the device-replicated `PrivateMetadataStore` (the one directory of an identity's own state: its devices, the typed tickets to its other stores, and its connections records — plus the namespace accessor and the bounded catch-up wait the linking ceremony stands on), the cross-identity `ConnectionMetadataStore` (one replica per connection direction, issuer-written / counterparty-read per Invariant 3, carrying grants keyed by data-store issuer), and `forget_namespace` (the rollback counterpart of `import_namespace` — unregisters the issuer, not just drops the replica). Scenario tests in its `tests/`. Capability _semantics_ stay above: tokens are opaque payloads here. Invariants are numbered in `mia-docs/openspec/specs/components/pdn-node/invariants.md` (referenced by number, never name). To hack on the fork locally, `[patch]` it to `./pdn-store` (see the workspace `Cargo.toml` comment).
- [`crates/pdn-layer`](crates/pdn-layer/) — the platform surface products consume: domain model (`Claim`, `Attribute`, `Capability`, `Connection`, `Invite`), the `PdnOp` operation AST, and the `uwill` module (capability-token format, future chain validation). No iroh dependencies.
- [`crates/pdn-node`](crates/pdn-node/) — the embeddable runtime core: identity / connections / data / sync services as thin glue over `data-layer`, plus the runtime's two ceremonies. Pairing (ADR-0011): connections are produced by establishment — `invite` mints a one-time secret and a bearer-free payload, `establish` dials the pairing ALPN and runs the verify-and-burn dialogue, and the grant surface publishes/reads whole-store tickets over the connection's metadata pair (no manual recording). Device linking (ADR-0012, the `linking` module next to `pairing`): `linking_invite` mints a one-time secret and a bearer-free payload, `link` dials the linking ALPN — the inviter verifies-and-burns, registers the newcomer's device record itself (from the connection's authenticated peer id), and replies with fresh directory and data-namespace write tickets; `link` imports both and returns caught up, rolling both back on failure. Each `Runtime` is one running node hosting any number of identities, each added by an explicit act (create, or link from a payload); the runtime is the single owner of node assembly — both protocol handlers thread through `spawn` into data-layer's protocol slot — and of the hosted identities' store handles. Services are traits with one production implementation (`IdentityService` — a KERI-backed second implementation is the live prospect; the current one mints placeholder `PdnId`s). Whole-store ticket share/import in the data service is the interim access model for cross-identity namespaces, replaced when capability-scoped sharing lands. Scenario tests in its `tests/`. No host or HTTP dependencies.
- [`crates/pdn-node-http`](crates/pdn-node-http/) — the thin HTTP host for the demo stand: an axum binary embedding one runtime. `GET /live` always on; `/debug/` routes are demo scaffolding behind `PDN_DEBUG=1` (absent otherwise, shape unpinned). Env: `PDN_HOST` / `PDN_PORT` (default `127.0.0.1:3011`). Depends on `pdn-node` only — no direct `data-layer` dependency.

## Commands

Task runner is [just](https://github.com/casey/just). Key recipes:

- `just build` — `cargo build --workspace`
- `just test` — `cargo nextest run` (workspace tests via nextest; extra args forwarded). This workspace has no doctests, so nextest covers everything.
- `just stress` — flaky-hunt via nextest; all args forwarded to `cargo nextest run` (e.g. `just stress --stress-count 300 -E 'binary(linking)'`). See flaky-tests.md.
- `just check` — `cargo fmt --check` + `cargo clippy --workspace --all-targets` + `cargo check`
- `just check-fix` — auto-fix formatting/linting, then re-check
- `just precommit-check` — `check` + `test`
- `just fix` — `check-fix` + `test`
- `just setup-tooling` — installs `cargo-watch`, `cargo-nextest`, and wasm targets

Tests run under [cargo-nextest](https://nexte.st) (process-per-test, `--test-threads` defaults to CPU cores). It is a **required** tool: `just setup-tooling` installs it locally, CI installs it via `taiki-e/install-action`, and the devcontainer bakes it into the image (`.devcontainer/Dockerfile.app`). `just test`/`just stress` error out with a hint if it is missing.

Run a single crate's tests: `cargo nextest run -p <crate-name>`.
Run a single test: `cargo nextest run -E 'test(<test_name>)'`.

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
- [`flaky-tests.md`](mia-docs/openspec/specs/code-practices/flaky-tests.md) — every substantial change ends with a flaky-test stress pass, before anything is built on top. After landing a change that touches sync, linking, engine wiring, or bumps iroh/pdn-store, stress the affected scenario tests under nextest (`--stress-count`) and treat any failure as a defect of that change, diagnosed in isolation from other work. Full discipline — reproduction sizing (hundreds of runs, rule of three), fix minimization, deterministic pinning — in the spec. This exists so we never again build a feature first and then debug the previous implementation's flaky tests through it.

## Git

All git operations that change state — `commit`, `push`, `checkout`, `add`, `rebase`, etc. — are performed **by a human, never by Claude**, in this repo and in the nested `./pdn-store` checkout. Read-only commands (`status`, `diff`, `log`, `show`) are fine.

## pdn-store checks

Any modification of `./pdn-store` ends with running its pre-push checklist (`./pdn-store/CLAUDE.md`, section "Pre-push checklist") before committing: fmt, clippy for all three feature sets, `cargo +nightly docs-rs`, `cargo deny`, tests plus doctests, and the wasm build — all with warnings treated as errors, exactly as that repo's CI runs them.

## Code comments

- **Maximally brief, critical-only.** A comment earns its place by flagging an invariant, a contract edge, or a non-obvious why. Narrative, alternatives, and process history go.
- **Present tense only — never the code's or system's past or future.** No "legacy", "interim", "for now", "used to", "no longer", "until X lands", "arrives with UWill", "future work"; no review-finding references ("finding 8", "#4"), no PR numbers. Rewrite as present state or a present conditional ("without the guard the import would …"). ADR-XXXX and Invariant N references are fine; Dn references are not — those live in the change's design doc, not in code. Applies to all repos: mee-pdn, pdn-store.

## Docs

- **Each paragraph stands alone — the read-aloud test.** Understanding a paragraph must not require jumping to another text. The prose itself has to survive being read aloud: English is not the first language of every maintainer, and many sound the text out in their head while re-reading, so clumsy phrasing cuts reading speed several times over. Allowed bare references: ADR numbers, Invariant numbers, and D1..Dn where it is unambiguous which spec's decisions are meant — inside the change's own bundle (design/proposal/tasks) bare Dn is fine; in delta specs qualify it ("subset-rbsr D3"), since deltas archive away from the design. Review-finding numbers and dates like "19-07" stay out of specs (no findings doc exists in the tree) — inline the substance instead.
- **No undefined abbreviations or notation in prose.** Don't invent shorthand or mathematical notation (`F`, `|F|`, `O(|F|)`, `A_P`, …) — spell it out in plain words, or define the term on first use. Established names (UWill, ClaimId, pdn-store) are fine; gratuitous jargon (e.g. "seam") is not. A colleague should read it without decoding anything.
- **Markdown paragraphs are single-line.** Write each paragraph as one physical line (no hard wrapping); the renderer wraps it. Lists and headings stay one item per line.
- **Numbers: digits with thousands separators** (`10,000,000 records`, `1,000 peers`), not words — applies to every number in the docs, so the text scans at a glance.
- **Prototype generations: never bare «v3».** The pre-pivot prototype (`mee-v3-single-device/`: iroh-willow stack, `mee-*` names) is called **v3-single-device** everywhere — mia-docs, mee-noremote, chat. The current rebuild is informally **v3-multi-device** (calling it v4 was not approved), so an unqualified "v3" is ambiguous between the two.

## OpenSpec (mia-docs)

- **`openspec validate --all --strict` before archiving a change.** It is a parser gate for change deltas, not a review: it checks that delta files parse (`## ADDED/MODIFIED/REMOVED/RENAMED Requirements` sections) and that every requirement keeps at least one `#### Scenario:` block. It does not check content — SHALL wording, WHEN/THEN completeness, or truthfulness.
- **Scenario headers must be exactly `#### Scenario:` (four hashes).** A wrong heading level drops the scenario from parsing _silently_ — validation still passes if the requirement retains another scenario. Heading levels are checked by eye, not by the tool.
- **The main spec tree is not validated at all.** `openspec validate --specs` finds nothing in our layout (`openspec/specs/components/**`, `architecture/**` — the tool expects flat `specs/<name>/spec.md`). Everything below is therefore maintained by hand:
  - **Sweep the whole tree when a change removes or renames things.** A change's own deltas are not enough — grep `openspec/specs/**` for names of removed types/tests/mechanisms (precedent: multi-identity-node removed `Binding`/`BindingIndex`, and `pdn-node/namespace-addressing.md` kept naming them until noticed by accident).
  - **Sweep other active changes too.** A change landing first can invalidate assumptions in a sibling change's proposal/design (precedent: subset-rbsr referenced the removed ingest gate, a deleted test, and one-identity phrasing in D4 long after multi-identity-node landed).
  - **Keep `architecture/language/` in sync.** A term used by specs (e.g. swarm) gets a glossary entry there, and the spec links the term's first use to it.
  - **Verify spec scenarios against running code before writing them down.** A plausible formulation once claimed a zero-length write degenerates into deletion; the engine actually rejects it (`AttemptedToInsertEmptyEntry`). Each scenario should correspond to a test or a checked property of the fork.
