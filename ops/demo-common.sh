# What every demo narration shares: the voice, one checked call, and the few
# acts every show begins with. Sourced, never run: a narration names its
# nodes and its compose project before it sources this file.

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

# One request with its status and body apart, in `code` and `body`: `-w`
# appends the status to the body as its last three characters, `000` when
# nothing answered.
request() { # curl args...
  out=$(curl -s -w '%{http_code}' "$@") || out=000
  code=${out#"${out%???}"}
  body=${out%???}
}

# Every call a step makes is checked: a refused one stops the show there,
# with the node's own answer, instead of surfacing minutes later as a read
# that never arrives. The answer goes to stdout, for the step to keep in a
# variable of its own; inside `$(…)` the exit ends only the subshell, and the
# failed assignment ends the script under `set -e`. The ceiling sits above
# the longest call, a link bounded at 120 seconds.
call() { # method url [curl args...]
  method=$1 url=$2
  shift 2
  request -m 150 -X "$method" "$url" "$@"
  case "$code" in
    2??) printf '%s' "$body" ;;
    000) echo "  $method $url: no answer" >&2; exit 1 ;;
    *) echo "  $method $url answered $code: $body" >&2; exit 1 ;;
  esac
}

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

new_identity() { # url
  CREATED=$(call POST "$1/debug/identities") || exit 1
  printf '%s\n' "$CREATED" | sed -E 's/.*"identity":"([^"]+)".*/\1/'
}

link_device() { # identity from to label
  LINKING_INVITE=$(call POST "$2/debug/identities/$1/linking-invite") || exit 1
  call POST "$3/debug/link?timeout_secs=120" --data-raw "$LINKING_INVITE" >/dev/null
  fact "$4"
}

hosts() { # url identity label persona
  HOSTED=$(call GET "$1/debug/identities") || exit 1
  case "$HOSTED" in
    *"$2"*) shown "$3 hosts " "$4" ;;
    *) echo "  $3 does not host $4" >&2; exit 1 ;;
  esac
}

node_id() { # url
  STATUS=$(call GET "$1/debug/status") || exit 1
  printf '%s\n' "$STATUS" | sed -n 's/^node //p'
}
