# crates/pdn-types

Platform primitives (`define_byte_id_32!`, `define_byte_id_16!`, `PdnId`, `PdnIdentityProof`, `Aid`, `OperationalKey`, `ClaimId`, `NodeId`, `PodId`, `NonEmpty<T>`) plus the data vocabulary (`EntryPath`, `EntryInfo`, `NamespaceRole`, `NodeAddr`) and a pod's record vocabulary (`RecordKind`, `RecordId`, `RecordRef`). No namespace type lives here: a pdn-store namespace is a `data-layer` internal (ADR-0009), and an entry is addressed by issuer `PdnId` and `EntryPath`. No cryptography either: the ids a key derives — a `PdnId`, a pod id — are derived in `data-layer`'s `announcement` module, beside the key pair and its signatures.

Both `pdn-layer` and `data-layer` see only this crate — it is what keeps them independent of each other.
