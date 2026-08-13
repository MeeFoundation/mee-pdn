set dotenv-load
set positional-arguments

# Enables the write-retraction scenario tests' courtesy-bypass surface
# (`pdn-node/test-util`). Dev builds only — never a product build.
test_features := "--features pdn-node/test-util"

_default:
  @ just --list --unsorted

# The feature flag for a forwarded arg list, empty when the caller narrowed
# cargo's package selection away from pdn-node: `pdn-node/test-util` names a
# feature of one package, and cargo rejects it unless that package is selected.
_features *args:
  #!/bin/sh
  case " $* " in
    *" -p pdn-node "*|*" --package pdn-node "*|*" --package=pdn-node "*) echo '{{ test_features }}' ;;
    *" -p "*|*" --package "*|*" --package="*) ;;
    *) echo '{{ test_features }}' ;;
  esac

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

# Install local developer tooling (cargo-watch, cargo-nextest, wasm targets)
setup-tooling:
  #!/bin/sh
  set -eux
  cargo install cargo-watch
  cargo install cargo-nextest --locked
  rustup target add wasm32-wasip1 wasm32-unknown-unknown

# Run workspace tests via nextest — extra args forwarded (test nodes bind loopback — see data-layer node.rs). This workspace has no doctests, so nextest covers everything.
test *args:
  #!/bin/sh
  set -eu
  command -v cargo-nextest >/dev/null 2>&1 || { echo "cargo-nextest not found — run: just setup-tooling"; exit 1; }
  export PDN_BIND_ADDR=127.0.0.1
  cargo nextest run $(just _features "$@") "$@"

# Test in release mode via nextest — extra args forwarded (test nodes bind loopback — see data-layer node.rs)
test-release *args:
  #!/bin/sh
  set -eu
  command -v cargo-nextest >/dev/null 2>&1 || { echo "cargo-nextest not found — run: just setup-tooling"; exit 1; }
  export PDN_BIND_ADDR=127.0.0.1
  cargo nextest run --release $(just _features "$@") "$@"

# The image the stand runs. The scenarios look for exactly this tag.
image := "pdn-node-http:dev"

# Build the stand's image from the workspace as it resolves — a store fork
# pointed at the checkout beside it included.
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
#
# The nodes are torn down on every exit, the failing one included: a demo
# that leaves containers behind has the next run meeting the last run's
# state, which is the one thing a demo must never do.
[doc("Run the live demo across containers (needs docker)")]
demo:
  #!/bin/sh
  set -eu
  docker info >/dev/null 2>&1 || { echo "no container daemon — the demo needs one"; exit 1; }
  # The build and the bring-up are stagehands: their output is kept back so
  # the narration reads as one thing, and produced in full if either fails.
  log=$(mktemp)
  trap 'docker compose -f ops/compose.yml down --remove-orphans >/dev/null 2>&1; rm -f "$log"' EXIT
  # The count comes from the compose file rather than from this line: a
  # number written here goes stale the first time a node is added, and it
  # already did.
  nodes=$(docker compose -f ops/compose.yml config --services | wc -l | tr -d ' ')
  printf 'Building the node image and bringing %s of them up...\n' "$nodes"
  just build-image >"$log" 2>&1 || { cat "$log"; exit 1; }
  # The nodes come up from what was just built, named by its content id: the
  # show and the gate then run one artifact rather than one tag.
  PDN_STAND_IMAGE=$(just stand-image)
  [ -n "$PDN_STAND_IMAGE" ] || { echo "the image built, but the daemon does not name it"; exit 1; }
  export PDN_STAND_IMAGE
  docker compose -f ops/compose.yml up -d --wait >"$log" 2>&1 || { cat "$log"; exit 1; }
  sh ops/demo.sh

# The nextest profile bounding the stand's parallelism, chosen from what the
# container daemon reports about itself rather than from this machine's
# cores: the two differ whenever the daemon runs on a virtual machine or the
# suite runs inside a development container. Falls back to the profile's own
# default when no daemon answers, so a caller without one still gets a
# runnable command rather than an error from arithmetic on an empty string.
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
  #!/bin/sh
  set -eu
  cpus=$(docker info 2>/dev/null | awk '/^ *CPUs:/{print $2}')
  case "$cpus" in ''|*[!0-9]*) echo "cap-2"; exit 0 ;; esac
  for rung in 16 8 4 2; do
    [ "$cpus" -ge "$rung" ] && { echo "cap-$rung"; exit 0; }
  done
  echo "cap-1"

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
test-docker *args:
  #!/bin/sh
  set -eu
  command -v cargo-nextest >/dev/null 2>&1 || { echo "cargo-nextest not found — run: just setup-tooling"; exit 1; }
  docker info >/dev/null 2>&1 || { echo "no container daemon — the stand needs one"; exit 1; }
  just build-image
  # What was just built, named by its content id: every container of this run
  # starts from it, so a rebuild of the tag while the run is under way cannot
  # mix two revisions into one scenario.
  PDN_STAND_IMAGE=$(just stand-image)
  [ -n "$PDN_STAND_IMAGE" ] || { echo "the image built, but the daemon does not name it — refusing to run against a tag that can move"; exit 1; }
  export PDN_STAND_IMAGE
  cargo nextest run --profile "$(just stand-profile)" -p pdn-node-http -E 'binary(~stand)' --run-ignored all "$@"

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
stress *args:
  #!/bin/sh
  set -eu
  command -v cargo-nextest >/dev/null 2>&1 || { echo "cargo-nextest not found — run: just setup-tooling"; exit 1; }
  export PDN_BIND_ADDR=127.0.0.1
  features=$(just _features "$@")
  # Default to the scenario tests when the caller gave no selection; respect their filter otherwise.
  case " $* " in
    *" -E "*|*" --filter-expr "*|*" -p "*|*" --package "*) cargo nextest run $features "$@" ;;
    *)                                                     cargo nextest run $features -E 'kind(test)' "$@" ;;
  esac

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
  #!/bin/sh
  set -eu
  export PDN_BIND_ADDR=127.0.0.1
  exe=$(cargo test --workspace {{ test_features }} --no-run --message-format=json 2>/dev/null | python3 -c '
  import json, sys
  want = "{{ binary }}"
  hits = []
  for line in sys.stdin:
      try:
          m = json.loads(line)
      except ValueError:
          continue
      t = m.get("target", {})
      if (m.get("reason") == "compiler-artifact" and m.get("executable")
              and t.get("kind") == ["test"] and want in t.get("name", "")):
          hits.append((t["name"], m["executable"]))
  if len(hits) != 1:
      names = ", ".join(n for n, _ in hits) or "(none)"
      sys.stderr.write("want exactly one test binary matching \"" + want + "\"; matched: " + names + "\n")
      sys.exit(1)
  print(hits[0][1])
  ') || exit 1
  echo "hammering $(basename "$exe") x{{ count }} (loopback, one process per iteration)"
  fails=0
  i=1
  while [ "$i" -le {{ count }} ]; do
    if out=$(RUST_BACKTRACE=1 "$exe" </dev/null 2>&1); then
      printf '.'
    else
      printf 'X'
      fails=$((fails + 1))
      printf '\niter %s FAILED:\n%s\n' "$i" "$out" >&2
    fi
    i=$((i + 1))
  done
  echo
  echo "hammer: $fails failures over {{ count }} iterations"
  [ "$fails" -eq 0 ]

# Lint and type-check without modifying files
check:
  #!/bin/sh
  set -eux
  cargo fmt --all -- --check
  # Both configurations: the product build first — a break there is invisible
  # to a run that only ever enables the dev feature.
  cargo clippy --workspace --all-targets
  cargo clippy --workspace --all-targets {{ test_features }}
  cargo check --workspace --all-targets {{ test_features }}

# Lint and type-check, attempt fixes
check-fix:
  #!/bin/sh
  set -eux
  cargo fmt --all
  cargo clippy --workspace --all-targets {{ test_features }} --fix --allow-dirty --allow-staged
  # Both configurations: the product build first — a break there is invisible
  # to a run that only ever enables the dev feature.
  cargo clippy --workspace --all-targets
  cargo clippy --workspace --all-targets {{ test_features }}
  cargo check --workspace --all-targets {{ test_features }}

# Includes the container suite, as `fix` does: every test of the HTTP surface
# is one. Needs a container daemon, and builds the image.
[doc("Lint, build, test, container suite (needs docker)")]
precommit-check:
  #!/bin/sh
  set -eux
  just check
  just test
  just test-docker

# Includes the container suite: every test of the HTTP surface is one, so a
# pass without it says nothing about that crate. Needs a container daemon,
# and builds the image.
[doc("Lint, build, test, container suite, attempt fixes (needs docker)")]
fix:
  #!/bin/sh
  set -eux
  just check-fix
  just test
  just test-docker

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
