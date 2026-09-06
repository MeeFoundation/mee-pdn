# crates/pdn-types

Platform primitives (`define_byte_id!`, `PdnId`, `PdnIdentityProof`, `Aid`, `OperationalKey`, `ClaimId`, `NodeId`, `NonEmpty<T>`) plus the data vocabulary (`EntryPath`, `EntryInfo`, `NamespaceRole`, `NodeAddr`). No namespace type lives here: a pdn-store namespace is a `data-layer` internal (ADR-0009), and an entry is addressed by issuer `PdnId` and `EntryPath`.

Both `pdn-layer` and `data-layer` see only this crate — it is what keeps them independent of each other.
