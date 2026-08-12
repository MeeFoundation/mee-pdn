# crates/pdn-layer

The platform surface products consume: domain model (`Claim`, `Attribute`, `Capability`, `Connection`, `Invite`), the `PdnOp` operation AST, and the `uwill` module (capability-token format, future chain validation). No iroh dependencies.

This crate does NOT depend on `data-layer` — both see only `pdn-types`, and the `pdn-node` runtime glues them together.
