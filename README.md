# Mee Personal Data Network

Mee PDN is a decentralized, local‑first data platform focused on privacy and user sovereignty.

This repository is for version 3, previous version repos:

- [version 1 (mee-core)](https://github.com/MeeFoundation/mee-core)
- [version 2 (mdn-repos)](https://github.com/MeeFoundation/mdn-repos)

## Getting Started

### First-time setup

### Devcontainer setup

To work with this project in a devcontainer, see [.devcontainer/README.md](.devcontainer/README.md) for token configuration instructions.

### Mia-docs setup

```sh
# Fetch mia-docs
git clone git@github.com:MeeFoundation/mia-docs.git
```

### Development

- `just`: full list of recipes
- `just build`: build all workspace crates
- `just test`: run tests for all workspace crates
- `just check`: format & lint check
- `just precommit-fix`: format & lint check + autofix, tests

## Crates

Core types and APIs

- `crates/mee-types`: Shared domain types (byte-backed IDs, `MeeId`, `Aid`, `OperationalKey`, …)
- `crates/mee-sync-api`: Backend-agnostic sync API (`NamespaceId`, `EntryPath`, `NamespaceKind`, …)

Draft operation ASTs (material for discussion)

- `crates/mee-pdn-layer`: PDN-layer operation AST (identity, connections, claims, delegation)
- `crates/mee-willow-layer`: Willow/iroh-layer operation AST (namespaces, entries, UWill capabilities)
