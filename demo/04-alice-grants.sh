#!/bin/bash
# Act 4. Alice grants Bob one claim, from her phone. What she does not grant
# is absent on his node rather than hidden there.
# On the phone: Connections -> Bob -> Share claims with this peer ->
# tap contact/email -> Grant read-only.
source "$(dirname "$0")/lib.sh"; need_nodes
A=$(ident alice); B=$(ident bob)
pdnbrowser bob

echo "on the phone: Connections -> Bob -> Share claims with this peer ->"
echo "tap contact/email -> Grant read-only."
echo
echo "=== the grant, as Bob's node reads it ==="
pdnwait 'curl -s $BOB/debug/identities/$B/grants/$A | jq -ce "select(.grants|length>0)"' | jq || exit 1

echo "=== what arrived under Alice as the issuer ==="
pdnwait 'curl -s $BOB/debug/data/$A | jq -ce "select(.entries|length>0)"' | jq
echo "=== the value of the granted claim ==="
pdnwait 'curl -sf $BOB/debug/data/$A/contact/email'

echo "=== a path Alice holds and did not grant ==="
curl -s -w ' [HTTP %{http_code}]\n' "$BOB/debug/data/$A/notes/private"
echo "404 here means no such entry, not a refusal: outside the granted claim"
echo "nothing replicates to Bob at all — not the content, not the path, not the"
echo "number of them. Before the grant the answer was 409, namespace not bound."
