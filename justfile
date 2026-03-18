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
