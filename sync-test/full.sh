#!/bin/sh
# Everything a full report reads for one test, in one run: the ramp with its catch-ups (load.md, "One command").
#
#   sh full.sh <variant: plain|phone> <container: true|false> [run id]
#
# Test L is `plain`, on the laptop or in the runner image with container=true. Test P is `phone`, run through here by
# pdn-app's `just load` once the phone is ready; it cannot run in a container. RAMP overrides load.py's arguments, for
# a smoke run of the script itself.
set -u
cd "$(dirname "$0")" || exit 2
WORKSPACE=$(cd .. && pwd)
variant=$1
container=$2
run=${3:-load-$(date +%Y%m%d-%H%M)}
# The profile the CI history compares.
RAMP=${RAMP:-"--ramp 20 --n 2,4,8,16,12,24 --v 20000 --s 256"}
id="$run-ramp-$variant"

say() { echo "full.sh: $* ($(date +%T))"; }

if [ "$container" = true ]; then
  [ "$variant" = plain ] || { say "test P needs the phone on the laptop's cable, not a container"; exit 2; }
  docker image inspect "${IMAGE:-pdn-load-test}" >/dev/null 2>&1 \
    || { say "no runner image: sh sync-test/container.sh build first"; exit 2; }
  out="$PWD/runs"
  say "$id"
  # shellcheck disable=SC2086
  NAME="$id" sh ./container.sh run $RAMP --run-id "$id"
  status=$?
else
  # The node binary of the checked-out commit, for test L's nodes and test P's writers alike.
  cargo build --release --manifest-path "$WORKSPACE/Cargo.toml" -p pdn-node-http --features observation || exit 2
  out="$WORKSPACE/tmp/load"
  say "$id"
  # shellcheck disable=SC2086
  python3 -I load.py --variant "$variant" $RAMP --run-id "$id"
  status=$?
fi
say "run in $out: $id"
exit $status
