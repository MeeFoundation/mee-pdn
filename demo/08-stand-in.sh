#!/bin/bash
# Act 8. The phone established the connection and published the grant. With it
# in airplane mode, Alice's laptop node carries a fresh value to Bob: what a
# peer can reach does not depend on the device that granted it.
# On the phone: airplane mode, application still in view. Never the lock
# screen — on iOS a lock can end the process and measure something else.
source "$(dirname "$0")/lib.sh"; need_nodes
A=$(ident alice); B=$(ident bob)
pdnbrowser bob

echo "put the phone in airplane mode now, then press enter"; read -r _
# The time is in the value: a second run with the same text on Bob's side is
# indistinguishable from the first.
NEW="written by the laptop while the phone was away, $(date +%H:%M:%S)"
curl -s -X PUT "$ALICE/debug/data/$A/contact/email" --data-binary "$NEW" >/dev/null
echo "the laptop wrote: $NEW"
pdnwait "curl -sf \$BOB/debug/data/\$A/contact/email | grep -F '$NEW'" \
  && echo "^ Bob read it from Alice's remaining device"
echo
echo "take the phone out of airplane mode; it catches up on its own."
