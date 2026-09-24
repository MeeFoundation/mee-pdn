#!/bin/sh
# The nextest profile bounding the stand's parallelism, chosen from what the
# container daemon reports about itself rather than from this machine's
# cores: the two differ whenever the daemon runs on a virtual machine or the
# suite runs inside a development container. Falls back to the profile's own
# default when no daemon answers, so a caller without one still gets a
# runnable command rather than an error from arithmetic on an empty string.
set -eu
cpus=$(docker info 2>/dev/null | awk '/^ *CPUs:/{print $2}')
case "$cpus" in ''|*[!0-9]*) echo "cap-2"; exit 0 ;; esac
for rung in 16 8 4 2; do
  [ "$cpus" -ge "$rung" ] && { echo "cap-$rung"; exit 0; }
done
echo "cap-1"
