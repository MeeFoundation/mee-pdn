#!/bin/bash
# Act 1. Alice starts on her laptop: one node, 2 identities of hers, and
# entries written before any other device or person is involved.
# Nothing happens on the phone yet — it has not joined anything.
source "$(dirname "$0")/lib.sh"; need_nodes
A=$(ident alice); OTHER=$(alice_other)

curl -s "$ALICE/debug/status"
echo "=== the identities this node hosts ==="
curl -s "$ALICE/debug/identities" | jq

# Three entries, so that what a grant later withholds is real: one to grant,
# one to keep, one to grant with the right to write.
curl -s -X PUT "$ALICE/debug/data/$A/contact/email" --data-binary 'alice@example.org' >/dev/null
curl -s -X PUT "$ALICE/debug/data/$A/contact/phone" --data-binary '+31 6 1234 5678' >/dev/null
curl -s -X PUT "$ALICE/debug/data/$A/notes/private" --data-binary 'not for anyone' >/dev/null
# The same path under the other identity, holding something else: 2 lives on
# one node that are not 2 accounts.
curl -s -X PUT "$ALICE/debug/data/$OTHER/contact/email" --data-binary 'alice@work.example' >/dev/null

echo "=== the entries of the identity the phone will join ==="
curl -s "$ALICE/debug/data/$A" | jq
echo "=== the same path under her other identity ==="
curl -s "$ALICE/debug/data/$OTHER/contact/email"; echo
