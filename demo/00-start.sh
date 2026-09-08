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
  echo "=== $n ($(url_of "$n")) ==="
  curl -s "$(url_of "$n")/debug/status"
done

echo
echo "Alice's identity, the one her phone joins: $(ident alice)"
echo "Alice's other identity, which it does not:  $(alice_other)"
echo "Bob:   $(ident bob)"
echo "Carol: $(ident carol)"
echo
echo "logs: tail -f $PDN/tmp/pdn-alice.log"
