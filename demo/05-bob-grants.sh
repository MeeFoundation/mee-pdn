#!/bin/bash
# Act 5. Bob grants Alice 2 claims, one read-only and one writable, over the
# entries 00-start.sh seeded. His granting is watched on the browser screen
# driving his node; on the phone, the card What this peer shares with me
# fills itself, the writable claim carrying a write field and the read-only
# one carrying none.
source "$(dirname "$0")/lib.sh"; need_nodes
A=$(ident alice); B=$(ident bob)
pdnbrowser bob

curl -s -X POST "$BOB/debug/identities/$B/grants/$A" \
  -H 'content-type: application/json' \
  -d "{\"issuer\":\"$B\",\"claims\":[{\"path\":\"contact/email\",\"write\":false},{\"path\":\"notes/shared\",\"write\":true}]}" \
  -w 'HTTP %{http_code}\n'
curl -s "$BOB/debug/identities/$B/own-grants/$A" | jq
echo "the claims stand as hashes: a claim identity is derived one way from the"
echo "issuer and the path, and the path does not come back out of it."
echo
echo "on the phone: Connections -> Bob -> under notes/shared in What this peer"
echo "shares with me, type into write a new value and press return."
pdnwait 'curl -sf $BOB/debug/data/$B/notes/shared | grep -v "the first line, written by Bob"' \
  && echo "^ that came from the phone, into Bob's own entry"
echo
echo "contact/email is read-only, so that row on the phone carries no write"
echo "field at all — there is nothing to tap, and this value stays unchanged:"
curl -s "$BOB/debug/data/$B/contact/email"; echo
