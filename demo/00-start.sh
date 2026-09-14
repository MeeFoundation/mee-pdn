#!/bin/bash
# Before the audience. Bring up the three nodes that are not phones and name
# what each one holds: Alice's laptop node with her 2 identities, Bob's node,
# and Carol's, which holds nothing of Alice.
source "$(dirname "$0")/lib.sh"

[ -x "$NODE_BIN" ] || { echo "building pdn-node-http"; (cd "$PDN" && cargo build -p pdn-node-http) || exit 1; }
command -v jq >/dev/null || { echo "jq is required: brew install jq" >&2; exit 1; }
command -v qrencode >/dev/null || { echo "qrencode is required: brew install qrencode" >&2; exit 1; }

# A previous node has to release its directory lock. It does not always take a
# term signal: one seen here ignored SIGTERM past twenty seconds, and the start
# that follows then fails with the directory held by another node.
pkill -f 'target/debug/pdn-node-http'
for i in $(seq 1 20); do pgrep -f 'target/debug/pdn-node-http' >/dev/null || break; sleep 1; done
if pgrep -f 'target/debug/pdn-node-http' >/dev/null; then
  echo "a previous node ignored the term signal for 20 seconds; killing it"
  pkill -9 -f 'target/debug/pdn-node-http'
  sleep 2
fi

for n in $NODES; do start_node "$n"; done
for n in $NODES; do
  for i in $(seq 1 20); do node_up "$n" && break; sleep 1; done
  if ! node_up "$n"; then
    echo "$n did not come up, here is its log:" >&2
    tail -20 "$(log_of "$n")" >&2
    exit 1
  fi
done

for n in $NODES; do
  # port, not the full http:// address: that address serves no page at "/",
  # and a bare http:// label reads as a browser link to click, which this
  # isn't — it is only a tag for the status dump that follows.
  echo "=== $n (port $(port_of "$n")) ==="
  curl -s "$(url_of "$n")/debug/status"
done

A=$(ident alice); OTHER=$(alice_other); B=$(ident bob); ident carol >/dev/null

# Base data, seeded before the audience so acts 1 and 5 only display and grant
# rather than also typing it live. The phone joins Alice's identity already
# holding entries — task 5.10's own premise — and act 1 shows that as true.
curl -s -X PUT "$ALICE/debug/data/$A/contact/email" --data-binary 'alice@example.org' >/dev/null
curl -s -X PUT "$ALICE/debug/data/$A/contact/phone" --data-binary '+31 6 1234 5678' >/dev/null
curl -s -X PUT "$ALICE/debug/data/$A/notes/private" --data-binary 'not for anyone' >/dev/null
curl -s -X PUT "$ALICE/debug/data/$OTHER/contact/email" --data-binary 'alice@work.example' >/dev/null
curl -s -X PUT "$BOB/debug/data/$B/contact/email" --data-binary 'bob@example.org' >/dev/null
curl -s -X PUT "$BOB/debug/data/$B/notes/shared" --data-binary 'the first line, written by Bob' >/dev/null

echo
echo "Alice's identity, the one her phone joins: $A"
echo "Alice's other identity, which it does not:  $OTHER"
echo "Bob:   $B"
echo "Carol: $(ident carol)"
echo
echo "each node's screens, the same ones the phone runs — start the page once"
echo "with: cd $PDN/pdn-app && npm run web, then open whichever node you need:"
echo "  alice: $(browser_url alice)"
echo "  bob:   $(browser_url bob)"
echo "  carol: $(browser_url carol)"
echo "the acts also switch to the node they need on their own via pdnbrowser."
echo
echo "logs: tail -f $PDN/tmp/pdn-alice.log"
