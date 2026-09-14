#!/bin/bash
# Act 2. Alice joins her phone to the identity that already holds those
# entries. The payload is minted here and drawn as a code; a machine has no
# camera, so the phone is the side that reads.
# On the phone: Bring the node up and nothing else — no identity is created
# here. The first identity this phone ever holds is the one that arrives.
# Then: Add an identity -> Receive one from another device -> Joining this
# device to an identity.
source "$(dirname "$0")/lib.sh"; need_nodes
A=$(ident alice)

echo "waiting for the endpoint to settle and take a home relay"
echo "  relay: $(pdnwait 'curl -s -X POST "$ALICE/debug/identities/$A/linking-invite?lifetime_secs=10" | jq -re ".inviter_addr.addrs[]|select(.Relay)|.Relay"')"

curl -s -X POST "$ALICE/debug/identities/$A/linking-invite?lifetime_secs=180" > "$PDN/tmp/link.json"
jq '{identity, addrs: .inviter_addr.addrs}' "$PDN/tmp/link.json"
pdnqr "$PDN/tmp/link.png" < "$PDN/tmp/link.json"
echo "the code is on the screen and lives 180 seconds — read it with the phone"
echo
echo "on the phone before reading it: Bring the node up, and no more than that."
echo "An identity created here first is indistinguishable on the screen from"
echo "the one that arrives, and the arrival is the whole act."
echo
echo "then: Add an identity -> Receive one from another device -> Joining this"
echo "device to an identity -> scan the code above."
echo
echo "what to look for on the phone once the ceremony ends:"
echo "  - it hosts $A, and no other identity of Alice's"
echo "  - My entries already lists contact/email, contact/phone and notes/private,"
echo "    none of which were written on this phone"
