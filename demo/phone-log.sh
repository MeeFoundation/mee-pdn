#!/bin/bash
# The phone's log. The command launches the application itself and holds its
# stderr: devicectl has no way to attach to one already running, so the
# application restarts and the node inside it has to be brought up again.
source "$(dirname "$0")/lib.sh"
exec xcrun devicectl device process launch --console --device "$DEV" "$BUNDLE"
