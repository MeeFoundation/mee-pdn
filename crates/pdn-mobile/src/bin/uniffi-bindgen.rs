//! The binding generator, behind the `cli` feature.
//!
//! `pdn-sdk`'s packaging recipes run this against the built library and
//! turn what it emits into the artifacts a device installs. Nothing a
//! device installs contains it.

fn main() {
    uniffi::uniffi_bindgen_main();
}
