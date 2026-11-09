# crates/kerita

A subset of KERI: KERI 2.00 events in the native CESR body under genus 2.00, in the text domain — `icp`, `rot`, `ixn`, `dip`, `drt` and the receipt `rct` — read, written and folded into key state. The CESR primitives under it are `crates/kerita-cesr`'s, re-exported as `kerita::primitive` and `kerita::signature`. KERI alone: what ADR-0017 adds on top of it — the roles a role statement gives and the weight each carries, the limits on a KEL, the old witness's receipt on a change of witness, stamps, records — belongs to the crates that consume this one, `pdn-keri` among them, which read from the key state the slots that signed, the seals an event anchors and the witness list. Usable on its own, outside the platform, as a KERI subset: what the subset holds behaves as keripy does, what it does not is unsupported, and widening it is weighed by its cost. The oracle in `oracle/` writes the corpora of both crates, and the fuzz targets in `fuzz/` drive the entry points of both. Specs: `components/mee-pdn/kerita/` (`crate`, `encoding`, `key-state`, `conformance`) in the mia-docs spec tree.

## Clean room

A rule for every agent and model working in this crate, from the first file read on:

- **Agents and models never open, fetch, search or quote the source, tests or docs of keriox (THCLab: keri-core, keri-controller, keri-sdk, cesrox, teliox) or of any project whose license is not on the allow list of `deny.toml`.** A project's license is read from its metadata before any of its files is opened.
- **keripy (Apache-2.0) is the reference.** Its test cases are copied verbatim, each with its source file and keripy commit recorded, and its logic may be followed closely.

## Code

- **Idiomatic Rust, functional where it can be, on current libraries.** Key state is an immutable value, accepting an event is a function to the next key state or a verdict, and a KEL is folded with `try_fold`. Types cannot hold an invalid value. keripy's class structure is not carried over.
- **No I/O, no clock, no randomness and no secret key in the product build.** The crate hands a signer the bytes to sign and takes the signature back; an event it cannot validate yet comes back as a verdict naming what it waits for, and the caller keeps the event.
- **Unsupported is a refusal of its own.** An input outside the subset — another protocol version, body or algorithm, a trait other than `EO` — is answered as unsupported, never as malformed or invalid, and nothing outside the subset is accepted.
