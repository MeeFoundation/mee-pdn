#!/bin/bash
# Act 2. Your data stays with you: the laptop writes an entry and lists them.
# On the phone: My entries -> a path and a value -> Write -> Read it.
source "$(dirname "$0")/lib.sh"; need_node
MINE=$(mine)

curl -s -X PUT "$MAC/debug/data/$MINE/contact/email" --data-binary 'laptop@example.com'
curl -s "$MAC/debug/data/$MINE" | jq
