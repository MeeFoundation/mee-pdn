set dotenv-load
set positional-arguments

_default:
  @ just --list --unsorted

# Every nextest recipe's first step.
_need-nextest:
  @command -v cargo-nextest >/dev/null 2>&1 || { echo "cargo-nextest not found — run: just setup-tooling"; exit 1; }

# Build the entire workspace
build:
  #!/bin/sh
  set -eux
  cargo build --workspace

# Build workspace in release mode
build-release:
  #!/bin/sh
  set -eux
  cargo build --workspace --release

# Watch and rebuild on changes (requires cargo-watch)
build-watch:
  #!/bin/sh
  set -eux
  cargo watch -x 'build --workspace'

_check-devcontainer-tools:
  @sh scripts/check-devcontainer-tools.sh

# Install local developer tooling: the pinned toolchain (rust-toolchain.toml) and the tools tools.toml pins
setup-tooling:
  @sh scripts/setup-tooling.sh

# Refresh: `cd mia-docs && openspec update`, then this, then review the diff.
[doc("Derive the root's OpenSpec skills (.claude, .agents) from mia-docs' generated ones")]
openspec-skills:
  @python3 -I scripts/openspec-skills.py

# Run workspace tests via nextest — extra args forwarded; a run with no selection ends with the workspace doctests, a `-p` or `-E` selection skips them
test *args: _need-nextest
  @sh scripts/test.sh "$@"

# The store's other feature sets, beyond the default one the workspace
# builds: `--all-features` and `--no-default-features` compile different
# code — `fs-store` gates the file-backed store, `rpc` the network API — and
# the wasm build in `check-store` is the one consumer of the featureless one.
# Extra args are forwarded to nextest.
[doc("Test the store under its other feature sets, doctests included")]
test-store *args: _need-nextest
  #!/bin/sh
  set -eu
  cargo nextest run -p pdn-store --all-features "$@"
  cargo nextest run -p pdn-store --no-default-features "$@"
  cargo test -p pdn-store --all-features --doc

# Test in release mode via nextest — extra args forwarded (test nodes bind loopback — see data-layer node.rs)
test-release *args: _need-nextest
  #!/bin/sh
  set -eu
  export PDN_BIND_ADDR=127.0.0.1
  cargo nextest run --release $(sh scripts/test-features.sh "$@") "$@"

# The image the stand runs. The scenarios look for exactly this tag.
image := "pdn-node-http:dev"

# Build the stand's image from the workspace.
[doc("Build the stand's node image")]
build-image:
  #!/bin/sh
  set -eux
  DOCKER_BUILDKIT=1 docker build -f ops/Dockerfile -t {{ image }} .

# What the build context actually carries, listed against the allowed set in
# .dockerignore. The criterion is presence — anything outside that set is a
# leak whatever it weighs; the sizes say which leak is expensive.
[doc("List what the docker build context carries")]
check-context:
  #!/bin/sh
  set -eu
  DOCKER_BUILDKIT=1 docker build -q -f ops/Dockerfile.context -t pdn-context-check:dev . >/dev/null
  docker run --rm pdn-context-check:dev

# Run one node by hand: debug surface on, HTTP port published. PORT overrides
# the published port, BIND the interface it is published on.
#
# BIND defaults to loopback because the surface this publishes is
# unauthenticated and mints live ceremony secrets: the host binds every
# interface inside the container, and without an address here the daemon
# would carry that to every interface of the machine. A node reachable from
# another machine is asked for explicitly — `BIND=0.0.0.0 just run-image`.
[doc("Run one stand node in the foreground")]
run-image:
  #!/bin/sh
  set -eux
  PORT=${PORT:-3011}
  BIND=${BIND:-127.0.0.1}
  docker run --rm -e PDN_DEBUG=1 -p "${BIND}:${PORT}:3011" {{ image }}

# The live demo: several nodes on one network — Alice with two personas on a
# phone plus a laptop, Bob and Carol with a phone and a laptop each — driven
# through their debug surfaces while everything between them runs over the
# runtimes' own protocols.
[doc("Run the live demo across containers (needs docker)")]
demo:
  @sh scripts/run-demo.sh

# The identity of the image a run tests: the content id the daemon gave the
# build, rather than the tag that also names it. A tag is a name any build
# moves — a second worktree rebuilding it mid-run would otherwise put two
# revisions into one scenario — and an id cannot be moved. Prints nothing
# when no daemon answers or nothing is built, so a caller reads an empty
# answer rather than an error from a missing image.
[doc("Print the image id the stand's scenarios run against")]
stand-image:
  #!/bin/sh
  set -eu
  docker images --no-trunc --quiet {{ image }} 2>/dev/null | head -n 1

[doc("Print the nextest profile matching the daemon's CPU count")]
stand-profile:
  @sh scripts/stand-profile.sh

# The stand: build the image, then run the container scenarios against it.
# Extra args are forwarded to `cargo nextest run`.
#
# Deliberately outside `just test` and outside the flaky hunt's default
# selection, for two reasons that do not depend on how long the scenarios
# take: the image has to be built first, or a run tests whatever image is
# lying around, and the flaky hunt selects the integration binaries by
# default, which would put a container binary into every stress run. Needs a
# container daemon.
[doc("Build the image and run the container scenarios (needs docker)")]
test-docker *args: _need-nextest
  @sh scripts/test-docker.sh "$@"

# The container flaky hunt: the stand's suite repeated, with everything a
# failure needs kept and everything a clean run leaves behind thrown away.
#
# Extra args are forwarded to `cargo nextest run`. Needs a container daemon.
[doc("Hunt flaky container scenarios: repeat the stand's suite, keep what a failure needs")]
stress-docker count="100" *args: _need-nextest
  @sh scripts/stress-docker.sh "$@"

# Stress / flaky-hunt via nextest. All args are forwarded to `cargo nextest run`.
#
# With no test selection it defaults to the scenario (integration) tests,
# `-E 'kind(test)'` — the unit tests are deterministic, so stressing them is
# wasted. Pass your own `-E`/`--filter-expr` or `-p`/`--package` to override:
#
#   just stress --stress-count 300 -E 'binary(linking)'
#   just stress --stress-count 300 -p pdn-node
#
# `--retries N --flaky-result fail` handles a known-flaky test.
#
# On macOS a per-process node-startup cost serializes across processes, so
# parallel repeats gain little locally — `just hammer` amortizes it (a whole
# binary per process). See mia-docs flaky-tests.md.
[doc("Stress / flaky-hunt via nextest — all args forwarded to cargo nextest run")]
stress *args: _need-nextest
  @sh scripts/stress.sh "$@"

# Local flaky-hunt: run one test BINARY in a loop — a fresh process per
# iteration, running all its tests once via libtest.
#
# This amortizes the per-process node-startup cost across the binary's tests,
# unlike nextest's process-per-test, which pays it per test and, on macOS,
# serializes those payments (see mia-docs flaky-tests.md).
#
# `binary` matches a test target by substring; `count` defaults to 100:
#
#   just hammer linking 300
#
# Does not stop on failure; prints the failure log and the total.
[doc("Local flaky-hunt: loop one test binary, a fresh process per iteration")]
hammer binary count="100":
  @sh scripts/hammer.sh "$@"

# Check the dependency tree against deny.toml (advisories, licenses, sources)
audit:
  cargo deny check

# Lint and type-check without modifying files
check: _check-devcontainer-tools
  #!/bin/sh
  set -eux
  cargo fmt --all -- --check
  # Both configurations: the product build first — a break there is invisible
  # to a run that only ever enables the dev feature. Product targets only:
  # `--all-targets` builds the dev targets, and a dev-dependency on
  # `pdn-node/test-util` unifies the feature back in, which would make this
  # line a copy of the next one.
  cargo clippy --workspace --lib --bins
  cargo clippy --workspace --all-targets $(sh scripts/test-features.sh)
  cargo check --workspace --all-targets $(sh scripts/test-features.sh)

# Lint and type-check, attempt fixes
check-fix:
  #!/bin/sh
  set -eux
  cargo fmt --all
  cargo clippy --workspace --all-targets $(sh scripts/test-features.sh) --fix --allow-dirty --allow-staged
  # Both configurations: the product build first — a break there is invisible
  # to a run that only ever enables the dev feature. Product targets only:
  # `--all-targets` builds the dev targets, and a dev-dependency on
  # `pdn-node/test-util` unifies the feature back in, which would make this
  # line a copy of the next one.
  cargo clippy --workspace --lib --bins
  cargo clippy --workspace --all-targets $(sh scripts/test-features.sh)
  cargo check --workspace --all-targets $(sh scripts/test-features.sh)

# The store's other feature sets, its docs, and its wasm build, which
# `check` never compiles: clippy with warnings denied on `--all-features`
# and `--no-default-features`, rustdoc with warnings denied (an intra-doc
# link to a private item is one), and the featureless build for
# `wasm32-unknown-unknown`, where `getrandom` needs its backend named.
[doc("Lint the store under its other feature sets, its docs, and the wasm32 build")]
check-store:
  #!/bin/sh
  set -eu
  cargo clippy -p pdn-store --all-features --all-targets -- -Dwarnings
  cargo clippy -p pdn-store --no-default-features --lib --bins --tests -- -Dwarnings
  RUSTDOCFLAGS=-Dwarnings cargo doc -p pdn-store --all-features --no-deps
  RUSTFLAGS='--cfg getrandom_backend="wasm_js"' cargo build -p pdn-store --target wasm32-unknown-unknown --no-default-features

# Every test of the HTTP surface is a container test: needs a container
# daemon and builds the image. The store's feature sets and wasm build run as
# in the pipeline. `clean-stale --init` marks an unmarked target/, so a first
# run cleans too; `clean-stale` runs only after every step passed, since a
# failed run leaves unread what its later steps build.
[doc("Audit, lint, build, test, store matrix, container suite, drop stale target/ (needs docker)")]
precommit-check:
  #!/bin/sh
  set -eux
  just clean-stale --init
  just audit
  just check
  just check-store
  just test
  just test-store
  just test-docker
  just clean-stale

# Every test of the HTTP surface is a container test: needs a container
# daemon and builds the image. The store's feature sets and wasm build run as
# in the pipeline. `clean-stale --init` marks an unmarked target/, so a first
# run cleans too; `clean-stale` runs only after every step passed, since a
# failed run leaves unread what its later steps build.
[doc("Audit, lint, build, test, store matrix, container suite, attempt fixes, drop stale target/ (needs docker)")]
fix:
  #!/bin/sh
  set -eux
  just clean-stale --init
  just audit
  just check-fix
  just check-store
  just test
  just test-store
  just test-docker
  just clean-stale

# How it tells stale from live: scripts/clean-stale.py.
[doc("Remove target/ artifacts no build has read since the previous run (--dry-run: report only)")]
clean-stale *args:
  @python3 -I scripts/clean-stale.py "$@"

pr-review branch:
  #!/bin/sh
  set -eu
  git fetch origin
  git checkout {{ branch }}
  git pull origin {{ branch }}
  git checkout main
  git pull origin main
  git merge {{ branch }} --no-ff -m "Merge {{ branch }}"
  git reset --soft HEAD~1
  just build
