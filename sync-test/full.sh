#!/bin/sh
# Everything a full report reads for one test, in one run: the ramp with its catch-ups (load.md, "One command").
#
#   sh full.sh <variant: plain|phone> <container: true|false> [run id] [profile] [cpus] [memory] [load.py args...]
#
# Test L is `plain`, on the laptop or in the runner image with container=true. Test P is `phone`, run through here by
# pdn-app's `just load` once the phone is ready; it cannot run in a container. The profile is a file of load.py
# arguments under profiles/; the arguments after it come last, and argparse takes the last of a repeated option, so
# they override the profile's. cpus and memory cap the container; the laptop ignores them.
set -u
cd "$(dirname "$0")" || exit 2
WORKSPACE=$(cd .. && pwd)
variant=$1
container=$2
run=${3:-load-$(date +%Y%m%d-%H%M)}
profile=${4:-ci}
cpus=${5:-}
memory=${6:-}
shift "$(( $# < 6 ? $# : 6 ))"
id="$run-$profile-$variant"

say() { echo "full.sh: $* ($(date +%T))"; }

[ -f "profiles/$profile.args" ] \
  || { say "no profile $profile; profiles/ holds: $(cd profiles && echo *.args | sed 's/\.args//g')"; exit 2; }
# shellcheck disable=SC2046
set -- $(sed 's/#.*//' "profiles/$profile.args") "$@"

if [ "$container" = true ]; then
  [ "$variant" = plain ] || { say "test P needs the phone on the laptop's cable, not a container"; exit 2; }
  docker image inspect "${IMAGE:-pdn-load-test}" >/dev/null 2>&1 \
    || { say "no runner image: sh sync-test/container.sh build first"; exit 2; }
  out="$PWD/runs"
  say "$id: $*"
  NAME="$id" CPUS="$cpus" MEMORY="$memory" sh ./container.sh run "$@" --run-id "$id"
  status=$?
else
  # The node binary of the checked-out commit, for test L's nodes and test P's writers alike.
  cargo build --release --manifest-path "$WORKSPACE/Cargo.toml" -p pdn-node-http --features observation || exit 2
  out="$WORKSPACE/tmp/load"
  say "$id: $*"
  python3 -I load.py --variant "$variant" "$@" --run-id "$id"
  status=$?
fi
say "run in $out: $id"
exit $status
