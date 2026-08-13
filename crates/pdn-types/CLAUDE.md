# crates/pdn-types

Platform primitives (`define_byte_id!`, `PdnId`, `PdnIdentityProof`, `Aid`, `OperationalKey`, `ClaimId`, `NodeId`, `NonEmpty<T>`) plus the data vocabulary (`NamespaceId` = `(about, issued_by)`, `EntryPath`, `EntryInfo`, `NamespaceRole`, `NodeAddr`).

Both `pdn-layer` and `data-layer` see only this crate — it is what keeps them independent of each other.
