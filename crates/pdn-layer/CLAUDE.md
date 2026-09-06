# crates/pdn-layer

The platform surface products consume: domain model (`Claim`, `Attribute`, `Capability`, `Connection`, `Invite`), the `PdnOp` operation AST, and the `uwill` module (the capability-token types alone — nothing validates, issues, or revokes a token; enforcement runs on data-layer's grant vocabulary). No iroh dependencies. Nothing in the workspace consumes this crate yet; it is the draft of the surface the runtime grows toward.

This crate does NOT depend on `data-layer` — both see only `pdn-types`, and the `pdn-node` runtime glues them together.
