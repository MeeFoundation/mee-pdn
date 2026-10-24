#!/bin/sh
# Nextest for a flaky hunt, every arg forwarded; with no selection of the
# caller's own it runs the scenario tests alone.
set -eu
cd "$(dirname "$0")/.."
export PDN_BIND_ADDR=127.0.0.1
features=$(sh scripts/test-features.sh "$@")
log=$(mktemp)
# Ctrl-C ends a stress run as often as its last iteration does, and the log can run to hundreds of megabytes. INT
# as well as EXIT: dash runs no EXIT trap on a signal it has no trap for.
trap 'rm -f "$log" "$log.status"' EXIT INT TERM HUP
status=0
case " $* " in
  *" -E "*|*" --filter-expr "*|*" -p "*|*" --package "*) sh scripts/stress-run.sh "$log" $features "$@" || status=$? ;;
  *)                                                     sh scripts/stress-run.sh "$log" $features -E 'kind(test)' "$@" || status=$? ;;
esac
exit "$status"
