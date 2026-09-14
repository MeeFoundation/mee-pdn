#!/bin/bash
# Act 6. A withdrawal closes what a grant opened, in both directions, and the
# same claim granted again reopens it.
# On the phone: What I share with this peer -> Withdraw this grant, then
# Grant read-only again. Bob's withdrawal below is answered on the phone's
# screen, which says he no longer shares it rather than showing a fault.
source "$(dirname "$0")/lib.sh"; need_nodes
A=$(ident alice); B=$(ident bob)
pdnbrowser bob

echo "on the phone: Connections -> Bob -> What I share with this peer ->"
echo "Withdraw this grant."
echo "=== withdraw on the phone; Bob stops knowing Alice as an issuer ==="
pdnwait 'curl -s $BOB/debug/identities/$B/grants/$A | jq -ce "select((.grants|length)==0)"' || exit 1
curl -s -w ' [HTTP %{http_code}]\n' "$BOB/debug/data/$A/contact/email"
echo "409 is the namespace unbound: not an empty answer, and not a fault —"
echo "his node stopped knowing that issuer altogether."
echo "Alice's own read of the same entry is untouched:"
curl -s "$ALICE/debug/data/$A/contact/email"; echo

echo "on the phone: still on Bob's connection screen, Share claims with this"
echo "peer -> tap contact/email -> Grant read-only."
echo "=== grant the same claim again on the phone; the access reopens ==="
pdnwait 'curl -sf $BOB/debug/data/$A/contact/email' && echo "^ read again by Bob"

echo
echo "=== now the other direction: Bob withdraws his grant to Alice ==="
curl -s -X DELETE "$BOB/debug/identities/$B/grants/$A/$B" -w 'HTTP %{http_code}\n'
echo "watch the phone: the claims leave the card with a line saying this peer"
echo "no longer shares them — plain text, no error banner."
echo "press enter to give it back"; read -r _
curl -s -X POST "$BOB/debug/identities/$B/grants/$A" \
  -H 'content-type: application/json' \
  -d "{\"issuer\":\"$B\",\"claims\":[{\"path\":\"contact/email\",\"write\":false},{\"path\":\"notes/shared\",\"write\":true}]}" \
  -w 'HTTP %{http_code}\n'
