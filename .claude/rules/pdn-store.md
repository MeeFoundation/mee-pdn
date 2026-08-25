---
description: Pre-push checklist required after any modification of the store crate
paths:
  - "crates/pdn-store/**"
---

# pdn-store checks

Any modification of `crates/pdn-store` ends with the checks the pipeline's `store` job runs, all from the workspace root: `just check`, `just check-store` (clippy on the other two feature sets and rustdoc, warnings denied, then the wasm32 build), `just test`, and `just test-store` (the other two feature sets, then doctests). The crate's `CLAUDE.md`, section "Pre-push checklist", says what each covers.
