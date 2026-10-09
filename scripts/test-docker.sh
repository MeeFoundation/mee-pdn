#!/bin/sh
# The stand: build the image, then run the container scenarios against it,
# extra args forwarded to nextest.
set -eu
cd "$(dirname "$0")/.."
docker info >/dev/null 2>&1 || { echo "no container daemon — the stand needs one"; exit 1; }
just build-image
just build-observation-image
# What was just built, named by its content id: every container of this run
# starts from it, so a rebuild of the tag while the run is under way cannot
# mix two revisions into one scenario.
PDN_STAND_IMAGE=$(just stand-image)
PDN_STAND_OBSERVATION_IMAGE=$(just stand-image pdn-node-http:observation)
[ -n "$PDN_STAND_IMAGE" ] && [ -n "$PDN_STAND_OBSERVATION_IMAGE" ] || { echo "the images built, but the daemon does not name them — refusing to run against a tag that can move"; exit 1; }
export PDN_STAND_IMAGE PDN_STAND_OBSERVATION_IMAGE
cargo nextest run --profile "$(sh scripts/stand-profile.sh)" -p pdn-node-http -E 'binary(~stand)' --run-ignored all "$@"
