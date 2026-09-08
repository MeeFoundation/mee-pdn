#!/bin/bash
# Act 5. Bob grants Alice 2 claims, one read-only and one writable. This is
# the direction with a screen on the receiving end.
# On the phone: the card What this peer shares with me fills itself; the
# writable claim carries a write field and the read-only one carries none.
source "$(dirname "$0")/lib.sh"; need_nodes
A=$(ident alice); B=$(ident bob)

curl -s -X PUT "$BOB/debug/data/$B/contact/email" --data-binary 'bob@example.org' >/dev/null
curl -s -X PUT "$BOB/debug/data/$B/notes/shared" --data-binary 'the first line, written by Bob' >/dev/null
curl -s -X POST "$BOB/debug/identities/$B/grants/$A" \
  -H 'content-type: application/json' \
  -d "{\"issuer\":\"$B\",\"claims\":[{\"path\":\"contact/email\",\"write\":false},{\"path\":\"notes/shared\",\"write\":true}]}" \
  -w 'HTTP %{http_code}\n'
curl -s "$BOB/debug/identities/$B/own-grants/$A" | jq
echo "the claims stand as hashes: a claim identity is derived one way from the"
echo "issuer and the path, and the path does not come back out of it."
echo
echo "now write into the writable claim from the phone. What lands here:"
pdnwait 'curl -sf $BOB/debug/data/$B/notes/shared | grep -v "the first line, written by Bob"' \
  && echo "^ that came from the phone, into Bob's own entry"
echo
echo "then try the read-only claim on the phone: the write is refused there,"
echo "named as what was refused, and this value is unchanged:"
curl -s "$BOB/debug/data/$B/contact/email"; echo
