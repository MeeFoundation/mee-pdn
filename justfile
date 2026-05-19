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

# Run workspace unit tests
test:
  #!/bin/sh
  set -eux
  cargo test --workspace --all-targets

# Test in release mode
test-release:
  #!/bin/sh
  set -eux
  cargo test --workspace --release --all-targets

# Build image and run container integration tests
integration-tests:
  #!/bin/sh
  set -eux
  IMAGE_TAG=mee-demo:dev just build-image
  cargo test -p mee-demo --test containers -- --ignored --nocapture

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
  just build-core-wasm
  just integration-tests

# Lint, build, test, integration tests, attempt fixes
precommit-fix:
  #!/bin/sh
  set -eux
  just check-fix
  just test
  just build-core-wasm
  just integration-tests

# Build core libraries for wasm
build-core-wasm:
  #!/bin/sh
  set -eux
  cargo build --target wasm32-unknown-unknown \
    -p mee-wasm
  cargo build --target wasm32-wasip1 \
    -p mee-identity-api \
    -p mee-identity-keri \
    -p mee-node-api

# Build the wasm-bindgen facade for browser target
wasm-build-bindgen:
  #!/bin/sh
  set -eux
  cargo build --target wasm32-unknown-unknown -p mee-wasm

# P2P demo (iroh, willow): two local nodes share, connect, replicate entries
p2p-demo:
  #!/bin/sh
  set -eu
  mkdir -p var

  cargo build -p mee-demo

  cleanup() {
    echo "Cleaning up running nodes"
    set +e
    for who in alice bob; do
      if [ -f var/${who}.pid ]; then
        PID=$(cat var/${who}.pid)
        kill "$PID" 2>/dev/null || true
        for i in $(seq 1 20); do
          if ! kill -0 "$PID" 2>/dev/null; then break; fi
          sleep 0.1
        done
        kill -9 "$PID" 2>/dev/null || true
        rm -f var/${who}.pid
      fi
    done
  }
  trap cleanup EXIT

  ALICE_HOST=127.0.0.1
  ALICE_PORT=3011
  ALICE_URL="http://${ALICE_HOST}:${ALICE_PORT}"
  BOB_HOST=127.0.0.1
  BOB_PORT=3012
  BOB_URL="http://${BOB_HOST}:${BOB_PORT}"

  echo "Spawning Alice and Bob (mee-demo)"
  MEE_HOST=${ALICE_HOST} MEE_PORT=${ALICE_PORT} ./target/debug/mee-demo &
  echo $! > var/alice.pid
  MEE_HOST=${BOB_HOST} MEE_PORT=${BOB_PORT} ./target/debug/mee-demo &
  echo $! > var/bob.pid

  # Wait for /live readiness
  for i in $(seq 1 100); do
    if curl -sf ${ALICE_URL}/live >/dev/null 2>&1 && curl -sf ${BOB_URL}/live >/dev/null 2>&1; then
      break
    fi
    sleep 0.1
  done

  echo "Fetching Alice's home namespace"
  ALICE_NS=$(curl -s ${ALICE_URL}/p2p/home-namespace | sed -E 's/.*"namespace":"([^"]+)".*/\1/')
  echo "Alice namespace: ${ALICE_NS}"

  echo "Fetching Bob's invite"
  BOB_INVITE=$(curl -s ${BOB_URL}/p2p/invite)
  echo "Bob invite: ${BOB_INVITE}"

  echo "Alice connects to Bob using Bob's invite"
  curl -s -X POST ${ALICE_URL}/p2p/connect \
    -H 'content-type: application/json' \
    -d "{\"invite\":${BOB_INVITE}}"
  echo ""

  echo "Alice inserts msgs/1 into her home namespace"
  curl -s -X POST ${ALICE_URL}/p2p/insert \
    -H 'content-type: application/json' \
    -d "{\"namespace\":\"${ALICE_NS}\",\"path\":\"msgs/1\",\"body\":\"hello via willow\"}"
  echo ""

  echo "Waiting for replication of msgs/1 on Bob..."
  for i in $(seq 1 100); do
    if curl -s -X POST ${BOB_URL}/p2p/list \
        -H 'content-type: application/json' \
        -d "{\"namespace\":\"${ALICE_NS}\"}" | grep -q '"msgs/1"'; then
      break
    fi
    sleep 0.2
  done

  echo "Listing Alice's namespace on Bob after first insert:"
  curl -s -X POST ${BOB_URL}/p2p/list \
    -H 'content-type: application/json' \
    -d "{\"namespace\":\"${ALICE_NS}\"}"
  echo ""

  echo "Alice inserts msgs/2"
  curl -s -X POST ${ALICE_URL}/p2p/insert \
    -H 'content-type: application/json' \
    -d "{\"namespace\":\"${ALICE_NS}\",\"path\":\"msgs/2\",\"body\":\"hello again via willow\"}" >/dev/null
  echo ""

  echo "Waiting for replication of msgs/2 on Bob..."
  for i in $(seq 1 100); do
    if curl -s -X POST ${BOB_URL}/p2p/list \
        -H 'content-type: application/json' \
        -d "{\"namespace\":\"${ALICE_NS}\"}" | grep -q '"msgs/2"'; then
      break
    fi
    sleep 0.2
  done

  echo "Listing Alice's namespace on Bob after second insert:"
  curl -s -X POST ${BOB_URL}/p2p/list \
    -H 'content-type: application/json' \
    -d "{\"namespace\":\"${ALICE_NS}\"}"
  echo ""

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
