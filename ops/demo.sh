#!/bin/sh
# The demo's narration: one person with two personas, two counterparties, a
# phone and a laptop each. Every step says what it is doing before it does
# it, and the nodes are driven over HTTP alone — what passes between them is
# the runtimes' own traffic, and a ceremony payload travels through here the
# way a code travels between two screens through a person.
#
# Reads the seven base URLs from the environment so the same narration can be
# pointed at containers, at processes, or at anything else that serves the
# surface.
set -eu

# The restart step acts on one container through the compose project; a
# narration pointed at something else — bare processes, say — sets
# `DEMO_COMPOSE=none` and the step is skipped.
DEMO_COMPOSE=${DEMO_COMPOSE:-docker compose -f ops/compose.yml}

# How a node's base URL is asked for again once its container has restarted,
# as a command taking the service name. Empty where the URLs outlive the
# containers behind them: a published port does, a container's own address
# on the network does not.
DEMO_RESOLVE=${DEMO_RESOLVE:-}

ALICE_PHONE=${ALICE_PHONE:-http://127.0.0.1:3011}
ALICE_WORK_LAPTOP=${ALICE_WORK_LAPTOP:-http://127.0.0.1:3012}
ALICE_LEISURE_LAPTOP=${ALICE_LEISURE_LAPTOP:-http://127.0.0.1:3013}
BOB_PHONE=${BOB_PHONE:-http://127.0.0.1:3014}
BOB_LAPTOP=${BOB_LAPTOP:-http://127.0.0.1:3015}
CAROL_PHONE=${CAROL_PHONE:-http://127.0.0.1:3016}
CAROL_LAPTOP=${CAROL_LAPTOP:-http://127.0.0.1:3017}

# Colour is for the room, so it follows the terminal rather than the script:
# off when the output is a file or a pipeline, on when a person is watching.
# `DEMO_COLOR=always` forces it for a recording, `never` for a transcript.
case "${DEMO_COLOR:-auto}" in
  always) tint=1 ;;
  never) tint=0 ;;
  *) [ -t 1 ] && tint=1 || tint=0 ;;
esac
if [ "$tint" = 1 ]; then
  STEP=$(printf '\033[1;36m'); FACT=$(printf '\033[2m')
  VALUE=$(printf '\033[32m'); OFF=$(printf '\033[0m')
else
  STEP=; FACT=; VALUE=; OFF=
fi

say() { printf '\n%s%s%s\n' "$STEP" "$*" "$OFF"; }

# One observed fact under a step: the sentence dim, whatever was actually
# read in colour, so a room can follow the values without reading the prose.
fact() { printf '  %s%s%s\n' "$FACT" "$*" "$OFF"; }
shown() { printf '  %s%s%s%s%s\n' "$FACT" "$1" "$OFF$VALUE" "$2" "$OFF"; }

# Every node answers liveness before the first step, so a slow start is not
# mistaken for a broken one later.
wait_live() {
  for _ in $(seq 1 200); do
    # A ceiling on one probe, without which an address nothing answers on
    # holds each attempt open for minutes and the budget above means nothing.
    curl -sf -m 2 "$1/live" >/dev/null 2>&1 && return 0
    sleep 0.25
  done
  echo "no answer from $1" >&2
  exit 1
}

# Poll a read until it carries the expected bytes. Repeating the read is the
# only wait the surface offers — nothing here forces a reconciliation.
reads() { # url issuer path expected label
  for _ in $(seq 1 240); do
    if [ "$(curl -s "$1/debug/data/$2/$3")" = "$4" ]; then
      shown "$5 sees " "$4"
      return 0
    fi
    sleep 0.5
  done
  echo "  $5 never saw \"$4\"" >&2
  exit 1
}

new_identity() { curl -s -X POST "$1/debug/identities" | sed -E 's/.*"identity":"([^"]+)".*/\1/'; }

printf '\n'

for url in "$ALICE_PHONE" "$ALICE_WORK_LAPTOP" "$ALICE_LEISURE_LAPTOP" \
           "$BOB_PHONE" "$BOB_LAPTOP" "$CAROL_PHONE" "$CAROL_LAPTOP"; do
  wait_live "$url"
done

say "Alice keeps two personas on one phone: one for work, one for leisure."
AT_WORK=$(new_identity "$ALICE_PHONE")
AT_LEISURE=$(new_identity "$ALICE_PHONE")
fact "Alice at work    $AT_WORK"
fact "Alice at leisure $AT_LEISURE"

say "Bob and Carol each start on their own phone."
BOB=$(new_identity "$BOB_PHONE")
CAROL=$(new_identity "$CAROL_PHONE")
fact "Bob   $BOB"
fact "Carol $CAROL"

say "Alice at work and Bob meet, phone to phone."
curl -s -X POST "$ALICE_PHONE/debug/identities/$AT_WORK/invite?lifetime_secs=300" -o /tmp/pdn-demo-invite
curl -s -X POST "$BOB_PHONE/debug/identities/$BOB/establish" --data-binary @/tmp/pdn-demo-invite -o /dev/null
fact "Bob is now a connection of Alice at work"

say "Alice at leisure and Carol meet, phone to phone."
curl -s -X POST "$ALICE_PHONE/debug/identities/$AT_LEISURE/invite?lifetime_secs=300" -o /tmp/pdn-demo-invite
curl -s -X POST "$CAROL_PHONE/debug/identities/$CAROL/establish" --data-binary @/tmp/pdn-demo-invite -o /dev/null
fact "Carol is now a connection of Alice at leisure"

say "Everyone adds a laptop. A device belongs to an identity, not to a person, so Alice adds one per persona."
link_laptop() { # identity from to label
  curl -s -X POST "$2/debug/identities/$1/linking-invite" -o /tmp/pdn-demo-link
  curl -s -X POST "$3/debug/link?timeout_secs=120" --data-binary @/tmp/pdn-demo-link -o /dev/null
  fact "$4"
}
link_laptop "$AT_WORK" "$ALICE_PHONE" "$ALICE_WORK_LAPTOP" "Alice's work laptop joined Alice at work"
link_laptop "$AT_LEISURE" "$ALICE_PHONE" "$ALICE_LEISURE_LAPTOP" "Alice's leisure laptop joined Alice at leisure"
link_laptop "$BOB" "$BOB_PHONE" "$BOB_LAPTOP" "Bob's laptop joined"
link_laptop "$CAROL" "$CAROL_PHONE" "$CAROL_LAPTOP" "Carol's laptop joined"

say "Each laptop hosts exactly the persona it joined — asked of the laptop itself."
hosts() { curl -s "$1/debug/identities" | grep -q "$2" && shown "$3 hosts " "$4"; }
hosts "$ALICE_WORK_LAPTOP" "$AT_WORK" "Alice's work laptop" "Alice at work"
hosts "$ALICE_LEISURE_LAPTOP" "$AT_LEISURE" "Alice's leisure laptop" "Alice at leisure"

say "Alice writes her work address and her club address, each under its own persona."
curl -s -X PUT "$ALICE_PHONE/debug/data/$AT_WORK/contact/email" --data-binary 'alice@acme.example' -o /dev/null
curl -s -X PUT "$ALICE_PHONE/debug/data/$AT_LEISURE/contact/email" --data-binary 'alice@bridgeclub.example' -o /dev/null
fact "the same path under two personas, holding different data"

say "Alice grants Bob and Carol read and write on exactly that one field."
curl -s -X POST "$ALICE_PHONE/debug/identities/$AT_WORK/grants/$BOB" \
  -d "{\"issuer\":\"$AT_WORK\",\"claims\":[{\"path\":\"contact/email\",\"write\":true}]}" -o /dev/null
curl -s -X POST "$ALICE_PHONE/debug/identities/$AT_LEISURE/grants/$CAROL" \
  -d "{\"issuer\":\"$AT_LEISURE\",\"claims\":[{\"path\":\"contact/email\",\"write\":true}]}" -o /dev/null
fact "the grant names a path, and nothing else of the namespace travels"

say "Reading: the data reaches Bob and Carol, on the phone that met Alice and on the laptop that joined later."
reads "$BOB_PHONE" "$AT_WORK" "contact/email" "alice@acme.example" "Bob's phone"
reads "$BOB_LAPTOP" "$AT_WORK" "contact/email" "alice@acme.example" "Bob's laptop"
reads "$CAROL_PHONE" "$AT_LEISURE" "contact/email" "alice@bridgeclub.example" "Carol's phone"
reads "$CAROL_LAPTOP" "$AT_LEISURE" "contact/email" "alice@bridgeclub.example" "Carol's laptop"

say "Writing: Bob corrects the address from his laptop, Carol from hers."
curl -s -X PUT "$BOB_LAPTOP/debug/data/$AT_WORK/contact/email" --data-binary 'alice@acme.example (desk 4)' -o /dev/null
curl -s -X PUT "$CAROL_LAPTOP/debug/data/$AT_LEISURE/contact/email" --data-binary 'alice@bridgeclub.example (tuesdays)' -o /dev/null
fact "both wrote into a namespace that is not theirs, under the grant that named it writable"

say "The correction comes back to Alice — to the phone that issued the grant and to the laptop that joined afterwards."
reads "$ALICE_PHONE" "$AT_WORK" "contact/email" "alice@acme.example (desk 4)" "Alice's phone, at work,"
reads "$ALICE_WORK_LAPTOP" "$AT_WORK" "contact/email" "alice@acme.example (desk 4)" "Alice's work laptop"
reads "$ALICE_PHONE" "$AT_LEISURE" "contact/email" "alice@bridgeclub.example (tuesdays)" "Alice's phone, at leisure,"
reads "$ALICE_LEISURE_LAPTOP" "$AT_LEISURE" "contact/email" "alice@bridgeclub.example (tuesdays)" "Alice's leisure laptop"

if [ "$DEMO_COMPOSE" != "none" ]; then
  say "Bob's laptop is stopped — and started again. Its state is on disk, so what comes back is the same device."
  node_id() { curl -s "$1/debug/status" | sed -n 's/^node //p'; }
  BEFORE=$(node_id "$BOB_LAPTOP")
  $DEMO_COMPOSE stop bob-laptop >/dev/null 2>&1
  $DEMO_COMPOSE start bob-laptop >/dev/null 2>&1
  [ -z "$DEMO_RESOLVE" ] || BOB_LAPTOP=$($DEMO_RESOLVE bob-laptop)
  wait_live "$BOB_LAPTOP"
  AFTER=$(node_id "$BOB_LAPTOP")
  if [ -z "$BEFORE" ] || [ "$BEFORE" != "$AFTER" ]; then
    echo "  Bob's laptop came back as a different node ($BEFORE -> $AFTER)" >&2
    exit 1
  fi
  shown "the laptop's node id is unchanged: " "$AFTER"
  hosts "$BOB_LAPTOP" "$BOB" "Bob's laptop still" "Bob"

  say "Alice updates the address once more. The returned laptop converges — its connection still stands, and nothing was established a second time."
  curl -s -X PUT "$ALICE_PHONE/debug/data/$AT_WORK/contact/email" --data-binary 'alice@acme.example (desk 5)' -o /dev/null
  reads "$BOB_LAPTOP" "$AT_WORK" "contact/email" "alice@acme.example (desk 5)" "Bob's returned laptop"
fi

say "Two personas on one phone with a laptop each, Bob and Carol on theirs, seven devices in all — and each side reads and writes only what the other named."

printf '\n'
