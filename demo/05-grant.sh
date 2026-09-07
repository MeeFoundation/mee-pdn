#!/bin/bash
# Act 5. The laptop shares with the phone: it publishes a grant on one claim.
# On the phone: the card What this peer shares with me fills itself.
source "$(dirname "$0")/lib.sh"; need_node
MINE=$(mine); PEER=$(peer)

curl -s -X POST "$MAC/debug/identities/$MINE/grants/$PEER" \
  -H 'content-type: application/json' \
  -d "{\"issuer\":\"$MINE\",\"claims\":[{\"path\":\"contact/email\",\"write\":false}]}" \
  -w 'HTTP %{http_code}\n'
curl -s "$MAC/debug/identities/$MINE/own-grants/$PEER" | jq
echo "the claims appear as hashes: a claim identity is derived one way from the"
echo "issuer and the path, and the path does not come back out of it."
