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

### Demo (HTTP)

Run two local nodes (Alice/Bob), send a ping, and view the inbox.

* `just http-demo`

## Crates

Core types and APIs

* `crates/mee-types`: Shared domain types
* `crates/mee-did-api`: DID
* `crates/mee-transport-api`: Transport
* `crates/mee-local-store-api`: Namespaced KV
* `crates/mee-node-api`: Minimal PDN node composition crate

Implementations & demos

* `crates/mee-did-key`: Stub `did:key` manager (demo)
* `crates/mee-transport-http`: HTTP transport backend (demo)
* `crates/mee-local-store-mem`: In-memory KV store backend (demo)
* `crates/mee-node-axum`: Axum-based demo node
* `crates/mee-wasm`: Minimal wasm-bindgen facade for wasm builds
* `crates/mee-dev`: Developer tools, CLI
