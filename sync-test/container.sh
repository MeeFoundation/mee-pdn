#!/bin/sh
# The load test in a container.
#
#   sh container.sh build                 node image from the mee-pdn workspace, then the runner image
#   sh container.sh runner                the runner image alone, over a node image already built
#   sh container.sh run <load.py args>    e.g. --ramp 20 --n 2,4,8 --v 5000 --skip-catchup
#
# Results land in ./runs/<run id>/ (mounted at /out); `docker stop <name>` ends a run cleanly. CPUS and MEMORY cap
# the container, so the numbers are measured against a known budget:
#   CPUS=8 MEMORY=12g sh container.sh run --ramp 20 --n 4 --v 2000 --skip-catchup
set -eu
cd "$(dirname "$0")"
WORKSPACE=$(cd .. && pwd)
IMAGE=${IMAGE:-pdn-load-test}

case "${1:-}" in
build)
  (cd "$WORKSPACE" && just build-observation-image)
  exec sh ./container.sh runner
  ;;
runner)
  rev=$(git -C "$WORKSPACE" rev-parse HEAD)$(git -C "$WORKSPACE" diff --quiet || echo +dirty)
  docker build -t "$IMAGE" --build-arg "REVISION=$rev" .
  ;;
run)
  shift
  mkdir -p runs
  limits=""
  [ -n "${CPUS:-}" ] && limits="$limits --cpus $CPUS"
  [ -n "${MEMORY:-}" ] && limits="$limits --memory $MEMORY"
  name=${NAME:-pdn-load-$(date +%Y%m%d-%H%M%S)}
  echo "container $name; stop it with: docker stop $name"
  # --init forwards docker stop's SIGTERM to the runner, which stops every node and saves the run; the stop timeout
  # gives it the seconds that takes, where Docker's default would kill it first.
  # shellcheck disable=SC2086
  exec docker run --rm --init --stop-timeout 60 --name "$name" $limits -v "$PWD/runs:/out" "$IMAGE" "$@"
  ;;
*)
  sed -n '2,10p' container.sh | sed 's/^# \{0,1\}//'
  exit 2
  ;;
esac
