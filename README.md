# Mee Personal Data Network

Mee PDN is a decentralized, local‑first data platform focused on privacy and user sovereignty.

This repository is for version 3, previous version repos:

- [version 1 (mee-core)](https://github.com/MeeFoundation/mee-core)
- [version 2 (mdn-repos)](https://github.com/MeeFoundation/mdn-repos)

## Getting Started

### First-time setup

### Devcontainer setup

To work with this project in a devcontainer, see [.devcontainer/README.md](.devcontainer/README.md) for token configuration instructions.

### `mia-docs` setup

```sh
# Fetch mia-docs
git clone git@github.com:MeeFoundation/mia-docs.git
```

### `pdn-store` setup

```sh
# Fetch pdn-store
git clone git@github.com:MeeFoundation/pdn-store.git
```

### Development

- `just`: full list of recipes
- `just build`: build all workspace crates
- `just test`: run tests for all workspace crates
- `just check`: format & lint check
- `just fix`: format & lint check + autofix, tests

## Crates

Layers: `pdn-layer` (domain) / `data-layer` (sync) / iroh (bytes on the wire). Both layers see only `pdn-types`; `pdn-node` is the embeddable runtime built over them.

- `crates/pdn-types`: platform primitives (`PdnId`, `Aid`, `OperationalKey`, `ClaimId`, `NodeId`, …) plus the data vocabulary (`NamespaceId`, `EntryPath`, `EntryInfo`, `NamespaceRole`)
- `crates/data-layer`: the data layer over the forked iroh-docs (`pdn-store`) — the entries-only `DataLayer` trait, node/stack assembly, and the metadata stores
- `crates/pdn-layer`: the platform surface products consume — domain model (`Claim`, `Attribute`, `Capability`, `Connection`, `Invite`), the `PdnOp` operation AST, and the `uwill` capability-token module
- `crates/pdn-node`: the embeddable runtime core — identity / connections / data / sync services, plus the pairing and device-linking ceremonies
- `crates/pdn-node-http`: thin HTTP host for the demo stand — an axum binary embedding one runtime
- `crates/test-utils`: shared test helpers
