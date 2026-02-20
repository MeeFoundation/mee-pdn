# Mee Personal Data Network

Mee PDN is a decentralized, local‑first data platform focused on privacy and user sovereignty.

This repository is for version 3, previous version repos:

* [version 1 (mee-core)](https://github.com/MeeFoundation/mee-core)
* [version 2 (mdn-repos)](https://github.com/MeeFoundation/mdn-repos)

## Getting Started

### First-time setup

* install [rust](https://www.rust-lang.org/)
* install [just](https://github.com/casey/just)
* setup tooling - `just setup-tooling`

### Development

* `just`: full list of recipes
* `just build`: build all workspace crates
* `just test`: run tests for all workspace crates
* `just check`: format & lint check
* `just precommit-fix`: format & lint check + autofix, tests

### Demo (P2P over Willow)

* `just p2p-demo`
* Flow: Bob exposes a Willow user id; Alice creates a share ticket for Bob; Bob imports the ticket and starts sync; Alice inserts an entry; Bob lists entries replicated via P2P.

## Crates

Core types and APIs

* `crates/mee-types`: Shared domain types
* `crates/mee-did-api`: DID
* `crates/mee-local-store-api`: Namespaced KV
* `crates/mee-node-api`: Minimal PDN node composition crate

Implementations & demos

* `crates/mee-did-key`: Stub `did:key` manager (demo)
* `crates/mee-local-store-mem`: In-memory KV store backend (demo)
* `crates/mee-sync-api`: backend-agnostic sync API used by the demo
* `crates/mee-sync-iroh-willow`: willow/iroh backend for the sync API
* `crates/mee-wasm`: Minimal wasm-bindgen facade for wasm builds
* `crates/mee-demo`: axum node exposing P2P sync endpoints (demo)
