#!/bin/bash
# Act 8. Carol holds no connection to Alice and no grant from her. She obtains
# nothing — not the granted claim, not another, not the knowledge that any
# exists — while Bob demonstrably reads the granted claim in the same place.
# Nothing happens on the phone.
source "$(dirname "$0")/lib.sh"; need_nodes
A=$(ident alice); B=$(ident bob); C=$(ident carol)

echo "=== Carol's connections ==="
curl -s "$CAROL/debug/identities/$C/connections" | jq
echo "=== Carol reading the claim Bob was granted ==="
curl -s -w ' [HTTP %{http_code}]\n' "$CAROL/debug/data/$A/contact/email"
echo "=== Carol listing anything at all of Alice's ==="
curl -s -w ' [HTTP %{http_code}]\n' "$CAROL/debug/data/$A"
echo "409: she addresses an issuer she holds nothing of. Not an empty list —"
echo "her node does not know that issuer exists."
echo
echo "=== the same claim, on Bob's node, at the same moment ==="
curl -s -w ' [HTTP %{http_code}]\n' "$BOB/debug/data/$A/contact/email"
