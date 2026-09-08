#!/bin/bash
# Pre-flight, before the audience. The tools are here, the QR path works end to
# end, and every node publishes a relay address — without one the reach ends at
# the network the laptop is on, and the phone is usually not on it.
source "$(dirname "$0")/lib.sh"; need_nodes

command -v jq >/dev/null && echo "jq is here" || echo "jq is MISSING"
command -v qrencode >/dev/null && echo "qrencode is here" || echo "qrencode is MISSING"

A=$(ident alice)
curl -s -X POST "$ALICE/debug/identities/$A/invite?lifetime_secs=60" > "$PDN/tmp/probe.json"
base64 < "$PDN/tmp/probe.json" | tr -d '\n' | tr '+/' '-_' | tr -d '=' | qrencode -l L -s 6 -o "$PDN/tmp/probe.png"
if diff -q "$PDN/tmp/probe.json" <(swift "$(dirname "$0")/qrdecode.swift" "$PDN/tmp/probe.png" 2>/dev/null | pdnraw) >/dev/null; then
  echo "the QR reads back into the invite it was made from"
else
  echo "the QR path is BROKEN"
fi
rm -f "$PDN/tmp/probe.json" "$PDN/tmp/probe.png"

for n in $NODES; do
  id=$(ident "$n")
  addrs=$(curl -s -X POST "$(url_of "$n")/debug/identities/$id/invite" | jq -c '[.inviter_addr.addrs[]|keys[0]]')
  echo "$n publishes: $addrs"
done
echo "expect Relay and a few Ip in each. The nodes are spawned with"
echo "PDN_CONNECTIVITY=product: addresses are published under the node id to"
echo "n0's name servers, and a session that finds no direct path is carried"
echo "by n0's relay. Without it the host binds direct paths and the phone,"
echo "which is not on this network, is unreachable."
