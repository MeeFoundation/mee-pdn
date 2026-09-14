#!/bin/bash
# Act 7. A grant belongs to the identity Bob named as audience, not to the
# device that accepted it: Alice's laptop node, which took no part in either
# grant in act 5 or 6, already reads what Bob shares, and picks up a change he
# makes to it on its own. Nothing happens on the phone.
source "$(dirname "$0")/lib.sh"; need_nodes
A=$(ident alice); B=$(ident bob)
pdnbrowser alice

echo "=== Alice's laptop reads what Bob granted, with no phone involved at all ==="
curl -s -w ' [HTTP %{http_code}]\n' "$ALICE/debug/data/$B/contact/email"

# The time is in the value: a second run with the same text is indistinguishable from the first.
NEW="written by Bob at $(date +%H:%M:%S)"
curl -s -X PUT "$BOB/debug/data/$B/contact/email" --data-binary "$NEW" >/dev/null
echo "Bob changed it to: $NEW"
pdnwait "curl -sf \$ALICE/debug/data/\$B/contact/email | grep -F '$NEW'" \
  && echo "^ Alice's laptop picked up the change on its own"
