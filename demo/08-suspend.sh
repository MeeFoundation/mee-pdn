#!/bin/bash
# Act 8. The application is put away: the laptop writes while the phone is "gone".
# On the phone: the home gesture (do not lock the screen), then come back.
source "$(dirname "$0")/lib.sh"; need_node
MINE=$(mine)

# A value that differs on every run: a repeat with the same text shows the
# audience nothing, because the phone's card looks exactly as it did before.
value="changed while the phone slept, $(date +%H:%M:%S)"
curl -s -X PUT "$MAC/debug/data/$MINE/contact/email" --data-binary "$value"
echo "the laptop wrote to contact/email: $value"
echo "it reads back on the laptop as: $(curl -sf "$MAC/debug/data/$MINE/contact/email")"
echo
echo "Now come back into the application: What this peer shares with me ->"
echo "contact/email carries this line, with no bring-up and no reconnect."
