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

## Crates

* `crates/mee-core`: Node composition layer
* `crates/mee-dev`: Developer CLI for node actions
* `crates/mee-transport-api`: Transport types and traits
* `crates/mee-transport-http`: HTTP transport backend
* `crates/mee-did-api`: DID types and traits
* `crates/mee-did-key`: Placeholder `did:key` provider
* `crates/mee-local-store-api`: Namespaced KV store types and traits
* `crates/mee-local-store-mem`: In-memory KV store backend
