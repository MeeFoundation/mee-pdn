set dotenv-load

_default:
  @ just --list --unsorted

# Clean demo data
clean-demo:
  rm -rf var/dev-nodes var/caps-alice-to-bob.txt || true

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

# Run workspace tests
test:
  #!/bin/sh
  set -eux
  cargo test --workspace --all-targets

# Test in release mode
test-release:
  #!/bin/sh
  set -eux
  cargo test --workspace --release --all-targets

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

# Lint, build, test
precommit-check:
  #!/bin/sh
  set -eux
  just check
  just test
  just build-core-wasm

# # Lint, build, test, attempt fixes
precommit-fix:
  #!/bin/sh
  set -eux
  just check-fix
  just test
  just build-core-wasm

# Build core libraries for wasm
build-core-wasm:
  #!/bin/sh
  set -eux
  cargo build --target wasm32-unknown-unknown \
    -p mee-wasm
  cargo build --target wasm32-wasip1 \
    -p mee-did-api \
    -p mee-did-key \
    -p mee-node-api

# Build the wasm-bindgen facade for browser target
wasm-build-bindgen:
  #!/bin/sh
  set -eux
  cargo build --target wasm32-unknown-unknown -p mee-wasm

# P2P demo (iroh, willow): share, import, replicate an entry
p2p-demo:
  #!/bin/sh
  set -eu
  mkdir -p var

  cargo build -p mee-demo

  cleanup() {
    echo "Cleaning up running nodes"
    set +e
    if [ -f var/alice.pid ]; then
      PID=$(cat var/alice.pid)
      kill "$PID" 2>/dev/null || true
      for i in $(seq 1 20); do
        if ! kill -0 "$PID" 2>/dev/null; then break; fi
        sleep 0.1
      done
      kill -9 "$PID" 2>/dev/null || true
      rm -f var/alice.pid
    fi
    if [ -f var/bob.pid ]; then
      PID=$(cat var/bob.pid)
      kill "$PID" 2>/dev/null || true
      for i in $(seq 1 20); do
        if ! kill -0 "$PID" 2>/dev/null; then break; fi
        sleep 0.1
      done
      kill -9 "$PID" 2>/dev/null || true
      rm -f var/bob.pid
    fi
  }
  trap cleanup EXIT

  # Reusable node URLs
  ALICE_HOST=127.0.0.1
  ALICE_PORT=3011
  ALICE_URL="http://${ALICE_HOST}:${ALICE_PORT}"
  BOB_HOST=127.0.0.1
  BOB_PORT=3012
  BOB_URL="http://${BOB_HOST}:${BOB_PORT}"

  echo "Initializing Alice and Bob nodes (P2P via mee-demo)"
  MEE_HOST=${ALICE_HOST} MEE_PORT=${ALICE_PORT} ./target/debug/mee-demo &
  echo $! > var/alice.pid
  MEE_HOST=${BOB_HOST} MEE_PORT=${BOB_PORT} ./target/debug/mee-demo &
  echo $! > var/bob.pid

  # Wait for readiness
  for i in $(seq 1 50); do
    if curl -sf ${ALICE_URL}/live >/dev/null 2>&1 && curl -sf ${BOB_URL}/live >/dev/null 2>&1; then
      break
    fi
    sleep 0.1
  done

  echo "Fetching Bob's DID-based invite"
  BOB_INVITE=$(curl -s ${BOB_URL}/p2p/invite)
  echo "Bob invite: ${BOB_INVITE}"

  echo "Alice connects to Bob P2P using his invite"
  curl -s -X POST ${ALICE_URL}/p2p/connect -H 'content-type: application/json' -d "{\"invite\":${BOB_INVITE}}"
  echo ""

  echo "Waiting for initial reconciliation..."
  curl -s "${BOB_URL}/p2p/sync-status?wait_ms=2000" >/dev/null
  echo "Alice inserts an entry into the shared space"
  curl -s -X POST ${ALICE_URL}/p2p/insert -H 'content-type: application/json' -d '{"path":"msgs/1","body":"hello via willow"}'

  echo "Waiting for replication..."
  curl -s "${BOB_URL}/p2p/sync-status?wait_ms=2000" >/dev/null
  echo "Listing entries on Bob after sync:"
  curl -s ${BOB_URL}/p2p/list || true
  echo "\n"

  echo "Inserting a second entry on Alice"
  curl -s -X POST ${ALICE_URL}/p2p/insert -H 'content-type: application/json' -d '{"path":"msgs/2","body":"hello again via willow"}' >/dev/null

  echo "Waiting for replication..."
  curl -s "${BOB_URL}/p2p/sync-status?wait_ms=2000" >/dev/null

  echo "Listing entries on Bob after second insert:"
  curl -s ${BOB_URL}/p2p/list || true
  echo "\n"

# Build the Docker image for the Axum node
build-image:
  #!/bin/sh
  set -eux
  IMAGE_TAG=${IMAGE_TAG:-mee-demo:dev}
  DOCKER_BUILDKIT=1 docker build -f ops/Dockerfile -t ${IMAGE_TAG} .

# Run the Docker image locally
run-image:
  #!/bin/sh
  set -eux
  IMAGE_TAG=${IMAGE_TAG:-mee-demo:dev}
  PORT=${PORT:-3000}
  docker run --rm -e MEE_PORT=${PORT} -e MEE_HOST=0.0.0.0 -p ${PORT}:${PORT} ${IMAGE_TAG}

# Build image and run container integration tests
test-containers:
  #!/bin/sh
  set -eux
  IMAGE_TAG=mee-demo:dev just build-image
  cargo test -p mee-demo --test containers -- --ignored --nocapture
