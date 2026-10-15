# crates/pdn-types

Platform primitives (`define_byte_id_32!`, `define_byte_id_16!`, `PdnId`, `PdnIdentityProof`, `OperationalKey`, `ClaimId`, `NodeId`, `PodId`) plus a pod's record vocabulary (`RecordKind`, `RecordId`, `RecordRef`). No namespace type lives here: a pdn-store namespace is a `data-layer` internal, and a record is addressed by its pod's id and its `RecordRef`. No cryptography either: the ids a key derives — a `PdnId`, a pod id — are derived in `data-layer`'s `identity_key` module, beside the key pair and its signatures.

Both `pdn-layer` and `data-layer` see only this crate — it is what keeps them independent of each other.
