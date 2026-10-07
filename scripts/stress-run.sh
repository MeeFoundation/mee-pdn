#!/bin/sh
# Nextest with its output shown and kept in a log, exiting non-zero when any
# stress iteration failed. Usage: stress-run.sh <log> [nextest args...]
#
# Under `--stress-count` or `--stress-duration` with fail-fast off, the
# pinned runner exits with the status of its last iteration alone while its
# summary counts every failed one, so the summary is read back.
set -eu
cd "$(dirname "$0")/.."
log=$1
shift
# A pipe in place of the terminal turns the runner's colour off.
if [ -t 1 ]; then
  export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"
fi
# The status crosses the pipe in a file: sh has no pipefail. `tee` ignores
# Ctrl-C, or the runner's own summary of a cancelled run goes nowhere.
{
  status=0
  cargo nextest run "$@" 2>&1 || status=$?
  echo "$status" >"$log.status"
} | (trap '' INT && exec tee "$log")
status=$(cat "$log.status")
rm -f "$log.status"
if [ "$status" -eq 0 ] && awk '
  { gsub(/\033\[[0-9;]*m/, "") }
  /stress run iterations:/ && /(^|[^0-9])[1-9][0-9]* failed/ { found = 1 }
  END { exit !found }
' "$log"; then
  echo "stress: the runner exited clean while its summary counted failed iterations"
  status=1
fi
exit "$status"
