#!/bin/sh
# tools.toml as `name@version` words, one per tool.
set -eu
cd "$(dirname "$0")/.."
awk -F'"' '/^[a-z]/ { sub(/ *= *$/, "", $1); print $1 "@" $2 }' tools.toml
