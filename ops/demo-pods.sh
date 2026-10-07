#!/bin/sh
# The pods demo's narration: Alice, Bob, Carol and Dave in one pod, a
# shopping list every member edits, the owners' acts on membership, the pod
# carrying on — a newcomer, an edit — while every device of its creator is
# offline, and the creator's devices coming back and catching up. The nodes
# are driven over HTTP alone, and an invite travels through here the way a
# code travels between two screens through a person.
#
# Reads the six base URLs from the environment so the same narration can be
# pointed at containers, at processes, or at anything else that serves the
# surface.
set -eu

# Taking Alice's devices offline stops and starts their containers through
# the compose project; a narration pointed at something else sets
# `DEMO_COMPOSE=none`, and her devices stay online throughout.
DEMO_COMPOSE=${DEMO_COMPOSE:-docker compose -f ops/compose-pods.yml}

ALICE_PHONE=${ALICE_PHONE:-http://127.0.0.1:3021}
ALICE_LAPTOP=${ALICE_LAPTOP:-http://127.0.0.1:3022}
BOB_PHONE=${BOB_PHONE:-http://127.0.0.1:3023}
BOB_LAPTOP=${BOB_LAPTOP:-http://127.0.0.1:3024}
CAROL_PHONE=${CAROL_PHONE:-http://127.0.0.1:3025}
DAVE_PHONE=${DAVE_PHONE:-http://127.0.0.1:3026}

# shellcheck source-path=SCRIPTDIR source=demo-common.sh
. "$(dirname "$0")/demo-common.sh"

pod_route() { # url identity pod
  printf '%s/debug/identities/%s/pods/%s' "$1" "$2" "$3"
}

# A record's id from the reference `put_record` answers with.
record_id() {
  sed -E 's/.*"id":"([^"]+)".*/\1/'
}

# An operation's payload as the listing spells it: a JSON array of bytes.
bytes_of() {
  printf '%s' "$1" | od -An -tu1 -v | tr -s ' \n' '\n\n' | grep . | paste -sd, -
}

# The reads below poll as the connections demo's do: repeating the read is
# the only wait the surface offers. 409 is "no member here yet" — a device
# that has not taken the pod in — and 404 a record not arrived yet.

members_are() { # url reader pod label id:owner|plain:name...
  m_url=$1 m_reader=$2 m_pod=$3 m_label=$4
  shift 4
  for _ in $(seq 1 240); do
    request -m 2 "$(pod_route "$m_url" "$m_reader" "$m_pod")/members"
    case "$code" in
      200)
        m_ok=1 m_shown=
        for m in "$@"; do
          m_id=${m%%:*} m_rest=${m#*:}
          m_role=${m_rest%%:*} m_name=${m_rest#*:}
          if [ "$m_role" = owner ]; then m_owner=true m_name="$m_name (owner)"; else m_owner=false; fi
          case "$body" in *"{\"id\":\"$m_id\",\"owner\":$m_owner}"*) ;; *) m_ok=0 ;; esac
          m_shown="${m_shown:+$m_shown, }$m_name"
        done
        m_count=$(printf '%s' "$body" | grep -o '"id"' | wc -l | tr -d ' ')
        if [ "$m_ok" = 1 ] && [ "$m_count" = "$#" ]; then
          shown "$m_label lists " "$m_shown"
          return 0
        fi ;;
      409) ;;
      000) echo "  $m_label's members: no answer" >&2; exit 1 ;;
      *) echo "  $m_label's members answered $code: $body" >&2; exit 1 ;;
    esac
    sleep 0.5
  done
  echo "  $m_label never listed $m_shown; last answer $code: $body" >&2
  exit 1
}

list_reads() { # url reader pod list label writer:text:name...
  o_url=$1 o_reader=$2 o_pod=$3 o_list=$4 o_label=$5
  shift 5
  for _ in $(seq 1 240); do
    request -m 2 "$(pod_route "$o_url" "$o_reader" "$o_pod")/records/$ALICE/mergeable-document/$o_list/ops"
    case "$code" in
      200)
        o_ok=1 o_shown=
        for o in "$@"; do
          o_writer=${o%%:*} o_rest=${o#*:}
          o_text=${o_rest%%:*} o_name=${o_rest#*:}
          printf '%s' "$body" \
            | grep -qE "\"writer\":\"$o_writer\",\"id\":\"[^\"]*\",\"payload\":\[$(bytes_of "$o_text")\]" \
            || o_ok=0
          o_shown="${o_shown:+$o_shown, }$o_text ($o_name)"
        done
        o_count=$(printf '%s' "$body" | grep -o '"writer"' | wc -l | tr -d ' ')
        if [ "$o_ok" = 1 ] && [ "$o_count" = "$#" ]; then
          shown "$o_label reads the list: " "$o_shown"
          return 0
        fi ;;
      404|409) ;;
      000) echo "  $o_label's read: no answer" >&2; exit 1 ;;
      *) echo "  $o_label's read answered $code: $body" >&2; exit 1 ;;
    esac
    sleep 0.5
  done
  echo "  $o_label never read the list as $o_shown; last answer $code: $body" >&2
  exit 1
}

claim_reads() { # url reader pod member id expected label
  for _ in $(seq 1 240); do
    request -m 2 "$(pod_route "$1" "$2" "$3")/records/$4/claim/$5"
    case "$code" in
      200) if [ "$body" = "$6" ]; then shown "$7 reads " "$6"; return 0; fi ;;
      404|409) ;;
      000) echo "  $7's read: no answer" >&2; exit 1 ;;
      *) echo "  $7's read answered $code: $body" >&2; exit 1 ;;
    esac
    sleep 0.5
  done
  echo "  $7 never read \"$6\"; last answer $code: $body" >&2
  exit 1
}

# The denial: for the whole watch, every read is absent or refused.
claim_never_reads() { # url reader pod member id label seconds
  c_until=$(( $(date +%s) + $7 ))
  while [ "$(date +%s)" -lt "$c_until" ]; do
    request -m 2 "$(pod_route "$1" "$2" "$3")/records/$4/claim/$5"
    case "$code" in
      404|409) ;;
      *) echo "  $6's read answered $code: $body" >&2; exit 1 ;;
    esac
    sleep 0.2
  done
  fact "$6 reads nothing of it: every read for $7 seconds answered 404 or 409"
}

pod_gone() { # url identity pod label
  for _ in $(seq 1 240); do
    HELD=$(call GET "$1/debug/identities/$2/pods") || exit 1
    case "$HELD" in
      *"$3"*) ;;
      *) fact "$4 no longer lists the pod"; return 0 ;;
    esac
    sleep 0.5
  done
  echo "  $4 still lists the pod" >&2
  exit 1
}

act() { # url identity pod act-json
  call POST "$(pod_route "$1" "$2" "$3")/acts" --data-raw "$4" >/dev/null
}

refused() { # url identity pod act-json label
  request -m 30 -X POST "$(pod_route "$1" "$2" "$3")/acts" --data-raw "$4"
  case "$code" in
    403) shown "$5: " "403, refused" ;;
    *) echo "  $5 answered $code, not a refusal: $body" >&2; exit 1 ;;
  esac
}

edit_list() { # url identity text
  call POST "$(pod_route "$1" "$2" "$FAMILY")/records/$ALICE/mergeable-document/$LIST/ops" \
    --data-raw "$3" >/dev/null
}

printf '\n'

for url in "$ALICE_PHONE" "$ALICE_LAPTOP" "$BOB_PHONE" "$BOB_LAPTOP" \
           "$CAROL_PHONE" "$DAVE_PHONE"; do
  wait_live "$url"
done

say "Alice, Bob, Carol and Dave each start on their own phone."
ALICE=$(new_identity "$ALICE_PHONE")
BOB=$(new_identity "$BOB_PHONE")
CAROL=$(new_identity "$CAROL_PHONE")
DAVE=$(new_identity "$DAVE_PHONE")
fact "Alice $ALICE"
fact "Bob   $BOB"
fact "Carol $CAROL"
fact "Dave  $DAVE"

say "Alice adds her laptop before any pod exists."
link_device "$ALICE" "$ALICE_PHONE" "$ALICE_LAPTOP" "Alice's laptop joined"

say "Alice creates a pod for the family. It has an id and no key of its own; its creator is its first owner."
CREATED=$(call POST "$ALICE_PHONE/debug/identities/$ALICE/pods")
FAMILY=$(printf '%s' "$CREATED" | sed -E 's/.*"pod":"([^"]+)".*/\1/')
fact "pod $FAMILY"
members_are "$ALICE_PHONE" "$ALICE" "$FAMILY" "Alice's phone" "$ALICE:owner:Alice"

say "Alice invites Bob. The invite carries a one-time secret, and Bob's phone joins with it — no connection between them, now or later."
INVITE=$(call POST "$(pod_route "$ALICE_PHONE" "$ALICE" "$FAMILY")/invites?lifetime_secs=300")
call POST "$BOB_PHONE/debug/identities/$BOB/pods/join" --data-raw "$INVITE" >/dev/null
fact "Bob joined on his phone"

say "Any member invites: Bob invites Carol."
INVITE=$(call POST "$(pod_route "$BOB_PHONE" "$BOB" "$FAMILY")/invites?lifetime_secs=300")
call POST "$CAROL_PHONE/debug/identities/$CAROL/pods/join" --data-raw "$INVITE" >/dev/null
fact "Carol joined on her phone"

say "Bob adds his laptop after joining: it reaches the pod through Bob's own devices."
link_device "$BOB" "$BOB_PHONE" "$BOB_LAPTOP" "Bob's laptop joined"

say "Every device of every member lists the same three members."
THREE="$ALICE:owner:Alice $BOB:plain:Bob $CAROL:plain:Carol"
# shellcheck disable=SC2086 # one word per member
members_are "$ALICE_LAPTOP" "$ALICE" "$FAMILY" "Alice's laptop" $THREE
# shellcheck disable=SC2086
members_are "$BOB_LAPTOP" "$BOB" "$FAMILY" "Bob's laptop" $THREE
# shellcheck disable=SC2086
members_are "$CAROL_PHONE" "$CAROL" "$FAMILY" "Carol's phone" $THREE

say "Alice starts a shopping list: a mergeable-document, which every member edits."
PLACED=$(call POST "$(pod_route "$ALICE_PHONE" "$ALICE" "$FAMILY")/records?kind=mergeable-document" --data-raw 'milk')
LIST=$(printf '%s' "$PLACED" | record_id)
list_reads "$CAROL_PHONE" "$CAROL" "$FAMILY" "$LIST" "Carol's phone" "$ALICE:milk:Alice"

say "Carol adds to it from her phone. Each edit is an operation signed by its writer."
edit_list "$CAROL_PHONE" "$CAROL" 'eggs'
list_reads "$ALICE_LAPTOP" "$ALICE" "$FAMILY" "$LIST" "Alice's laptop" "$ALICE:milk:Alice" "$CAROL:eggs:Carol"
list_reads "$BOB_LAPTOP" "$BOB" "$FAMILY" "$LIST" "Bob's laptop" "$ALICE:milk:Alice" "$CAROL:eggs:Carol"

say "Membership has two roles. Carol, a plain member, cannot remove anyone."
refused "$CAROL_PHONE" "$CAROL" "$FAMILY" "{\"remove\":\"$BOB\"}" "Carol removing Bob"

say "Alice promotes Bob to owner."
act "$ALICE_PHONE" "$ALICE" "$FAMILY" "{\"promote\":\"$BOB\"}"
PROMOTED="$ALICE:owner:Alice $BOB:owner:Bob $CAROL:plain:Carol"
# shellcheck disable=SC2086
members_are "$BOB_PHONE" "$BOB" "$FAMILY" "Bob's phone" $PROMOTED
# shellcheck disable=SC2086
members_are "$CAROL_PHONE" "$CAROL" "$FAMILY" "Carol's phone" $PROMOTED

if [ "$DEMO_COMPOSE" != "none" ]; then
  say "Alice's phone and laptop go offline."
  PHONE_BEFORE=$(node_id "$ALICE_PHONE")
  LAPTOP_BEFORE=$(node_id "$ALICE_LAPTOP")
  $DEMO_COMPOSE stop alice-phone alice-laptop >/dev/null 2>&1
  fact "no device of the pod's creator is online"
fi

say "Dave joins on Bob's invitation. His first session, with Bob's phone, brings the whole pod — the list included."
INVITE=$(call POST "$(pod_route "$BOB_PHONE" "$BOB" "$FAMILY")/invites?lifetime_secs=300")
call POST "$DAVE_PHONE/debug/identities/$DAVE/pods/join" --data-raw "$INVITE" >/dev/null
list_reads "$DAVE_PHONE" "$DAVE" "$FAMILY" "$LIST" "Dave's phone" "$ALICE:milk:Alice" "$CAROL:eggs:Carol"
members_are "$DAVE_PHONE" "$DAVE" "$FAMILY" "Dave's phone" \
  "$ALICE:owner:Alice" "$BOB:owner:Bob" "$CAROL:plain:Carol" "$DAVE:plain:Dave"

say "Dave adds bread. Every member device relays what it holds, so the edit reaches whoever is online."
edit_list "$DAVE_PHONE" "$DAVE" 'bread'
FULL="$ALICE:milk:Alice $CAROL:eggs:Carol $DAVE:bread:Dave"
# shellcheck disable=SC2086
list_reads "$BOB_LAPTOP" "$BOB" "$FAMILY" "$LIST" "Bob's laptop" $FULL
# shellcheck disable=SC2086
list_reads "$CAROL_PHONE" "$CAROL" "$FAMILY" "$LIST" "Carol's phone" $FULL

if [ "$DEMO_COMPOSE" != "none" ]; then
  say "Alice's devices come back. Their state is on disk — the same nodes, the same pod, the addresses of the members they met — and they catch up on what happened without them."
  $DEMO_COMPOSE start alice-phone alice-laptop >/dev/null 2>&1
  wait_live "$ALICE_PHONE"
  wait_live "$ALICE_LAPTOP"
  PHONE_AFTER=$(node_id "$ALICE_PHONE")
  LAPTOP_AFTER=$(node_id "$ALICE_LAPTOP")
  if [ -z "$PHONE_BEFORE" ] || [ "$PHONE_BEFORE" != "$PHONE_AFTER" ] \
     || [ -z "$LAPTOP_BEFORE" ] || [ "$LAPTOP_BEFORE" != "$LAPTOP_AFTER" ]; then
    echo "  Alice's devices came back as different nodes" >&2
    exit 1
  fi
  shown "Alice's phone, node id unchanged: " "$PHONE_AFTER"
  shown "Alice's laptop, node id unchanged: " "$LAPTOP_AFTER"
fi
# shellcheck disable=SC2086
list_reads "$ALICE_PHONE" "$ALICE" "$FAMILY" "$LIST" "Alice's phone" $FULL
members_are "$ALICE_LAPTOP" "$ALICE" "$FAMILY" "Alice's laptop" \
  "$ALICE:owner:Alice" "$BOB:owner:Bob" "$CAROL:plain:Carol" "$DAVE:plain:Dave"

say "Bob, an owner, removes Dave. An owner's act needs no other owner."
act "$BOB_PHONE" "$BOB" "$FAMILY" "{\"remove\":\"$DAVE\"}"
# shellcheck disable=SC2086
members_are "$CAROL_PHONE" "$CAROL" "$FAMILY" "Carol's phone" $PROMOTED

say "Bob places the new door code as a claim. It reaches Alice, Carol and Bob's laptop, and never Dave."
PLACED=$(call POST "$(pod_route "$BOB_PHONE" "$BOB" "$FAMILY")/records?kind=claim" --data-raw 'door code 4711')
CODE=$(printf '%s' "$PLACED" | record_id)
claim_reads "$BOB_LAPTOP" "$BOB" "$FAMILY" "$BOB" "$CODE" "door code 4711" "Bob's laptop"
claim_reads "$CAROL_PHONE" "$CAROL" "$FAMILY" "$BOB" "$CODE" "door code 4711" "Carol's phone"
claim_reads "$ALICE_LAPTOP" "$ALICE" "$FAMILY" "$BOB" "$CODE" "door code 4711" "Alice's laptop"
claim_never_reads "$DAVE_PHONE" "$DAVE" "$FAMILY" "$BOB" "$CODE" "Dave's phone" 5
pod_gone "$DAVE_PHONE" "$DAVE" "$FAMILY" "Dave's phone"

say "Carol leaves. Leaving is every member's own act."
act "$CAROL_PHONE" "$CAROL" "$FAMILY" '"leave"'
members_are "$ALICE_PHONE" "$ALICE" "$FAMILY" "Alice's phone" "$ALICE:owner:Alice" "$BOB:owner:Bob"
pod_gone "$CAROL_PHONE" "$CAROL" "$FAMILY" "Carol's phone"

say "One pod on six devices: any member invited, the pod carried on while its creator was offline and the creator caught up on coming back, and an owner's removal stopped what reaches the one removed."

printf '\n'
