# crates/pdn-layer

The platform surface products consume: the domain model (`Claim`, `Attribute`) and the `PdnOp` operation AST. No iroh dependencies. Nothing in the workspace consumes this crate yet; it is the draft of the surface the runtime grows toward.

This crate does NOT depend on `data-layer` — both see only `pdn-types`, and the `pdn-node` runtime glues them together.
