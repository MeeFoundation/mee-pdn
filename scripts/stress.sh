#!/bin/sh
# Nextest for a flaky hunt, every arg forwarded; with no selection of the
# caller's own it runs the scenario tests alone.
set -eu
cd "$(dirname "$0")/.."
export PDN_BIND_ADDR=127.0.0.1
features=$(sh scripts/test-features.sh "$@")
case " $* " in
  *" -E "*|*" --filter-expr "*|*" -p "*|*" --package "*) cargo nextest run $features "$@" ;;
  *)                                                     cargo nextest run $features -E 'kind(test)' "$@" ;;
esac
