# crates/pdn-node

The embeddable runtime core: identity / connections / data / sync services as thin glue over `data-layer`, plus the runtime's two ceremonies.

Pairing (ADR-0011): connections are produced by establishment — `invite` mints a bearer-free payload carrying a one-time secret (no ticket, no identity proof — nothing grants durable access; the secret burns on first use), `establish` dials the pairing ALPN and runs the verify-and-burn dialogue, and the grant surface publishes/reads whole-store tickets over the connection's metadata pair (no manual recording).

Device linking (ADR-0012, the `linking` module next to `pairing`): `linking_invite` mints the same kind of bearer-free payload (one-time secret inside, nothing durable), `link` dials the linking ALPN — the inviter verifies-and-burns, registers the newcomer's device record itself (from the connection's authenticated peer id), and replies with fresh directory and data-namespace write tickets; `link` imports both and returns caught up, rolling both back on failure.

Each `Runtime` is one running node hosting any number of identities, each added by an explicit act (create, or link from a payload); the runtime is the single owner of node assembly — both protocol handlers thread through `spawn` into data-layer's protocol slot — and of the hosted identities' store handles. Services are traits with one production implementation (`IdentityService` — a KERI-backed second implementation is the live prospect; the current one mints placeholder `PdnId`s). Whole-store ticket share/import in the data service is the interim access model for cross-identity namespaces, replaced when capability-scoped sharing lands.

Scenario tests in `tests/`. No host or HTTP dependencies.
