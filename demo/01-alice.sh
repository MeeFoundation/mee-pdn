#!/bin/bash
# Act 1. Alice's laptop node, shown as it already stands: one node, 2
# identities of hers, and entries seeded by 00-start.sh before any other
# device or person is involved. Nothing happens on the phone yet — it has
# not joined anything.
source "$(dirname "$0")/lib.sh"; need_nodes
A=$(ident alice); OTHER=$(alice_other)
pdnbrowser alice

curl -s "$ALICE/debug/status"
echo "=== the identities this node hosts ==="
curl -s "$ALICE/debug/identities" | jq

echo "=== the entries of the identity the phone will join ==="
curl -s "$ALICE/debug/data/$A" | jq
echo "=== the same path under her other identity ==="
curl -s "$ALICE/debug/data/$OTHER/contact/email"; echo
