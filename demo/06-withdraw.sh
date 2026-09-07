#!/bin/bash
# Act 6. The phone withdrew its grant, then published it again.
# On the phone: Withdraw this grant first, then Grant read-only again.
source "$(dirname "$0")/lib.sh"; need_node
MINE=$(mine); PEER=$(peer)
path=${1:-name}

# Every stage says what was read, in words. A raw body and a status code are
# evidence, not a conclusion, and on a stage nobody reads JSON out loud.
read_value() { curl -s -w '|%{http_code}' "$MAC/debug/data/$PEER/$path"; }

echo "=== before anything: what this node holds ==="
n=$(curl -s "$MAC/debug/identities/$MINE/grants/$PEER" | jq '.grants|length')
out=$(read_value); code=${out##*|}; body=${out%|*}
echo "read: $n grant(s) from the phone, and $path answers HTTP $code"
[ "$code" = "200" ] && echo "read: the value is \"$body\""

echo
echo "withdraw the grant on the phone now"
if pdnwait 'curl -s $MAC/debug/identities/$MINE/grants/$PEER | jq -ce "select((.grants|length)==0)"' >/dev/null; then
  out=$(read_value); code=${out##*|}
  echo "read: no grant is published toward this node any more, and $path answers HTTP $code"
  if [ "$code" = "409" ]; then
    echo "read: 409 is the namespace unbound — exactly what this node answered before the two ever met"
  else
    echo "read: expected 409 (namespace unbound); got $code"
  fi
else
  echo "read: the grant is still here — either it was not withdrawn, or nothing is reaching this node"
  exit 1
fi

echo
echo "now grant it again on the phone"
if value=$(pdnwait "curl -sf \$MAC/debug/data/\$PEER/$path"); then
  echo "read: the value is back, \"$value\""
  echo
  echo "A withdrawal closes further delivery and does not recall what was already"
  echo "delivered. Granting again opens the same claim back up."
else
  echo "read: nothing came back within 48 seconds."
  echo "Either the grant was not published again, or the two nodes are not talking."
  echo "The log tells them apart — a run of these means the transport is down:"
  grep -c "sync failed" "$NODE_LOG" | sed 's/^/  sync failures so far: /'
  tail -3 "$NODE_LOG" | sed 's/\x1b\[[0-9;]*m//g' | cut -c1-160 | sed 's/^/  /'
  exit 1
fi
