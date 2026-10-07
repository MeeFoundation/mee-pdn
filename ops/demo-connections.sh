#!/bin/sh
# The connections demo's narration: one person with two personas, two
# counterparties, a phone and a laptop each. Every step says what it is doing
# before it does it, and the nodes are driven over HTTP alone — what passes
# between them is the runtimes' own traffic, and a ceremony payload travels
# through here the way a code travels between two screens through a person.
#
# Reads the seven base URLs from the environment so the same narration can be
# pointed at containers, at processes, or at anything else that serves the
# surface.
set -eu

# The restart step acts on one container through the compose project; a
# narration pointed at something else — bare processes, say — sets
# `DEMO_COMPOSE=none` and the step is skipped.
DEMO_COMPOSE=${DEMO_COMPOSE:-docker compose -f ops/compose-connections.yml}

ALICE_PHONE=${ALICE_PHONE:-http://127.0.0.1:3011}
ALICE_WORK_LAPTOP=${ALICE_WORK_LAPTOP:-http://127.0.0.1:3012}
ALICE_LEISURE_LAPTOP=${ALICE_LEISURE_LAPTOP:-http://127.0.0.1:3013}
BOB_PHONE=${BOB_PHONE:-http://127.0.0.1:3014}
BOB_LAPTOP=${BOB_LAPTOP:-http://127.0.0.1:3015}
CAROL_PHONE=${CAROL_PHONE:-http://127.0.0.1:3016}
CAROL_LAPTOP=${CAROL_LAPTOP:-http://127.0.0.1:3017}

# shellcheck source-path=SCRIPTDIR source=demo-common.sh
. "$(dirname "$0")/demo-common.sh"

# Poll a read until it carries the expected bytes. Repeating the read is the
# only wait the surface offers — nothing here forces a reconciliation. An
# entry not there yet (404) is waited on; any other answer ends the show.
reads() { # url reader issuer path expected label
  for _ in $(seq 1 240); do
    request -m 2 "$1/debug/data/$2/$3/$4"
    case "$code" in
      200) if [ "$body" = "$5" ]; then shown "$6 sees " "$5"; return 0; fi ;;
      404) ;;
      # Unbound until the grant record reaches the reader and its binder runs.
      409) ;;
      000) echo "  $6's read: no answer" >&2; exit 1 ;;
      *) echo "  $6's read answered $code: $body" >&2; exit 1 ;;
    esac
    sleep 0.5
  done
  echo "  $6 never saw \"$5\"" >&2
  exit 1
}

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
WORK_INVITE=$(call POST "$ALICE_PHONE/debug/identities/$AT_WORK/invite?lifetime_secs=300")
call POST "$BOB_PHONE/debug/identities/$BOB/establish" --data-raw "$WORK_INVITE" >/dev/null
fact "Bob is now a connection of Alice at work"

say "Alice at leisure and Carol meet, phone to phone."
LEISURE_INVITE=$(call POST "$ALICE_PHONE/debug/identities/$AT_LEISURE/invite?lifetime_secs=300")
call POST "$CAROL_PHONE/debug/identities/$CAROL/establish" --data-raw "$LEISURE_INVITE" >/dev/null
fact "Carol is now a connection of Alice at leisure"

say "Everyone adds a laptop. A device belongs to an identity, not to a person, so Alice adds one per persona."
link_device "$AT_WORK" "$ALICE_PHONE" "$ALICE_WORK_LAPTOP" "Alice's work laptop joined Alice at work"
link_device "$AT_LEISURE" "$ALICE_PHONE" "$ALICE_LEISURE_LAPTOP" "Alice's leisure laptop joined Alice at leisure"
link_device "$BOB" "$BOB_PHONE" "$BOB_LAPTOP" "Bob's laptop joined"
link_device "$CAROL" "$CAROL_PHONE" "$CAROL_LAPTOP" "Carol's laptop joined"

say "Each laptop hosts exactly the persona it joined — asked of the laptop itself."
hosts "$ALICE_WORK_LAPTOP" "$AT_WORK" "Alice's work laptop" "Alice at work"
hosts "$ALICE_LEISURE_LAPTOP" "$AT_LEISURE" "Alice's leisure laptop" "Alice at leisure"

say "Alice writes her work address and her club address, each under its own persona."
call PUT "$ALICE_PHONE/debug/data/$AT_WORK/$AT_WORK/contact/email" --data-raw 'alice@acme.example' >/dev/null
call PUT "$ALICE_PHONE/debug/data/$AT_LEISURE/$AT_LEISURE/contact/email" --data-raw 'alice@bridgeclub.example' >/dev/null
fact "the same path under two personas, holding different data"

say "Alice grants Bob and Carol read and write on exactly that one field."
call POST "$ALICE_PHONE/debug/identities/$AT_WORK/grants/$BOB" \
  --data-raw "{\"issuer\":\"$AT_WORK\",\"claims\":[{\"path\":\"contact/email\",\"write\":true}]}" >/dev/null
call POST "$ALICE_PHONE/debug/identities/$AT_LEISURE/grants/$CAROL" \
  --data-raw "{\"issuer\":\"$AT_LEISURE\",\"claims\":[{\"path\":\"contact/email\",\"write\":true}]}" >/dev/null
fact "the grant names a path, and nothing else of the namespace travels"

say "Reading: the data reaches Bob and Carol, on the phone that met Alice and on the laptop that joined later."
reads "$BOB_PHONE" "$BOB" "$AT_WORK" "contact/email" "alice@acme.example" "Bob's phone"
reads "$BOB_LAPTOP" "$BOB" "$AT_WORK" "contact/email" "alice@acme.example" "Bob's laptop"
reads "$CAROL_PHONE" "$CAROL" "$AT_LEISURE" "contact/email" "alice@bridgeclub.example" "Carol's phone"
reads "$CAROL_LAPTOP" "$CAROL" "$AT_LEISURE" "contact/email" "alice@bridgeclub.example" "Carol's laptop"

say "Writing: Bob corrects the address from his laptop, Carol from hers."
call PUT "$BOB_LAPTOP/debug/data/$BOB/$AT_WORK/contact/email" --data-raw 'alice@acme.example (desk 4)' >/dev/null
call PUT "$CAROL_LAPTOP/debug/data/$CAROL/$AT_LEISURE/contact/email" --data-raw 'alice@bridgeclub.example (tuesdays)' >/dev/null
fact "both wrote into a namespace that is not theirs, under the grant that named it writable"

say "The correction comes back to Alice — to the phone that issued the grant and to the laptop that joined afterwards."
reads "$ALICE_PHONE" "$AT_WORK" "$AT_WORK" "contact/email" "alice@acme.example (desk 4)" "Alice's phone, at work,"
reads "$ALICE_WORK_LAPTOP" "$AT_WORK" "$AT_WORK" "contact/email" "alice@acme.example (desk 4)" "Alice's work laptop"
reads "$ALICE_PHONE" "$AT_LEISURE" "$AT_LEISURE" "contact/email" "alice@bridgeclub.example (tuesdays)" "Alice's phone, at leisure,"
reads "$ALICE_LEISURE_LAPTOP" "$AT_LEISURE" "$AT_LEISURE" "contact/email" "alice@bridgeclub.example (tuesdays)" "Alice's leisure laptop"

if [ "$DEMO_COMPOSE" != "none" ]; then
  say "Bob's laptop is stopped — and started again. Its state is on disk, so what comes back is the same device."
  BEFORE=$(node_id "$BOB_LAPTOP")
  $DEMO_COMPOSE stop bob-laptop >/dev/null 2>&1
  $DEMO_COMPOSE start bob-laptop >/dev/null 2>&1
  wait_live "$BOB_LAPTOP"
  AFTER=$(node_id "$BOB_LAPTOP")
  if [ -z "$BEFORE" ] || [ "$BEFORE" != "$AFTER" ]; then
    echo "  Bob's laptop came back as a different node ($BEFORE -> $AFTER)" >&2
    exit 1
  fi
  shown "the laptop's node id is unchanged: " "$AFTER"
  hosts "$BOB_LAPTOP" "$BOB" "Bob's laptop still" "Bob"

  say "Alice updates the address once more. The returned laptop converges — its connection still stands, and nothing was established a second time."
  call PUT "$ALICE_PHONE/debug/data/$AT_WORK/$AT_WORK/contact/email" --data-raw 'alice@acme.example (desk 5)' >/dev/null
  reads "$BOB_LAPTOP" "$BOB" "$AT_WORK" "contact/email" "alice@acme.example (desk 5)" "Bob's returned laptop"
fi

say "Two personas on one phone with a laptop each, Bob and Carol on theirs, seven devices in all — and each side reads and writes only what the other named."

printf '\n'
