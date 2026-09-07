#!/bin/bash
# Act 4. The phone shares with the laptop: wait for the grant, read the value.
# On the phone: Share claims with this peer -> tap a path -> Grant read-only.
source "$(dirname "$0")/lib.sh"; need_node
MINE=$(mine); PEER=$(peer)

echo "=== the grant the phone published ==="
pdnwait 'curl -s $MAC/debug/identities/$MINE/grants/$PEER | jq -ce "select(.grants|length>0)"' | jq || exit 1

echo "=== what arrived under that issuer ==="
curl -s "$MAC/debug/data/$PEER" | jq

# The path comes from what actually arrived, not from what the script remembers.
path=$(curl -s "$MAC/debug/data/$PEER" | jq -r '.entries[0].path // empty')
if [ -n "$path" ]; then
  echo "=== the value of the claim $path ==="
  pdnwait "curl -sf \$MAC/debug/data/\$PEER/$path"
  # A read answers with what is here now. A value the phone has just changed
  # takes about ten seconds to arrive, and until it does the old one reads
  # perfectly well — which looks like a failure and is not one.
  echo "(this is the value as of now; one just changed on the phone takes about"
  echo " ten seconds to arrive — run this script again to see it)"
fi

echo "=== a path outside the grant ==="
curl -s -w ' [HTTP %{http_code}]\n' "$MAC/debug/data/$PEER/notes/private"
echo "404 here means 'no such entry', not a refusal: outside a claim nothing"
echo "replicates here at all. Before the grant the answer was 409, namespace not bound."
