# Common to every step of the demonstration. It does not run on its own — the
# scripts beside it source it.
#
# Variables do not survive a separate run, so whatever the next step needs goes
# into a file: the laptop's identity in tmp/mac-identity, the phone's in
# tmp/peer-id, put there as soon as the ceremony introduces the two.

# ${BASH_SOURCE[0]} is empty when this file is sourced from zsh, and the root
# would then resolve one level up and every read would answer emptiness rather
# than refuse. ${(%):-%x} is zsh's own answer to the same question.
_lib="${BASH_SOURCE[0]:-${(%):-%x}}"
PDN="$(cd "$(dirname "$_lib")/.." && pwd)"
MAC=${MAC:-http://127.0.0.1:3011}
NODE_DIR="$PDN/tmp/pdn-mac-node2"
NODE_LOG="$PDN/tmp/pdn-mac-node2.log"
NODE_BIN="$PDN/target/debug/pdn-node-http"
DEV=${DEV:-5A613C8A-3804-5F62-A945-C7D8D934D088}
BUNDLE=org.mee.pdn.app

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
  echo "nothing arrived within 48 seconds — see $NODE_LOG" >&2
  return 1
}

# Does the laptop's node answer?
node_up() { curl -sf "$MAC/ready" >/dev/null; }

# Require a node that is up: without one the remaining steps mean nothing.
need_node() {
  node_up && return 0
  echo "the laptop's node does not answer on $MAC — run demo/00-start.sh" >&2
  exit 1
}

# The laptop's identity: the one already in the directory, or a fresh one.
mine() {
  local id
  id=$(curl -sf "$MAC/debug/identities" | jq -r '.identities[0] // empty')
  if [ -z "$id" ]; then
    id=$(curl -sf -X POST "$MAC/debug/identities" | jq -r .identity)
    echo "minted the laptop's identity: $id" >&2
  fi
  echo "$id" > "$PDN/tmp/mac-identity"
  echo "$id"
}

# The phone's identity, as the ceremony step recorded it.
peer() {
  local id
  id=$(cat "$PDN/tmp/peer-id" 2>/dev/null)
  if [ -z "$id" ]; then
    echo "there is no connection yet — run demo/03-invite.sh" >&2
    exit 1
  fi
  echo "$id"
}
