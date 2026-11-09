# kerita

A subset of [KERI](https://trustoverip.github.io/kswg-keri-specification/) in Rust: KERI 2.00 key events in the native CESR body, in the text domain, read, written and folded into key state.

- **What it reads:** the events `icp`, `rot`, `ixn`, `dip`, `drt` and the receipt `rct`; Ed25519 and P-256 keys and signatures, plain, indexed and dual-indexed; Blake3-256 digests; count and weighted thresholds; digest seals and key event seals; the configuration trait `EO`. Everything else is refused as unsupported.
- **What it decides:** whether an event advances an AID's key state, or what it waits for — the prior event, signatures, witness receipts or its delegator's approval — answered as keripy, the reference implementation, answers it.
- **What it does not do:** no I/O, no storage, no clock, no randomness and no secret key. The caller keeps the events, and signs the bytes the crate hands it.
- **How it is held to keripy:** a pinned keripy in a container produces a committed corpus of vectors from keripy's own tests, and the crate's own KELs are folded in keripy; both run every night, and every entry point is fuzzed.

The [CESR](https://trustoverip.github.io/kswg-cesr-specification/) primitives are in [`kerita-cesr`](../kerita-cesr/), re-exported here.

**The name:** *-ita* is the Spanish diminutive suffix, so Kerita is "little KERI".

Licensed under the Apache License, Version 2.0; `NOTICE` names the material taken from keripy and cesr-rs. Not published yet.
