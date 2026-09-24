#!/bin/sh
# The live demo's stage: the nodes brought up, reached, and torn down around
# the narration in ops/demo.sh.
#
# Runs from the daemon's own host and from another container on that daemon
# alike: the first reaches the nodes over the ports they publish, the second
# over the container network, which is the only address it can reach. The
# narration is the same either way.
#
# The nodes and their volumes are torn down on every exit, the failing one
# included: each node keeps its state on a volume, so a demo that leaves
# either behind has the next run meeting the last run's state, which is the
# one thing a demo must never do.
set -eu
cd "$(dirname "$0")/.."
docker info >/dev/null 2>&1 || { echo "no container daemon — the demo needs one"; exit 1; }
docker compose version >/dev/null 2>&1 || { echo "no compose plugin — the demo brings its nodes up with one"; exit 1; }
# Which of the two the run is decides what comes up and how it is reached.
# A published port belongs to the daemon's host: a run from there adds the
# ports file and drives loopback, a run from a container leaves it out and
# drives the nodes' own addresses instead.
if [ -f /.dockerenv ]; then
  compose="docker compose -f ops/compose.yml"
  on_network=1
else
  compose="docker compose -f ops/compose.yml -f ops/compose.ports.yml"
  on_network=0
fi
export DEMO_COMPOSE="$compose"
# The build and the bring-up are stagehands: their output is kept back so
# the narration reads as one thing, and produced in full if either fails.
log=$(mktemp)
joined=0
# The namespace leaves the nodes' network before the nodes come down: a
# network still holding a member is a network the teardown cannot remove.
cleanup() {
  if [ "$joined" = 1 ]; then sh ops/demo-net.sh leave "$(hostname)" >/dev/null 2>&1 || true; fi
  $DEMO_COMPOSE down --remove-orphans --volumes >/dev/null 2>&1 || true
  rm -f "$log"
}
trap cleanup EXIT
# The count comes from the compose file rather than from this line: a
# number written here goes stale the first time a node is added, and it
# already did.
nodes=$($compose config --services | wc -l | tr -d ' ')
printf 'Building the node image and bringing %s of them up...\n' "$nodes"
just build-image >"$log" 2>&1 || { cat "$log"; exit 1; }
# The nodes come up from what was just built, named by its content id: the
# show and the gate then run one artifact rather than one tag.
PDN_STAND_IMAGE=$(just stand-image)
[ -n "$PDN_STAND_IMAGE" ] || { echo "the image built, but the daemon does not name it"; exit 1; }
export PDN_STAND_IMAGE
$compose up -d --wait >"$log" 2>&1 || { cat "$log"; exit 1; }
# On the network the run joins it first — a bridge network is reachable
# only from a namespace attached to it, and the namespace this joins is
# the one this container runs in, which its hostname names. The narration
# is then pointed at the nodes themselves, one URL per service of the
# compose file, and handed the resolver again for the node it restarts:
# an address here is a container's, and a container that comes back may
# come back on another.
if [ "$on_network" = 1 ]; then
  sh ops/demo-net.sh join "$(hostname)"
  joined=1
  export DEMO_RESOLVE="sh ops/demo-net.sh url"
  for svc in $($compose config --services); do
    eval "export $(echo "$svc" | tr 'a-z-' 'A-Z_')=$(sh ops/demo-net.sh url "$svc")"
  done
fi
sh ops/demo.sh
