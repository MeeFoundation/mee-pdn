#!/bin/sh
# Nextest for a flaky hunt, every arg forwarded; with no selection of the
# caller's own it runs the scenario tests alone.
set -eu
cd "$(dirname "$0")/.."
export PDN_BIND_ADDR=127.0.0.1
features=$(sh scripts/test-features.sh "$@")
log=$(mktemp)
status=0
case " $* " in
  *" -E "*|*" --filter-expr "*|*" -p "*|*" --package "*) sh scripts/stress-run.sh "$log" $features "$@" || status=$? ;;
  *)                                                     sh scripts/stress-run.sh "$log" $features -E 'kind(test)' "$@" || status=$? ;;
esac
rm -f "$log"
exit "$status"
