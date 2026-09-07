#!/bin/bash
# Act 1. Two independent nodes: this node's id and its own identities.
# On the phone: Bring the node up -> Create an identity.
source "$(dirname "$0")/lib.sh"; need_node

curl -s "$MAC/debug/status"
curl -s "$MAC/debug/identities" | jq
