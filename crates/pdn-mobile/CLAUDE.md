# crates/pdn-mobile

The mobile host: a uniffi facade over the runtime, and the second host of the same shape as `pdn-node-http`. One exported call per service call, no orchestration of its own, `pdn-node` as its only dependency — no `data-layer`.

One handle owns one node. Bring-up names the directory the node's state lives in (the embedder knows its sandbox; nothing here derives a directory or reads one from the environment) and the reconcile interval (a short one costs radio wakeups, so the number belongs to a host that knows what it is running). A stop is safe to repeat, a second bring-up of the same handle is refused rather than replacing its node, and a bring-up on a directory another running node holds is refused as that. The constraint is on the handle, not the process: the surface tests hold 3 handles against 3 runtimes in one binary.

The facade owns the asynchronous runtime the operations need, and every exported call is awaited rather than blocking the caller's thread. No lock is held across an await, and a handle released without a stop shuts its tasks down in the background instead of blocking the thread that let it go.

**Deliberately absent, and to stay absent:** any namespace ticket in either direction (a grant read hands back the capability alone), any share or import of a namespace, anything that forces a reconciliation or resets state, and the runtime's test-only surface — the crate builds with that feature off, so a forced write is absent from the binary rather than unexported. Repeating a read is the whole of waiting. The facade authorizes nothing: reaching it is reaching the node.

The error table is this crate's own and names more kinds than `pdn-node-http`'s does — an unreachable counterparty, a dialogue timeout, a catch-up timeout and an unspoken payload version are distinct here, because a person acts differently on each and a container test needed only "refused against broken". The divergence is stated in `src/error.rs` beside the table. What the runtime does not separate the table does not either.

A grant names claim identities derived one-way from the issuer and an entry path. Publication takes paths and derives; a grant read reports derived identities; `claim_id` exports the derivation so a caller can join a read against paths it knows or has listed. This is the one place the surface breaks "one exported call, one service call", and it is stated rather than hidden.

A ceremony payload crosses in the runtime's own serde encoding, wrapped in nothing: the textual form a screen draws as a code is that encoding in a base64 alphabet, so a payload minted here is consumed by any host over the same runtime — `tests/surface.rs` asserts the round trip through the encoding `pdn-node-http` takes. The facade names no field of a payload, which is why a code read for the wrong act reaches the runtime and comes back as its refusal.

One entry payload is bounded (`MAX_ENTRY_PAYLOAD`, 1 MiB): a payload crosses as one buffer in memory and a phone is killed for memory rather than asked to swap. Memory pressure is the one operating condition a phone adds that a container never had. The bound is on what this host writes, not on what a read hands back — an entry a peer wrote through a host with a 64 MB ceiling is in the replica either way, and refusing to hand it over would make a granted claim unreadable instead; a caller that cares reads the length a listing reports first.

Bring-up and stop are the 2 calls with a race between them, and both hazards are handled where the state lives rather than in the caller: the node is installed by the spawn's own task, so a cancelled call cannot strand the handle mid-transition, and a stop that overtakes a bring-up shuts the new node down and leaves the bring-up reporting the node as not up. `tests/surface.rs` holds both. A handle dropped without a stop tears down on a thread of its own, because the store's handle joins its actor thread as it is dropped.

`src/bin/uniffi-bindgen.rs` sits behind the `cli` feature and is what `pdn-sdk`'s packaging recipes run; nothing a device installs contains it.

A stress pass names its own width: each surface scenario spawns 3 nodes, so the runner's default parallelism puts tens of them in flight and the runtime's ceremony bounds are what expire first — `just stress -p pdn-mobile --stress-count 50 --test-threads 3` is green where the default width fails on starvation.

Tests: `tests/surface.rs` drives the surface through exported calls alone — every positive beside the tightest denial the surface can express (an identity connected to neither side obtains nothing). The second read negative `access-control-tests` names, a party holding the replica's ticket and no capability, cannot be staged here at all, because no ticket crosses the facade; it stays with the runtime's own suites. Requirements live in `mia-docs`: `mobile-common-host-surface` for this surface, `sdk-artifacts` for what `pdn-sdk` packages out of it.
