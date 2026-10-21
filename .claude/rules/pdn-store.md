---
description: Pre-push checklist required after any modification of the store crate
paths:
  - "crates/pdn-store/**"
---

# pdn-store checks

Any modification of `crates/pdn-store` ends with `just check` (the store's warnings and rustdoc's denied) and `just test`, from the workspace root. The crate's `CLAUDE.md`, section "Pre-push checklist", says what each covers.
