#!/bin/bash
# Act 3. The ceremony: show a QR and wait for the phone to read it.
# On the phone: Connections -> Read a code -> Accepting an invitation to connect.
source "$(dirname "$0")/lib.sh"; need_node
MINE=$(mine)

echo "waiting for the endpoint to settle and take a home relay"
echo "  relay: $(pdnwait 'curl -s -X POST "$MAC/debug/identities/$MINE/invite?lifetime_secs=10" | jq -re ".inviter_addr.addrs[]|select(.Relay)|.Relay"')"

curl -s -X POST "$MAC/debug/identities/$MINE/invite?lifetime_secs=180" > "$PDN/tmp/invite.json"
jq '.inviter_addr.addrs' "$PDN/tmp/invite.json"
pdnqr "$PDN/tmp/invite.png" < "$PDN/tmp/invite.json"
echo "the code is on the screen and lives 180 seconds — read it with the phone"

PEER=$(pdnwait 'curl -s $MAC/debug/identities/$MINE/connections | jq -re ".connections[0]"') || exit 1
echo "$PEER" > "$PDN/tmp/peer-id"
echo "the phone: $PEER"
