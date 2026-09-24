#!/bin/sh
# Every tools.toml pin against .devcontainer/Dockerfile.app, which repeats a
# tool's version as `ARG <NAME>_VERSION=`; a tool it does not install has no
# such line.
set -eu
cd "$(dirname "$0")/.."
failed=0
# Assigned first: a failure inside `for … in $(…)` leaves an empty loop and a clean exit.
specs=$(sh scripts/tool-specs.sh)
for spec in $specs; do
  tool="${spec%@*}"; want="${spec#*@}"
  arg="$(echo "$tool" | tr 'a-z-' 'A-Z_')_VERSION"
  have="$(sed -n "s/^ARG $arg=//p" .devcontainer/Dockerfile.app)"
  if [ -n "$have" ] && [ "$have" != "$want" ]; then
    echo "$tool: tools.toml pins $want, .devcontainer/Dockerfile.app has ARG $arg=$have" >&2
    failed=1
  fi
done
exit "$failed"
