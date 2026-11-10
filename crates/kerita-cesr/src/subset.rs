//! The CESR codes the crate reads; `oracle/corpus.py` lists the same codes, a count code in its
//! big form beside its small one, and a code missing from either list fails the corpus tests.

/// Keys, the digest and the plain signatures of the subset, then the codes a native body is built
/// from: numbers, tags and Base64 strings, the last carrying weighted thresholds.
pub(crate) const PRIMITIVES: &[&str] = &[
    "D", "B", "1AAJ", "1AAI", //
    "E",    //
    "0B", "0I", //
    "M", "0H", "R", "N", "S", "T", "0A", "U", //
    "0J", "0K", "X", "1AAF", "0L", "0M", "Y", "1AAN", "0N", "0O", "Z", //
    "4A", "5A", "6A", "7AAA", "8AAA", "9AAA",
];

/// Indexed Ed25519 and P-256 signatures, small and big, in both lists or the current one.
pub(crate) const INDEXED_SIGNATURES: &[&str] = &["A", "B", "E", "F", "2A", "2B", "2E", "2F"];

/// The CESR 2.00 count codes of a native body and of the attachments of the subset, in the small
/// form; a big one (`--K`) reads as its small one.
pub(crate) const COUNTERS: &[&str] = &["-B", "-C", "-F", "-J", "-K", "-L", "-M", "-Q", "-S", "-T"];

/// The genus-version code, which names the CESR version of what follows it in a stream.
pub(crate) const GENUS_VERSION: &str = "-_AAA";
