#!/bin/sh
# The stand: build the image, then run the container scenarios against it,
# extra args forwarded to nextest.
set -eu
cd "$(dirname "$0")/.."
docker info >/dev/null 2>&1 || { echo "no container daemon — the stand needs one"; exit 1; }
just build-image
# What was just built, named by its content id: every container of this run
# starts from it, so a rebuild of the tag while the run is under way cannot
# mix two revisions into one scenario.
PDN_STAND_IMAGE=$(just stand-image)
[ -n "$PDN_STAND_IMAGE" ] || { echo "the image built, but the daemon does not name it — refusing to run against a tag that can move"; exit 1; }
export PDN_STAND_IMAGE
cargo nextest run --profile "$(sh scripts/stand-profile.sh)" -p pdn-node-http -E 'binary(~stand)' --run-ignored all "$@"
