#!/bin/sh
# One test binary in a loop, a fresh process per iteration.
# Usage: hammer.sh <test target substring> <count>
set -eu
cd "$(dirname "$0")/.."
binary=$1
count=$2
export PDN_BIND_ADDR=127.0.0.1
exe=$(cargo test --workspace $(sh scripts/test-features.sh) --no-run --message-format=json 2>/dev/null \
  | python3 -I scripts/test-binary.py "$binary") || exit 1
echo "hammering $(basename "$exe") x$count (loopback, one process per iteration)"
fails=0
i=1
while [ "$i" -le "$count" ]; do
  if out=$(RUST_BACKTRACE=1 "$exe" </dev/null 2>&1); then
    printf '.'
  else
    printf 'X'
    fails=$((fails + 1))
    printf '\niter %s FAILED:\n%s\n' "$i" "$out" >&2
  fi
  i=$((i + 1))
done
echo
echo "hammer: $fails failures over $count iterations"
[ "$fails" -eq 0 ]
