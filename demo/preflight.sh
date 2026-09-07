#!/bin/bash
# Pre-flight: the tools are here and the QR path works end to end.
source "$(dirname "$0")/lib.sh"; need_node
MINE=$(mine)

command -v jq >/dev/null && echo "jq is here" || echo "jq is MISSING"
command -v qrencode >/dev/null && echo "qrencode is here" || echo "qrencode is MISSING"

curl -s -X POST "$MAC/debug/identities/$MINE/invite?lifetime_secs=60" > "$PDN/tmp/probe.json"
base64 < "$PDN/tmp/probe.json" | tr -d '\n' | tr '+/' '-_' | tr -d '=' | qrencode -l L -s 6 -o "$PDN/tmp/probe.png"
if diff -q "$PDN/tmp/probe.json" <(swift "$PDN/tmp/qrdecode.swift" "$PDN/tmp/probe.png" 2>/dev/null | pdnraw) >/dev/null; then
  echo "the QR reads back into the invite it was made from"
else
  echo "the QR path is BROKEN"
fi
rm -f "$PDN/tmp/probe.json" "$PDN/tmp/probe.png"

echo "the addresses the node puts into an invite:"
curl -s -X POST "$MAC/debug/identities/$MINE/invite" | jq -c '[.inviter_addr.addrs[]|keys[0]]'
echo "expect Relay and three Ip; without Relay the reach ends at one network"
