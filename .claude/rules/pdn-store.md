---
description: Pre-push checklist required after any modification of the nested pdn-store checkout
paths:
  - "pdn-store/**"
---

# pdn-store checks

Any modification of `./pdn-store` ends with running its pre-push checklist (`./pdn-store/CLAUDE.md`, section "Pre-push checklist") before committing: fmt, clippy for all three feature sets, `cargo +nightly docs-rs`, `cargo deny`, tests plus doctests, and the wasm build — all with warnings treated as errors, exactly as that repo's CI runs them.
