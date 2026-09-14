#!/bin/bash
# Act 3. Bob connects to Alice. His node mints the invite and draws it; the
# phone reads it. Then the connection turns up on Alice's laptop node, which
# took no part in the ceremony.
# On the phone: Connections -> Connect to someone -> Read a code ->
# Accepting an invitation to connect.
source "$(dirname "$0")/lib.sh"; need_nodes
A=$(ident alice); B=$(ident bob)

echo "waiting for Bob's endpoint to settle and take a home relay"
echo "  relay: $(pdnwait 'curl -s -X POST "$BOB/debug/identities/$B/invite?lifetime_secs=10" | jq -re ".inviter_addr.addrs[]|select(.Relay)|.Relay"')"

curl -s -X POST "$BOB/debug/identities/$B/invite?lifetime_secs=180" > "$PDN/tmp/invite.json"
jq '.inviter_addr.addrs' "$PDN/tmp/invite.json"
pdnqr "$PDN/tmp/invite.png" < "$PDN/tmp/invite.json"
echo "the code is on the screen and lives 180 seconds — read it with the phone"
echo "on the phone: Connections -> Connect to someone -> Read a code ->"
echo "Accepting an invitation to connect -> scan the code above."

echo "=== Bob's side of the connection ==="
pdnwait 'curl -s $BOB/debug/identities/$B/connections | jq -re ".connections[]|select(.==\"'"$A"'\")"' || exit 1
echo "Bob lists Alice"

echo "=== Alice's laptop, where nobody performed an act ==="
pdnbrowser alice
pdnwait 'curl -s $ALICE/debug/identities/$A/connections | jq -re ".connections[]|select(.==\"'"$B"'\")"' \
  && echo "the connection reached her other device on its own"

echo
echo "now present the same code to the phone a second time: it is refused, and"
echo "Bob still lists exactly one connection to Alice:"
echo "  curl -s $BOB/debug/identities/$B/connections | jq"
