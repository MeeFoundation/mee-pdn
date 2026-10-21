#!/bin/sh
# The product's dependency graph enables no feature of the observation
# surface: the live actor's ordered subscribers (`pdn-store/observation`),
# the runtime's hooks (`data-layer/test-util`, `pdn-node/test-util`) and the
# hosts' routes (`observation`). Each is a node of cargo's feature graph, so
# a product build that enabled one, by a default feature or a dependency's,
# prints it here. `-e normal` leaves the dev-dependencies out: a test build
# enables them on purpose.
set -eu
cd "$(dirname "$0")/.."
# Two steps, so a failing `cargo tree` fails the check rather than passing it.
tree="$(cargo tree --workspace -e normal,features -i pdn-store)"
hits="$(printf '%s\n' "$tree" | grep -E 'feature "(observation|test-util)"' || true)"
if [ -n "$hits" ]; then
  echo "the product graph enables an observation feature:" >&2
  echo "$hits" >&2
  exit 1
fi
