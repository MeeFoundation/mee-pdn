#!/bin/bash
# Bring the laptop's node up on its directory and name its identity.
# The directory survives a restart: the identity and the connections come back the same.
source "$(dirname "$0")/lib.sh"

[ -x "$NODE_BIN" ] || { echo "building pdn-node-http"; (cd "$PDN" && cargo build -p pdn-node-http) || exit 1; }
command -v jq >/dev/null || { echo "jq is required: brew install jq" >&2; exit 1; }
command -v qrencode >/dev/null || { echo "qrencode is required: brew install qrencode" >&2; exit 1; }

# The previous node has to release the directory lock. It does not always take
# a term signal: one seen here ignored SIGTERM past twenty seconds, and the
# start that follows then fails with the directory held by another node.
pkill -f 'target/debug/pdn-node-http'
for i in $(seq 1 20); do pgrep -f 'target/debug/pdn-node-http' >/dev/null || break; sleep 1; done
if pgrep -f 'target/debug/pdn-node-http' >/dev/null; then
  echo "the previous node ignored the term signal for 20 seconds; killing it"
  pkill -9 -f 'target/debug/pdn-node-http'
  sleep 2
fi

mkdir -p "$NODE_DIR"
PDN_DATA_DIR="$NODE_DIR" PDN_DEBUG=1 nohup "$NODE_BIN" > "$NODE_LOG" 2>&1 &
disown

for i in $(seq 1 20); do node_up && break; sleep 1; done
if ! node_up; then
  echo "the node did not come up, here is its log:" >&2
  tail -20 "$NODE_LOG" >&2
  exit 1
fi

curl -s "$MAC/debug/status"
echo "the laptop's identity: $(mine)"
echo
echo "log: tail -f $NODE_LOG"
