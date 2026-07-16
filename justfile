set dotenv-load

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

# Install local developer tooling (cargo-watch, etc.)
setup-tooling:
  #!/bin/sh
  set -eux
  cargo install cargo-watch
  rustup target add wasm32-wasip1 wasm32-unknown-unknown

# Run workspace unit tests (test nodes bind loopback — see data-layer node.rs)
test:
  #!/bin/sh
  set -eux
  export PDN_BIND_LOOPBACK=1
  cargo test --workspace --all-targets

# Test in release mode (test nodes bind loopback — see data-layer node.rs)
test-release:
  #!/bin/sh
  set -eux
  export PDN_BIND_LOOPBACK=1
  cargo test --workspace --release --all-targets

# Stress scenario tests: every integration-test binary in a counted loop, one verdict line per run, full output kept for failures only (see mia-docs flaky-tests.md). `only`/`skip` narrow the binary set by comma-separated substrings of `crate--name` (e.g. `just stress 300 connection_metadata`, `just stress 100 "" connection_metadata`).
stress iterations="300" only="" skip="":
  #!/bin/sh
  set -eu
  export PDN_BIND_LOOPBACK=1
  out="target/stress/$(date +%Y%m%d-%H%M%S)"
  mkdir -p "$out/failures"
  cargo test --workspace --no-run --message-format=json 2>"$out/build.log" | python3 -c '
  import json, sys
  only = [s for s in "{{ only }}".split(",") if s]
  skip = [s for s in "{{ skip }}".split(",") if s]
  for line in sys.stdin:
      try:
          m = json.loads(line)
      except ValueError:
          continue
      if m.get("reason") != "compiler-artifact" or not m.get("executable"):
          continue
      t = m.get("target", {})
      if t.get("kind") != ["test"]:
          continue
      src = t.get("src_path", "")
      if "/crates/" not in src:
          continue
      crate = src.split("/crates/")[1].split("/")[0]
      name = crate + "--" + t["name"]
      if only and not any(s in name for s in only):
          continue
      if any(s in name for s in skip):
          continue
      print(name + " " + m["executable"])
  ' | sort > "$out/binaries.txt"
  ln -sfn "$(basename "$out")" target/stress/latest
  if ! [ -s "$out/binaries.txt" ]; then echo "no binaries match only={{ only }} skip={{ skip }}"; exit 2; fi
  echo "binaries under stress:"
  cat "$out/binaries.txt"
  echo "watch progress: tail -f target/stress/latest/stress.log"
  i=1
  while [ "$i" -le {{ iterations }} ]; do
    while read -r name exe; do
      t0=$(date +%s)
      if log=$(RUST_BACKTRACE=1 "$exe" </dev/null 2>&1); then
        printf 'iter %03d/%s %-32s ok    %3ds\n' "$i" "{{ iterations }}" "$name" "$(( $(date +%s) - t0 ))"
      else
        printf 'iter %03d/%s %-32s FAIL  %3ds\n' "$i" "{{ iterations }}" "$name" "$(( $(date +%s) - t0 ))"
        printf '%s\n' "$log" > "$out/failures/iter$i-$name.log"
      fi
    done < "$out/binaries.txt"
    i=$((i + 1))
  done | tee "$out/stress.log"
  failures=$(grep -c ' FAIL ' "$out/stress.log" || true)
  echo "stress complete: $failures failures over {{ iterations }} iterations (logs: $out)"
  [ "$failures" -eq 0 ]

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
precommit-fix:
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
