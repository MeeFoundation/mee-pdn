#!/bin/sh
# The KERI crates' product builds stay leaves (crates/kerita/CLAUDE.md): no
# crate of the workspace but kerita-cesr under kerita, no async runtime, no
# network or storage crate, and nothing that draws randomness or reads the
# clock. `-e normal` leaves the dev-dependencies out: the tests sign with keys
# they generate.
set -eu
cd "$(dirname "$0")/.."
root="$(pwd)"
# Two steps, so a failing `cargo tree` fails the check rather than passing it.
tree="$(cargo tree -p kerita -e normal --prefix none --format '{p}')"
# A workspace crate prints its path, a registry crate none. kerita-cesr's tree
# is part of kerita's, so it is checked here too.
workspace="$(printf '%s\n' "$tree" | grep -F "($root/" | grep -v '^kerita \|^kerita-cesr ' || true)"
forbidden="$(printf '%s\n' "$tree" | awk '{print $1}' \
  | grep -E '^(tokio|async-std|smol|iroh|iroh-.*|noq-.*|quinn|reqwest|hyper|redb|rusqlite|heed|lmdb.*|sled|getrandom|rand|chrono|time)$' \
  || true)"
if [ -n "$workspace$forbidden" ]; then
  echo "the product build of kerita depends on what a leaf must not:" >&2
  printf '%s\n%s\n' "$workspace" "$forbidden" | sort -u | grep -v '^$' >&2
  exit 1
fi
