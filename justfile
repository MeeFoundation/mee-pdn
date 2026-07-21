set dotenv-load
set positional-arguments

_default:
  @ just --list --unsorted

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
  cargo nextest run "$@"

# Test in release mode via nextest — extra args forwarded (test nodes bind loopback — see data-layer node.rs)
test-release *args:
  #!/bin/sh
  set -eu
  command -v cargo-nextest >/dev/null 2>&1 || { echo "cargo-nextest not found — run: just setup-tooling"; exit 1; }
  export PDN_BIND_ADDR=127.0.0.1
  cargo nextest run --release "$@"

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
  # Default to the scenario tests when the caller gave no selection; respect their filter otherwise.
  case " $* " in
    *" -E "*|*" --filter-expr "*|*" -p "*|*" --package "*) cargo nextest run "$@" ;;
    *)                                                     cargo nextest run -E 'kind(test)' "$@" ;;
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
  exe=$(cargo test --workspace --no-run --message-format=json 2>/dev/null | python3 -c '
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
  cargo clippy --workspace --all-targets
  cargo check --workspace --all-targets

# Lint and type-check, attempt fixes
check-fix:
  #!/bin/sh
  set -eux
  cargo fmt --all
  cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged
  cargo clippy --workspace --all-targets
  cargo check --workspace --all-targets

# Lint, build, test, integration tests
precommit-check:
  #!/bin/sh
  set -eux
  just check
  just test

# Lint, build, test, integration tests, attempt fixes
fix:
  #!/bin/sh
  set -eux
  just check-fix
  just test

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
