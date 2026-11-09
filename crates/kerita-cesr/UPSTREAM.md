# Upstream of `src/cesr/`

`src/cesr/` is a fork of `cesr-rs`, the crate `crates/cesr` of [devrandom-labs/cesr](https://github.com/devrandom-labs/cesr), licensed MIT OR Apache-2.0 and taken here under Apache-2.0 (`NOTICE`). It was taken at commit `3a02703ab892325b149bf4a477c5209191f12386` (4 October 2026).

## How it was taken

- `crates/cesr/src/` was copied whole; its `lib.rs` became `cesr/mod.rs`.
- Paths `crate::` read `crate::cesr::`. The feature gates `alloc`, `core`, `crypto`, `b64` and `std` are gone, every one being on; `test-utils` reads `cfg(test)`. `#![no_std]`, `#![cfg_attr(docsrs, …)]` and the crate's `extern crate` lines are gone, `alloc` being declared at the crate root.
- The primitive vectors of the corpus passed on the code so taken, with upstream's dependencies, before anything below was changed.

## Divergences

Every change to the fork after it was taken, so that a later look at upstream can tell ours from theirs.

### The workspace's RustCrypto, and no secret key (task 4.2)

- `crypto/verify.rs` verifies Ed25519 with `ed25519-dalek` 3 and P-256 with `p256` 0.14. A P-256 signature is normalized to its low S value before it is verified, so both S values verify, as keripy verifies them.
- `crypto/digest.rs` computes Blake3-256 alone; any other digest code answers `DigestError::Unsupported`.
- Gone with their tests: `crypto/keypair.rs` and `crypto/salt.rs` with `crypto/salt/` (key generation, signing, seed stretching, mnemonics: secret keys the crate never holds), `Secp256k1` and `verify_secp256k1`, and the errors `KeyError`, `SaltError` and `MnemonicError`. Upstream's tests of signing and verification signed with the dropped key pairs; `crypto/verify.rs` carries its own, signing with fixed seeds.
- Dependencies dropped: `argon2`, `blake2`, `digest`, `getrandom`, `k256`, `rand_core`, `sha2`, `sha3`, `signature`, `zeroize`.

### The subset (task 4.3)

- The fork is a private module; the crate reads primitives through `src/primitive.rs`, which answers a code outside `src/subset.rs` as unsupported. The code tables keep every upstream code, so the fork stays close to upstream; nothing outside the subset is read, and no public item holds a seed.
- `core/indexer/builder.rs` refuses an indexed signature whose pad bits are not zero (`IndexerParseError::NonZeroPadBits`), as keripy does: upstream read `AA_Z0jw…` and `AACZ0jw…` as one signature.
- `core/matter/builder.rs` decodes a primitive's payload without Base64 padding: upstream's padded engine read `MAA=AA` as a short number of the wrong length, which writing back turned into a panic (found by kerita's fuzz targets, `crates/kerita/fuzz/`, and kept as `crates/kerita/tests/corpus/fuzz/primitive/crash-*`).
