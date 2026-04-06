# Mee Personal Data Network

Mee PDN is a decentralized, local‑first data platform focused on privacy and user sovereignty.

This repository is for version 3, previous version repos:

* [version 1 (mee-core)](https://github.com/MeeFoundation/mee-core)
* [version 2 (mdn-repos)](https://github.com/MeeFoundation/mdn-repos)

## Getting Started

### First-time setup


### Devcontainer setup

To work with this project in a devcontainer, see [.devcontainer/README.md](.devcontainer/README.md) for token configuration instructions.

### Development

* `just`: full list of recipes
* `just build`: build all workspace crates
* `just test`: run tests for all workspace crates
* `just check`: format & lint check
* `just precommit-fix`: format & lint check + autofix, tests

## Crates

Core types and APIs

* `crates/mee-types`: Shared domain types
* `crates/mee-identity-api`: Identity provider/resolver traits
* `crates/mee-sync-api`: Backend-agnostic sync traits
* `crates/mee-node-api`: Node composition trait + service traits

Implementations & demos

* `crates/mee-identity-keri`: KERI-shaped identity manager (placeholder)
* `crates/mee-sync-iroh-willow`: Willow/iroh P2P sync engine
* `crates/mee-node-demo-impl`: Wires implementations into `DemoNode`
* `crates/mee-demo`: Axum API over demo node
* `crates/mee-wasm`: Minimal wasm-bindgen facade for wasm builds
