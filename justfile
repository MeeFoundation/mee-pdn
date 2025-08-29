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
  cargo clippy --workspace --all-targets -- -D warnings
  cargo check --workspace --all-targets

# Lint and type-check, attempt fixes
check-fix:
  #!/bin/sh
  set -eux
  cargo fmt --all
  cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged
  cargo clippy --workspace --all-targets -- -D warnings
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
    -p mee-transport-api \
    -p mee-did-api \
    -p mee-did-key \
    -p mee-local-store-api \
    -p mee-local-store-mem \
    -p mee-node-api

# Build the wasm-bindgen facade for browser target
wasm-build-bindgen:
  #!/bin/sh
  set -eux
  cargo build --target wasm32-unknown-unknown -p mee-wasm

# Full HTTP demo: start both servers, ping, show inbox, cleanup
http-demo:
  #!/bin/sh
  set -eu
  mkdir -p var

  cargo build -p mee-node-axum -p mee-dev

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

  echo "Initializing Alice and Bob nodes"
  ./target/debug/mee-node-axum --profile alice --addr ${ALICE_HOST}:${ALICE_PORT} --base-url ${ALICE_URL} &
  echo $! > var/alice.pid
  ./target/debug/mee-node-axum --profile bob --addr ${BOB_HOST}:${BOB_PORT} --base-url ${BOB_URL} &
  echo $! > var/bob.pid

  # Wait for readiness
  for i in $(seq 1 50); do
    if curl -sf ${ALICE_URL}/live >/dev/null 2>&1 && curl -sf ${BOB_URL}/live >/dev/null 2>&1; then
      break
    fi
    sleep 0.1
  done

  echo "Fetching Bob's ticket and sending ping from Alice"
  BOB_TICKET=$(curl -s ${BOB_URL}/demo/ticket | sed -E 's/.*"ticket"\s*:\s*"([^"]+)".*/\1/')
  echo "Bob ticket: ${BOB_TICKET}"
  curl -s -X POST ${ALICE_URL}/demo/send/ping -H 'content-type: application/json' \
    -d "{\"to_ticket\":\"${BOB_TICKET}\",\"body_b64\":\"aGVsbG8=\"}"

  echo "Bob inbox:"
  curl -s ${BOB_URL}/demo/inbox || true
  echo "\n"

# Build the Docker image for the Axum node
build-image:
  #!/bin/sh
  set -eux
  IMAGE_TAG=${IMAGE_TAG:-mee-node-axum:dev}
  DOCKER_BUILDKIT=1 docker build -f ops/Dockerfile -t ${IMAGE_TAG} .

# Run the Docker image locally
run-image:
  #!/bin/sh
  set -eux
  IMAGE_TAG=${IMAGE_TAG:-mee-node-axum:dev}
  PROFILE=${PROFILE:-node}
  PORT=${PORT:-3000}
  BASE_URL=${BASE_URL:-http://localhost:${PORT}}
  docker run --rm -p ${PORT}:${PORT} ${IMAGE_TAG} \
    --profile ${PROFILE} \
    --addr 0.0.0.0:${PORT} \
    --base-url ${BASE_URL}
