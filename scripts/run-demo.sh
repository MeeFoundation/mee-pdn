#!/bin/sh
# A live demo's stage: the nodes of ops/compose-<demo>.yml brought up,
# reached over the ports they publish on loopback, and torn down around the
# narration in ops/demo-<demo>.sh. The devcontainer's daemon publishes on the
# devcontainer's own loopback, so a run there reaches the nodes as a run on
# the daemon's host does.
#
# The nodes and their volumes are torn down on every exit, the failing one
# included: each node keeps its state on a volume, so a demo that leaves
# either behind has the next run meeting the last run's state, which is the
# one thing a demo must never do.
#
# Usage: run-demo.sh connections|pods
set -eu
cd "$(dirname "$0")/.."
demo=${1:-}
case "$demo" in
  connections|pods) ;;
  *) echo "usage: run-demo.sh connections|pods" >&2; exit 2 ;;
esac
docker info >/dev/null 2>&1 || { echo "no container daemon — the demo needs one"; exit 1; }
docker compose version >/dev/null 2>&1 || { echo "no compose plugin — the demo brings its nodes up with one"; exit 1; }
compose="docker compose -f ops/compose-$demo.yml"
export DEMO_COMPOSE="$compose"
# The build and the bring-up are stagehands: their output is kept back so
# the narration reads as one thing, and produced in full if either fails.
log=$(mktemp)
cleanup() {
  $DEMO_COMPOSE down --remove-orphans --volumes >/dev/null 2>&1 || true
  rm -f "$log"
}
trap cleanup EXIT
# The count comes from the compose file rather than from this line: a
# number written here goes stale the first time a node is added, and it
# already did.
nodes=$($compose config --services | wc -l | tr -d ' ')
# The nodes come up from one image named by its content id, so the show and
# the gate run one artifact rather than one tag: the one handed over, as the
# pipeline does after its cached build, or else the one built here.
if [ -n "${PDN_STAND_IMAGE:-}" ]; then
  printf 'Bringing %s nodes up from %s...\n' "$nodes" "$PDN_STAND_IMAGE"
else
  printf 'Building the node image and bringing %s of them up...\n' "$nodes"
  just build-image >"$log" 2>&1 || { cat "$log"; exit 1; }
  PDN_STAND_IMAGE=$(just stand-image)
  [ -n "$PDN_STAND_IMAGE" ] || { echo "the image built, but the daemon does not name it"; exit 1; }
fi
export PDN_STAND_IMAGE
$compose up -d --wait >"$log" 2>&1 || { cat "$log"; exit 1; }
sh "ops/demo-$demo.sh"
