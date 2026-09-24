#!/bin/sh
# The container flaky hunt. Usage: stress-docker.sh <count> [nextest args...]
#
# Three things this encodes, each learnt from a hunt that lost evidence:
# the hunt runs to its end (`--no-fail-fast`), because one container that
# never gets its port published would otherwise cancel the remaining
# iterations; a failure keeps the nodes' logs, because the assertion says a
# value never arrived and only the logs say what the node was doing instead;
# and the count of replaced containers is printed either way, because a
# green run that replaced a dozen of them is not the same as a green run.
set -eu
cd "$(dirname "$0")/.."
count=$1
# The rest is nextest's: `count` left in would reach it as a filter and select nothing.
shift
docker info >/dev/null 2>&1 || { echo "no container daemon — the stand needs one"; exit 1; }
just build-image
PDN_STAND_IMAGE=$(just stand-image)
[ -n "$PDN_STAND_IMAGE" ] || { echo "the image built, but the daemon does not name it"; exit 1; }
export PDN_STAND_IMAGE
# The paths the harness writes to (`common/mod.rs`).
logs=target/tmp/stand-logs
replaced_log=target/tmp/stand-replacements.log
kept="target/tmp/stand-hunt-$(date +%Y%m%d-%H%M%S)"
# Cargo makes that directory when it builds the tests, and the first build
# here happens inside the image — on a fresh checkout the truncations below
# would find nothing to write into.
mkdir -p target/tmp
# Emptied first, so what is counted and kept belongs to this hunt alone —
# a day of runs leaves a hundred megabytes of logs behind otherwise.
rm -rf "$logs"
: > "$replaced_log"
status=0
# The runner's output is captured beside being shown: under
# `--stress-count` with `--no-fail-fast` the runner has been seen exiting
# zero while its own summary counted failed iterations, and a hunt that
# trusted the exit code alone then threw away exactly the evidence it
# exists to keep. The file is read back below; the `tail` is the live
# view, ended once the run is.
run_log="target/tmp/stand-hunt-run.log"
: > "$run_log"
tail -f "$run_log" &
tail_pid=$!
cargo nextest run --profile "$(sh scripts/stand-profile.sh)" -p pdn-node-http -E 'binary(~stand)' \
  --run-ignored all --stress-count "$count" --no-fail-fast "$@" >"$run_log" 2>&1 || status=$?
kill "$tail_pid" 2>/dev/null || true
wait "$tail_pid" 2>/dev/null || true
if [ "$status" -eq 0 ] && grep -qE '[1-9][0-9]* failed' "$run_log"; then
  echo "hunt: the runner exited clean while its summary counted failures — counting them"
  status=1
fi
replaced=$(grep -c 'never answered and was replaced' "$replaced_log" 2>/dev/null || true)
echo
echo "hunt: $count iterations requested, containers replaced: ${replaced:-0}"
if [ "$status" -eq 0 ]; then
  rm -rf "$logs"
  echo "hunt: nothing caught"
else
  mkdir -p "$kept"
  mv "$logs" "$kept/" 2>/dev/null || true
  cp "$replaced_log" "$kept/" 2>/dev/null || true
  cp "$run_log" "$kept/" 2>/dev/null || true
  echo "hunt: caught something — the nodes' logs are in $kept"
fi
exit "$status"
