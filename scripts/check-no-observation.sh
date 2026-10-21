#!/bin/sh
# Whether a built facade carries the observation surface: `strings` over the
# artifact for the names of 2 calls the surface exports and no product build
# compiles. `absent` passes when neither is there; `present` passes when one
# is, which proves the probe sees what it looks for.
set -eu
artifact="$1"
expect="$2"
[ -r "$artifact" ] || { echo "no artifact at $artifact" >&2; exit 2; }
found="$(strings "$artifact" | grep -cE 'serve_test_surface|report_device_sample' || true)"
case "$expect" in
  absent) [ "$found" -eq 0 ] || { echo "$artifact carries the observation surface ($found names)" >&2; exit 1; } ;;
  present) [ "$found" -gt 0 ] || { echo "$artifact lacks the observation surface" >&2; exit 1; } ;;
  *) echo "expect 'absent' or 'present', not '$expect'" >&2; exit 2 ;;
esac
