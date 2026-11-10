//! The [CESR] 2.00 primitives a subset of [KERI] reads, in the text domain: keys, digests, plain
//! and indexed signatures, numbers, count codes and the genus-version code, read and written as
//! keripy reads and writes them, and signatures verified. No I/O and no secret key. Spec:
//! `mia-docs/openspec/specs/components/mee-pdn/kerita-cesr/`.
//!
//! [CESR]: https://trustoverip.github.io/kswg-cesr-specification/
//! [KERI]: https://trustoverip.github.io/kswg-keri-specification/

extern crate alloc;

// A fork kept close to upstream (UPSTREAM.md): what the subset does not use stays, unused, the
// style lints upstream configures otherwise are not imposed on its code, and its docs keep links
// to upstream's other crates.
#[allow(
    rustdoc::broken_intra_doc_links,
    rustdoc::redundant_explicit_links,
    dead_code,
    unused_imports,
    clippy::indexing_slicing,
    clippy::trivially_copy_pass_by_ref,
    clippy::wrong_self_convention,
    clippy::unused_self,
    clippy::str_to_string,
    clippy::chunks_exact_to_as_chunks,
    clippy::enum_variant_names,
    clippy::assert_is_empty
)]
mod cesr;
pub mod count;
pub mod number;
pub mod primitive;
pub mod signature;
mod subset;
