#!/bin/bash
# Act 9. A restart: a node comes back on its directory as itself.
# On the phone: close the application with a swipe, open it, Bring the node up.
source "$(dirname "$0")/lib.sh"

before=$(curl -s "$MAC/debug/status" | head -1)
pid_before=$(pgrep -f 'target/debug/pdn-node-http' | head -1)
"$(dirname "$0")/00-start.sh" >/dev/null || exit 1
after=$(curl -s "$MAC/debug/status" | head -1)
pid_after=$(pgrep -f 'target/debug/pdn-node-http' | head -1)

echo "the process is a different one:  pid $pid_before -> pid $pid_after"
echo "the node is the same one:        $before"
[ "$before" = "$after" ] || echo "the node id changed after the restart: $after" >&2
echo "the identity is the same one:    $(mine)"
echo "the connection is the same one:  $(peer)"
echo
echo "only work in flight does not come back: a minted and unconsumed invite,"
echo "an interrupted ceremony. The home relay is taken again in about ten seconds."
