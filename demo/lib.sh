# Common to every step of the demonstration. It does not run on its own — the
# scripts beside it source it.
#
# Three nodes that are not phones run on this machine. Alice's laptop node is
# where her identity is created and her first entries are written; her phone
# joins that identity later, through the linking ceremony. Bob is the peer she
# connects to. Carol holds nothing of Alice and is here to prove it.
#
# Variables do not survive a separate run, so whatever the next step needs goes
# into a file under tmp/: each node's identity in tmp/<name>-id, Alice's second
# identity in tmp/alice-other-id.

# ${BASH_SOURCE[0]} is empty when this file is sourced from zsh, and the root
# would then resolve one level up and every read would answer emptiness rather
# than refuse. ${(%):-%x} is zsh's own answer to the same question.
_lib="${BASH_SOURCE[0]:-${(%):-%x}}"
PDN="$(cd "$(dirname "$_lib")/.." && pwd)"
NODE_BIN="$PDN/target/debug/pdn-node-http"
DEV=${DEV:-5A613C8A-3804-5F62-A945-C7D8D934D088}
BUNDLE=org.mee.pdn.app

ALICE=${ALICE:-http://127.0.0.1:3011}
BOB=${BOB:-http://127.0.0.1:3012}
CAROL=${CAROL:-http://127.0.0.1:3013}
NODES="alice bob carol"

url_of()  { case "$1" in alice) echo "$ALICE";; bob) echo "$BOB";; carol) echo "$CAROL";; esac; }
port_of() { case "$1" in alice) echo 3011;; bob) echo 3012;; carol) echo 3013;; esac; }
dir_of()  { echo "$PDN/tmp/pdn-$1"; }
log_of()  { echo "$PDN/tmp/pdn-$1.log"; }

# JSON in -> a QR on the screen, in the form the phone expects: the facade
# mints and reads a code as base64url without padding over JSON, whereas this
# surface returns bare JSON.
pdnqr() { base64 | tr -d '\n' | tr '+/' '-_' | tr -d '=' | qrencode -l L -s 6 -o "$1" && open "$1"; }

# A code taken off the phone's screen -> the JSON /debug accepts.
pdnraw() { python3 -c "import base64,sys;d=sys.stdin.read().strip();sys.stdout.write(base64.urlsafe_b64decode(d+'='*(-len(d)%4)).decode())"; }

# Wait up to 48 seconds for a command to print something. Nothing that travels
# over the network arrives at once, and an empty screen shows the audience nothing.
pdnwait() {
  local i out
  for i in $(seq 1 24); do
    out=$(eval "$1") && [ -n "$out" ] && { echo "$out"; return 0; }
    sleep 2
  done
  echo "nothing arrived within 48 seconds — see the node logs in $PDN/tmp" >&2
  return 1
}

# Does this node answer?
node_up() { curl -sf "$(url_of "$1")/ready" >/dev/null; }

# Require every node of the staging: without them the remaining steps mean nothing.
need_nodes() {
  local n missing=
  for n in $NODES; do node_up "$n" || missing="$missing $n"; done
  [ -z "$missing" ] && return 0
  echo "these nodes do not answer:$missing — run demo/00-start.sh" >&2
  exit 1
}

# A node's identity: the one recorded, the one already in its directory, or a
# fresh one. Alice's is the identity her phone joins, so every later step reads
# the same value from the same file.
ident() {
  local n=$1 f="$PDN/tmp/$1-id" id
  id=$(cat "$f" 2>/dev/null)
  if [ -z "$id" ]; then
    id=$(curl -sf "$(url_of "$n")/debug/identities" | jq -r '.identities[0] // empty')
    if [ -z "$id" ]; then
      id=$(curl -sf -X POST "$(url_of "$n")/debug/identities" | jq -r .identity)
      echo "minted $n's identity: $id" >&2
    fi
    echo "$id" > "$f"
  fi
  echo "$id"
}

# Alice's second identity — the one her phone does not join. A device belongs
# to an identity, not to a person, and this is what makes that visible.
alice_other() {
  local f="$PDN/tmp/alice-other-id" id
  id=$(cat "$f" 2>/dev/null)
  if [ -z "$id" ]; then
    id=$(curl -sf -X POST "$ALICE/debug/identities" | jq -r .identity)
    echo "$id" > "$f"
  fi
  echo "$id"
}

start_node() {
  local n=$1
  mkdir -p "$(dir_of "$n")"
  PDN_DATA_DIR="$(dir_of "$n")" PDN_PORT="$(port_of "$n")" PDN_DEBUG=1 \
    nohup "$NODE_BIN" > "$(log_of "$n")" 2>&1 &
  disown
}
