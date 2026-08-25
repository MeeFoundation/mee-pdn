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

# Run workspace tests via nextest — extra args forwarded (test nodes bind loopback — see data-layer node.rs). Nextest runs no doctests and the store's README is one, so a run with no selection ends with the workspace doctests; a `-p` or `-E` selection is nextest's alone and skips them.
test *args:
  #!/bin/sh
  set -eu
  command -v cargo-nextest >/dev/null 2>&1 || { echo "cargo-nextest not found — run: just setup-tooling"; exit 1; }
  export PDN_BIND_ADDR=127.0.0.1
  cargo nextest run $(just _features "$@") "$@"
  # Doctests are outside nextest's reach, and a selection cannot name them.
  case " $* " in
    *" -E "*|*" --filter-expr "*|*" -p "*|*" --package "*|*" --package="*) ;;
    *) cargo test --workspace --doc {{ test_features }} ;;
  esac

# The store's other feature sets, beyond the default one the workspace
# builds: `--all-features` and `--no-default-features` compile different
# code — `fs-store` gates the file-backed store, `rpc` the network API — and
# the wasm build in `check-store` is the one consumer of the featureless one.
# `-p` names the package by its own name, `iroh-docs`: cargo selects packages
# before any workspace alias applies. Extra args are forwarded to nextest.
[doc("Test the store under its other feature sets, doctests included")]
test-store *args:
  #!/bin/sh
  set -eu
  command -v cargo-nextest >/dev/null 2>&1 || { echo "cargo-nextest not found — run: just setup-tooling"; exit 1; }
  cargo nextest run -p iroh-docs --all-features "$@"
  cargo nextest run -p iroh-docs --no-default-features "$@"
  cargo test -p iroh-docs --all-features --doc

# Test in release mode via nextest — extra args forwarded (test nodes bind loopback — see data-layer node.rs)
test-release *args:
  #!/bin/sh
  set -eu
  command -v cargo-nextest >/dev/null 2>&1 || { echo "cargo-nextest not found — run: just setup-tooling"; exit 1; }
  export PDN_BIND_ADDR=127.0.0.1
  cargo nextest run --release $(just _features "$@") "$@"

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
#
# Runs from the daemon's own host and from another container on that daemon
# alike: the first reaches the nodes over the ports they publish, the second
# over the container network, which is the only address it can reach. The
# narration is the same either way.
#
# The nodes and their volumes are torn down on every exit, the failing one
# included: each node keeps its state on a volume, so a demo that leaves
# either behind has the next run meeting the last run's state, which is the
# one thing a demo must never do.
[doc("Run the live demo across containers (needs docker)")]
demo:
  #!/bin/sh
  set -eu
  docker info >/dev/null 2>&1 || { echo "no container daemon — the demo needs one"; exit 1; }
  docker compose version >/dev/null 2>&1 || { echo "no compose plugin — the demo brings its nodes up with one"; exit 1; }
  # Which of the two the run is decides what comes up and how it is reached.
  # A published port belongs to the daemon's host: a run from there adds the
  # ports file and drives loopback, a run from a container leaves it out and
  # drives the nodes' own addresses instead.
  if [ -f /.dockerenv ]; then
    compose="docker compose -f ops/compose.yml"
    on_network=1
  else
    compose="docker compose -f ops/compose.yml -f ops/compose.ports.yml"
    on_network=0
  fi
  export DEMO_COMPOSE="$compose"
  # The build and the bring-up are stagehands: their output is kept back so
  # the narration reads as one thing, and produced in full if either fails.
  log=$(mktemp)
  joined=0
  # The namespace leaves the nodes' network before the nodes come down: a
  # network still holding a member is a network the teardown cannot remove.
  cleanup() {
    if [ "$joined" = 1 ]; then sh ops/demo-net.sh leave "$(hostname)" >/dev/null 2>&1 || true; fi
    $DEMO_COMPOSE down --remove-orphans --volumes >/dev/null 2>&1 || true
    rm -f "$log"
  }
  trap cleanup EXIT
  # The count comes from the compose file rather than from this line: a
  # number written here goes stale the first time a node is added, and it
  # already did.
  nodes=$($compose config --services | wc -l | tr -d ' ')
  printf 'Building the node image and bringing %s of them up...\n' "$nodes"
  just build-image >"$log" 2>&1 || { cat "$log"; exit 1; }
  # The nodes come up from what was just built, named by its content id: the
  # show and the gate then run one artifact rather than one tag.
  PDN_STAND_IMAGE=$(just stand-image)
  [ -n "$PDN_STAND_IMAGE" ] || { echo "the image built, but the daemon does not name it"; exit 1; }
  export PDN_STAND_IMAGE
  $compose up -d --wait >"$log" 2>&1 || { cat "$log"; exit 1; }
  # On the network the run joins it first — a bridge network is reachable
  # only from a namespace attached to it, and the namespace this joins is
  # the one this container runs in, which its hostname names. The narration
  # is then pointed at the nodes themselves, one URL per service of the
  # compose file, and handed the resolver again for the node it restarts:
  # an address here is a container's, and a container that comes back may
  # come back on another.
  if [ "$on_network" = 1 ]; then
    sh ops/demo-net.sh join "$(hostname)"
    joined=1
    export DEMO_RESOLVE="sh ops/demo-net.sh url"
    for svc in $($compose config --services); do
      eval "export $(echo "$svc" | tr 'a-z-' 'A-Z_')=$(sh ops/demo-net.sh url "$svc")"
    done
  fi
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

# The container flaky hunt: the stand's suite repeated, with everything a
# failure needs kept and everything a clean run leaves behind thrown away.
#
# Three things this encodes, each learnt from a hunt that lost evidence:
# the sweep runs to its end (`--no-fail-fast`), because one container that
# never gets its port published would otherwise cancel the remaining
# iterations; a failure keeps the nodes' logs, because the assertion says a
# value never arrived and only the logs say what the node was doing instead;
# and the count of replaced containers is printed either way, because a
# green run that replaced a dozen of them is not the same as a green run.
#
# Extra args are forwarded to `cargo nextest run`. Needs a container daemon.
[doc("Hunt flaky container scenarios: repeat the stand's suite, keep what a failure needs")]
stress-docker count="100" *args:
  #!/bin/sh
  set -eu
  command -v cargo-nextest >/dev/null 2>&1 || { echo "cargo-nextest not found — run: just setup-tooling"; exit 1; }
  docker info >/dev/null 2>&1 || { echo "no container daemon — the stand needs one"; exit 1; }
  just build-image
  PDN_STAND_IMAGE=$(just stand-image)
  [ -n "$PDN_STAND_IMAGE" ] || { echo "the image built, but the daemon does not name it"; exit 1; }
  export PDN_STAND_IMAGE
  # The paths the harness writes to (`common/mod.rs`).
  logs=target/tmp/stand-logs
  replaced_log=target/tmp/stand-replacements.log
  kept="target/tmp/stand-hunt-$(date +%Y%m%d-%H%M%S)"
  # Cargo makes that directory when it builds the tests, and the first build
  # here happens inside the image — on a fresh checkout the truncations below
  # would find nothing to write into.
  mkdir -p target/tmp
  # Emptied first, so what is counted and kept belongs to this hunt alone —
  # a day of runs leaves a hundred megabytes of logs behind otherwise.
  rm -rf "$logs"
  : > "$replaced_log"
  status=0
  # `set positional-arguments` puts every parameter in "$@", `count` first —
  # left in, it reaches nextest as a filter and the hunt selects nothing.
  shift
  # The runner's output is captured beside being shown: under
  # `--stress-count` with `--no-fail-fast` the runner has been seen exiting
  # zero while its own summary counted failed iterations, and a hunt that
  # trusted the exit code alone then threw away exactly the evidence it
  # exists to keep. The file is read back below; the `tail` is the live
  # view, ended once the run is.
  run_log="target/tmp/stand-hunt-run.log"
  : > "$run_log"
  tail -f "$run_log" &
  tail_pid=$!
  cargo nextest run --profile "$(just stand-profile)" -p pdn-node-http -E 'binary(~stand)' \
    --run-ignored all --stress-count {{ count }} --no-fail-fast "$@" >"$run_log" 2>&1 || status=$?
  kill "$tail_pid" 2>/dev/null || true
  wait "$tail_pid" 2>/dev/null || true
  if [ "$status" -eq 0 ] && grep -qE '[1-9][0-9]* failed' "$run_log"; then
    echo "hunt: the runner exited clean while its summary counted failures — counting them"
    status=1
  fi
  replaced=$(grep -c 'never answered and was replaced' "$replaced_log" 2>/dev/null || true)
  echo
  echo "hunt: {{ count }} iterations requested, containers replaced: ${replaced:-0}"
  if [ "$status" -eq 0 ]; then
    rm -rf "$logs"
    echo "hunt: nothing caught"
  else
    mkdir -p "$kept"
    mv "$logs" "$kept/" 2>/dev/null || true
    cp "$replaced_log" "$kept/" 2>/dev/null || true
    cp "$run_log" "$kept/" 2>/dev/null || true
    echo "hunt: caught something — the nodes' logs are in $kept"
  fi
  exit "$status"

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
  # to a run that only ever enables the dev feature. Product targets only:
  # `--all-targets` builds the dev targets, and a dev-dependency on
  # `pdn-node/test-util` unifies the feature back in, which would make this
  # line a copy of the next one.
  cargo clippy --workspace --lib --bins
  cargo clippy --workspace --all-targets {{ test_features }}
  cargo check --workspace --all-targets {{ test_features }}

# Lint and type-check, attempt fixes
check-fix:
  #!/bin/sh
  set -eux
  cargo fmt --all
  cargo clippy --workspace --all-targets {{ test_features }} --fix --allow-dirty --allow-staged
  # Both configurations: the product build first — a break there is invisible
  # to a run that only ever enables the dev feature. Product targets only:
  # `--all-targets` builds the dev targets, and a dev-dependency on
  # `pdn-node/test-util` unifies the feature back in, which would make this
  # line a copy of the next one.
  cargo clippy --workspace --lib --bins
  cargo clippy --workspace --all-targets {{ test_features }}
  cargo check --workspace --all-targets {{ test_features }}

# The store's other feature sets, its docs, and its wasm build, which
# `check` never compiles: clippy with warnings denied on `--all-features`
# and `--no-default-features`, rustdoc with warnings denied (an intra-doc
# link to a private item is one), and the featureless build for
# `wasm32-unknown-unknown`, where `getrandom` needs its backend named.
[doc("Lint the store under its other feature sets, its docs, and the wasm32 build")]
check-store:
  #!/bin/sh
  set -eu
  cargo clippy -p iroh-docs --all-features --all-targets -- -Dwarnings
  cargo clippy -p iroh-docs --no-default-features --lib --bins --tests -- -Dwarnings
  RUSTDOCFLAGS=-Dwarnings cargo doc -p iroh-docs --all-features --no-deps
  RUSTFLAGS='--cfg getrandom_backend="wasm_js"' cargo build -p iroh-docs --target wasm32-unknown-unknown --no-default-features

# Includes the container suite, as `fix` does: every test of the HTTP surface
# is one. Needs a container daemon, and builds the image. The store's other
# feature sets and its wasm build run here as the pipeline runs them.
[doc("Lint, build, test, store matrix, container suite (needs docker)")]
precommit-check:
  #!/bin/sh
  set -eux
  just check
  just check-store
  just test
  just test-store
  just test-docker

# Includes the container suite: every test of the HTTP surface is one, so a
# pass without it says nothing about that crate. Needs a container daemon,
# and builds the image. The store's other feature sets and its wasm build
# run here as the pipeline runs them.
[doc("Lint, build, test, store matrix, container suite, attempt fixes (needs docker)")]
fix:
  #!/bin/sh
  set -eux
  just check-fix
  just check-store
  just test
  just test-store
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
