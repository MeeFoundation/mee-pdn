# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Directory layout

`mia-docs/` is a sibling repo cloned in-place at the top of the workspace (gitignored) — UWill ADRs, openspec specs.

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

Each crate carries its own `CLAUDE.md` with its contracts and what is deliberately absent from it — read that file when working inside the crate.

- [`crates/pdn-types`](crates/pdn-types/) — platform primitives and the data vocabulary. The only crate `pdn-layer` and `data-layer` share.
- [`crates/pdn-store`](crates/pdn-store/) — our iroh-docs variant, the document sync engine under `data-layer`, diverged from upstream where PDN's access model needs it. The package keeps the upstream name `iroh-docs` (`-p iroh-docs`); consumers alias it `pdn-store`.
- [`crates/data-layer`](crates/data-layer/) — the data layer over `crates/pdn-store`: the entries-only `DataLayer` trait, `SyncNode` stack assembly, the metadata stores, the protocol-agnostic ceremony slot.
- [`crates/pdn-layer`](crates/pdn-layer/) — the platform surface products consume: domain model, the `PdnOp` operation AST, the `uwill` module. No iroh dependencies.
- [`crates/pdn-node`](crates/pdn-node/) — the embeddable runtime core: identity / connections / data / sync services over `data-layer`, plus the pairing (ADR-0011) and linking (ADR-0012) ceremonies. No host or HTTP dependencies.
- [`crates/pdn-node-http`](crates/pdn-node-http/) — the thin HTTP host for the demo stand: an axum binary embedding one runtime, with the `/debug/` subtree behind `PDN_DEBUG=1`.

## Commands

Task runner is [just](https://github.com/casey/just) — `just --list` prints every recipe with its doc comment.

Every test of the HTTP surface is a container test. They carry `#[ignore]`, so
`just test` on a machine without a daemon or an image stays green and reports
them skipped; `just test-docker` and the pipeline's own job run them with
`--run-ignored all`. A test group in `.config/nextest.toml` bounds their
parallelism: the daemon holds a fixed share of the machine, and the runner's
default width would saturate it.

Tests run under [cargo-nextest](https://nexte.st) (process-per-test, `--test-threads` defaults to CPU cores). It is a **required** tool: `just setup-tooling` installs it locally, CI installs it via `taiki-e/install-action`, and the devcontainer bakes it into the image (`.devcontainer/Dockerfile.app`). `just test`/`just stress` error out with a hint if it is missing.

Run a single crate's tests: `just test -p <crate-name>`.
Run a single test: `just test -E 'test(<test_name>)'`.

Go through `just`, not bare `cargo nextest run`: the recipes enable `pdn-node/test-util`, the feature the write-retraction scenarios sit behind, and a bare cargo invocation matches zero of them without saying so. The recipes drop the flag when the caller narrows the package selection away from `pdn-node`, because cargo rejects a feature of an unselected package.

`crates/pdn-store` carries feature sets and a wasm target the workspace build never compiles: `just check-store` lints them and `just test-store` tests them, and the pipeline's `store` job runs both. Under `just test` the store's tests run like any crate's, and a run with no selection ends with the workspace doctests — the store's README is one, and nextest runs no doctests. Its three tests marked `#[ignore = "flaky"]` run only in the nightly workflow.

## Lint rules

Strict safety-first linting, configured in the workspace `Cargo.toml`, `clippy.toml` and `rustfmt.toml`, enforced by `just check`. Prefer `.get()` and `TryFrom`/`TryInto` over indexing and `as`.

## Code practices

Cross-cutting practices live in `mia-docs/openspec/specs/code-practices/`:

- [`operating-conditions.md`](mia-docs/openspec/specs/code-practices/operating-conditions.md) — the circumstances a design has to survive: several identities on one node, one device or several, a device linking before or during or after a process, an unstable connection, a disk that fills, a device that restarts (durable state outlives the process, in-memory bookkeeping does not, and the world keeps moving meanwhile), capabilities granted and narrowed and widened and revoked and granted again. Walk the list while designing; back with a spec scenario and a test the paths where a condition changes the outcome; name the ones deliberately left out. Not an instruction to multiply every test by every condition — that product is unaffordable and mostly vacuous.
- [`access-control-tests.md`](mia-docs/openspec/specs/code-practices/access-control-tests.md) — every test that asserts authorized access must, in the same place, assert the tightest unauthorized party is denied (read: an outsider, and a holder of the store's ticket but no read capability; write: a lower-level holder). A positive-only access test verifies nothing.
- [`product-path-arrangement.md`](mia-docs/openspec/specs/code-practices/product-path-arrangement.md) — arrange and act steps reach a namespace the way the product reaches it (grant published, binder imports); a hand-made ticket handover is admissible only as the test's subject or as the access-control negative control, named in the test's docs. Every rewrite onto the product path must fail with the mechanism deliberately broken. `sibling_serving.rs` is the worked example.
- [`flaky-tests.md`](mia-docs/openspec/specs/code-practices/flaky-tests.md) — every substantial change ends with a flaky-test stress pass, before anything is built on top. After landing a change that touches sync, linking, engine wiring, `crates/pdn-store`, or bumps iroh, stress the affected scenario tests under nextest (`--stress-count`) and treat any failure as a defect of that change, diagnosed in isolation from other work. Full discipline — reproduction sizing (hundreds of runs, rule of three), fix minimization, deterministic pinning — in the spec. This exists so we never again build a feature first and then debug the previous implementation's flaky tests through it.

## Git

All git operations that change state — `commit`, `push`, `checkout`, `add`, `rebase`, etc. — are performed **by a human, never by Claude**. Read-only commands (`status`, `diff`, `log`, `show`) are fine.

## Code comments

- **Maximally brief, critical-only.** A comment earns its place by flagging an invariant, a contract edge, or a non-obvious why. Narrative, alternatives, and process history go.
- **Present tense only — never the code's or system's past or future.** No "legacy", "interim", "for now", "used to", "no longer", "until X lands", "arrives with UWill", "future work"; no review-finding references ("finding 8", "#4"), no PR numbers. Rewrite as present state or a present conditional ("without the guard the import would …"). ADR-XXXX and Invariant N references are fine; Dn references are not — those live in the change's design doc, not in code. Applies to every crate, `crates/pdn-store` included.

## Docs

- **Each paragraph stands alone — the read-aloud test.** Understanding a paragraph must not require jumping to another text. The prose itself has to survive being read aloud: English is not the first language of every maintainer, and many sound the text out in their head while re-reading, so clumsy phrasing cuts reading speed several times over. Allowed bare references: ADR numbers, Invariant numbers, and D1..Dn where it is unambiguous which spec's decisions are meant — inside the change's own bundle (design/proposal/tasks) bare Dn is fine; in delta specs qualify it ("subset-rbsr D3"), since deltas archive away from the design. Review-finding numbers and dates like "19-07" stay out of specs (no findings doc exists in the tree) — inline the substance instead.
- **No undefined abbreviations or notation in prose.** Don't invent shorthand or mathematical notation (`F`, `|F|`, `O(|F|)`, `A_P`, …) — spell it out in plain words, or define the term on first use. Established names (UWill, ClaimId, pdn-store) are fine; gratuitous jargon (e.g. "seam") is not. A colleague should read it without decoding anything.
- **Platform terms keep their English names in every language.** Never translated, in docs or in chat: audience, author, binder, binding, book, capability, catch-up, claim, connection, connection metadata store (CMS), directory, entry, EntryPath, grant, host, hosted, identity, invite, key, linking, lock, memo, namespace, node, orphan, pairing, peer, private metadata store (PMS), race, registry, replica, retract, revoke, scope, session, snapshot, store, surface, swarm, sweep, sync, ticket, tombstone, withdraw. Spelling a term in the local alphabet is translating it too. Where the surrounding language inflects nouns, attach the ending after an apostrophe instead of reshaping the word, and never coin a derived adjective or verb from one of these terms: write "a namespace received under a grant", not "a granted namespace".
- **One term in the code, one word in the text.** Each term of that list keeps a single spelling throughout a document, and no two of them ever share one. Translation fails in both directions at once: it collapses terms the code separates (revoke, withdraw and retract; entry and write; the author of an entry and the person who wrote the document), and it splits a single term across several words, one of which usually already belongs to another term. Either way the prose stops matching the code it describes.
- **A mechanism is named by the word the code gives it.** Prose that describes an operation the code already names takes that name — `reclaim` for `reclaim_abandoned_sessions`, whose metric is `sync_sessions_reclaimed` — rather than a metaphor coined for the occasion. A coined word cannot be grepped, is defined nowhere, and drifts: one invented sweeping-metaphor ended up standing for three different things in one document — that reclaim pass, the removal of a namespace's sessions when its replica closes, and a sweep of stuck peer state that does not exist. An operation the code does not name is described in plain words, not given a new one.
- **Markdown paragraphs are single-line.** Write each paragraph as one physical line (no hard wrapping); the renderer wraps it. Lists and headings stay one item per line.
- **Numbers: digits with thousands separators** (`10,000,000 records`, `1,000 peers`), not words — applies to every number in the docs, so the text scans at a glance.
- **Prototype generations: never a bare number.** The pre-pivot prototype (`mee-v3-single-device/`: iroh-willow stack, `mee-*` names) is called **v3-single-device** everywhere — mia-docs, chat. The rebuild that grew out of it is **v3-multi-device**, frozen on the branch of that name, and the generation being built now is **v4-non-keri**: cells as the shared primitive, on identity that does not yet use KERI. An unqualified "v3" is ambiguous between the first two and an unqualified "v4" names no axis at all, so every mention carries the full name.

## Path-scoped rules

[`.claude/rules/`](.claude/rules/) holds guidance that loads only when the matching files are in play: `openspec.md` for the `mia-docs/` spec tree, `pdn-store.md` for `crates/pdn-store`.
