# crates/kerita-cesr

The [CESR](https://trustoverip.github.io/kswg-cesr-specification/) 2.00 primitives a subset of [KERI](https://trustoverip.github.io/kswg-keri-specification/) reads, in the text domain — keys, digests, plain and indexed signatures, numbers, tags, Base64 strings, count codes and the genus-version code — read and written as keripy reads and writes them, and signatures verified. CESR alone: an event, a message, key state and every rule of KERI are `crates/kerita`'s. A fork of `cesr-rs` under the private module `src/cesr/` does the encoding; `src/primitive.rs` is its facade and answers a code outside `src/subset.rs` as unsupported (`UPSTREAM.md` lists every divergence from upstream). Its corpus is written by kerita's oracle and its entry point fuzzed by kerita's fuzz targets. Specs: `components/mee-pdn/kerita-cesr/` (`crate`, `primitives`) in the mia-docs spec tree.

## Clean room

A rule for every agent and model working in this crate, from the first file read on:

- **Agents and models never open, fetch, search or quote the source, tests or docs of keriox (THCLab: keri-core, keri-controller, keri-sdk, cesrox, teliox) or of any project whose license is not on the allow list of `deny.toml`.** A project's license is read from its metadata before any of its files is opened.
- **keripy (Apache-2.0) is the reference.** Its test cases are copied verbatim, each with its source file and keripy commit recorded, and its logic may be followed closely.

## Code

- **No I/O, no clock, no randomness and no secret key in the product build.** A signature is verified, never made.
- **Unsupported is a refusal of its own.** A code outside the subset is answered as unsupported, never as malformed, and nothing outside the subset is read.
