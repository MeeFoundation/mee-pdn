#!/bin/bash
# Act 7. The right to write: the laptop grants a writable claim and waits for the phone.
# On the phone: under the writable claim, the write a new value field.
source "$(dirname "$0")/lib.sh"; need_node
MINE=$(mine); PEER=$(peer)

before='the first line, from the laptop'
curl -s -X PUT "$MAC/debug/data/$MINE/notes/shared" --data-binary "$before"

# Act 5's read-only grant named this very namespace, and a binder that
# already holds the replica keeps the read capability it imported unless the
# grant is withdrawn first. Without this the phone shows the write field and
# has nothing to sign the entry with.
curl -s -o /dev/null -X DELETE "$MAC/debug/identities/$MINE/grants/$PEER/$MINE"
sleep 10
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$MAC/debug/identities/$MINE/grants/$PEER" \
  -H 'content-type: application/json' \
  -d "{\"issuer\":\"$MINE\",\"claims\":[{\"path\":\"contact/email\",\"write\":false},{\"path\":\"notes/shared\",\"write\":true}]}")

if [ "$code" = "204" ]; then
  echo "published: contact/email read-only, notes/shared writable, both under $MINE"
  curl -s "$MAC/debug/identities/$MINE/own-grants/$PEER" | jq -c '.grant.claims'
else
  echo "the publication was refused with HTTP $code" >&2
  exit 1
fi

echo
echo "On the phone this takes about ten seconds to arrive. Open Connections ->"
echo "the laptop's row: notes/shared carries a write a new value field, and"
echo "contact/email has none. If the card is empty, leave the row and come back."
echo
echo "write a value on the phone; waiting for it in the laptop's own data"
echo "(the line below is what the phone wrote, read out of the laptop's store)"
pdnwait "curl -sf \$MAC/debug/data/\$MINE/notes/shared | grep -v '^$before\$'"
