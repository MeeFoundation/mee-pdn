# Mee Personal Data Network

Mee PDN is a decentralized, local‑first data platform focused on privacy and user sovereignty.

> [!WARNING]
> **Early development.** Privacy and user sovereignty are the point of this project, but the mechanisms meant to enforce them — access control, capability delegation, identity — are still being built, and none of them is hardened or recommended for production use. Formats, interfaces, and stored state change without notice. Please treat this as a research and development codebase: do not put real personal data into it, and do not rely on it to protect anything that matters to you.

This repository is the current rebuild, informally **v3-multi-device**. Previous generations:

- [version 1 (mee-core)](https://github.com/MeeFoundation/mee-core)
- [version 2 (mdn-repos)](https://github.com/MeeFoundation/mdn-repos)
- [v3-single-device](https://github.com/MeeFoundation/mee-pdn/tree/single-device) — the pre-pivot prototype (iroh-willow stack), on this repository's `single-device` branch

## First-time setup

Prerequisites on the host: Docker, and an editor with devcontainer support (VS Code or Zed). If you want the container's `claude` command, install [Claude Code](https://claude.com/claude-code) on the host as well — its OAuth token is minted there with `claude setup-token`.

### Devcontainer setup

To work with this project in a devcontainer, see [.devcontainer/README.md](.devcontainer/README.md) for token configuration instructions.

### `mia-docs` setup

Clone it into the repository root — specs and code practices are referenced as `mia-docs/...` from there, and `.gitignore` keeps the checkout out of this repository:

```sh
# From the repository root
git clone git@github.com:MeeFoundation/mia-docs.git
```

### `pdn-store` setup

`pdn-store` is our iroh-docs fork, pulled as a git dependency. Substantive changes almost always end up touching it, so you need the local checkout to patch the workspace against. Clone it into the repository root — `.gitignore` keeps it out of this repository, and the patch resolves it as `./pdn-store`:

```sh
# From the repository root
git clone git@github.com:MeeFoundation/pdn-store.git
```

Builds work as-is; enable the `[patch]` block next to the `pdn-store` dependency in the workspace [`Cargo.toml`](Cargo.toml) only when you are changing `pdn-store` and debugging it alongside `pdn-node`.

## Development

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
