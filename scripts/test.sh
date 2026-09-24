#!/bin/sh
# Nextest over the workspace, extra args forwarded; test nodes bind loopback
# (see data-layer node.rs). Nextest runs no doctests and the store's README
# is one, so a run with no selection ends with the workspace doctests.
set -eu
cd "$(dirname "$0")/.."
export PDN_BIND_ADDR=127.0.0.1
cargo nextest run $(sh scripts/test-features.sh "$@") "$@"
# Doctests are outside nextest's reach, and a selection cannot name them.
case " $* " in
  *" -E "*|*" --filter-expr "*|*" -p "*|*" --package "*|*" --package="*) ;;
  *) cargo test --workspace --doc $(sh scripts/test-features.sh) ;;
esac
