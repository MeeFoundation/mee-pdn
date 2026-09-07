# Demonstration run-sheet: the commands in order

Two nodes: the phone with the application, and the laptop's node. The full command reference is `demo-commands.md`; this file is only the sequence performed live.

The presenter runs the command blocks: they are the other side of the demonstration — the phone shows the interface, the laptop stands in for a second device. Section 0 and the pre-flight are done before the audience arrives; the blocks of acts 1 to 9 are run in front of them, as the story goes.

All of it is packed into scripts under `demo/` — one per act, `demo/00-start.sh`, `demo/01-nodes.sh` and so on; `demo/README.md` lists them and says what each does (in Russian). Below are the same commands loose, for when something has to be done by hand.

Every block is copy-and-paste whole. The waiting is built in: where something travels over the network, the command waits for it rather than showing the audience an empty answer.

The values of the current run are filled in. After restarting the node on a different directory, replace `MINE` with whatever `POST /debug/identities` returns.

## 0. Preamble — once before the start

```sh
cd ~/vsprojects/mee/mee-pdn
export PDN=$PWD
export MAC=http://127.0.0.1:3011
export MINE=dc9521aa34d45d9c7be6b702e6eb7c52b0afd7b6ce2636901ec64722540a1cff
export DEV=5A613C8A-3804-5F62-A945-C7D8D934D088
```

Three functions for the whole run. The ceremony code the phone reads is not JSON but base64url without padding over it — that is how the `pdn-mobile` facade mints it and how it reads one back — whereas the HTTP debug surface returns and accepts bare JSON. The third function waits for what travels over the network and prints it the moment it arrives.

```sh
# JSON in -> a QR on the screen, in the form the phone expects
pdnqr()  { base64 | tr -d '\n' | tr '+/' '-_' | tr -d '=' | qrencode -l L -s 6 -o "$1" && open "$1"; }
# a code taken off the phone's screen -> the JSON /debug accepts
pdnraw() { python3 -c "import base64,sys;d=sys.stdin.read().strip();sys.stdout.write(base64.urlsafe_b64decode(d+'='*(-len(d)%4)).decode())"; }
# wait up to 48 seconds for a command to print something
pdnwait() { for i in $(seq 1 24); do out=$(eval "$1") && [ -n "$out" ] && { echo "$out"; return 0; }; sleep 2; done; echo "nothing arrived within 48 seconds"; return 1; }
```

Bring the laptop's node up. The directory `tmp/pdn-mac-node2` survives a restart: the identity and the connections come back the same. For a run from a clean slate, do the "A full reset between runs" section first.

```sh
# The first block is required: without $PDN and $MAC the next line starts the wrong path
[ -x "$PDN/target/debug/pdn-node-http" ] || cargo build -p pdn-node-http
# The previous node has to release the directory lock, and that takes more than a second
pkill -f 'target/debug/pdn-node-http'
for i in $(seq 1 15); do pgrep -f 'target/debug/pdn-node-http' >/dev/null || break; sleep 1; done
PDN_DATA_DIR=$PDN/tmp/pdn-mac-node2 PDN_DEBUG=1 \
  nohup $PDN/target/debug/pdn-node-http > $PDN/tmp/pdn-mac-node2.log 2>&1 & disown
for i in $(seq 1 20); do curl -sf $MAC/ready >/dev/null && break; sleep 1; done
curl -sf $MAC/debug/status || { echo "the node did not come up, here is its log:"; tail -20 $PDN/tmp/pdn-mac-node2.log; }
```

If `debug/status` names no identity, the directory is clean and one has to be minted:

```sh
export MINE=$(curl -s -X POST $MAC/debug/identities | jq -r .identity)
echo "the laptop's identity: $MINE"
```

A second tab — the ceremony and the sync are visible there:

```sh
tail -f $PDN/tmp/pdn-mac-node2.log
```

A third one, to show what the phone says. The command launches the application itself and holds its stderr; `devicectl` has no way to attach to one already running:

```sh
xcrun devicectl device process launch --console --device $DEV org.mee.pdn.app
```

---

## Act 1. Two independent nodes

**Phone.** Open the application → **Bring the node up** → **Create an identity**. Show the audience the node id on the card.

**Laptop.** Its own node id and its own identities:

```sh
curl -s $MAC/debug/status
curl -s $MAC/debug/identities | jq
```

The point to say out loud: there are two nodes, no server between them, and each holds its own keys.

---

## Act 2. Your data stays with you

**Phone.** **My entries** → path `contact/email`, value `anton@example.com` → **Write**. Then **Read it** under the row.

**Laptop.** The same, as its own identity:

```sh
curl -s -X PUT $MAC/debug/data/$MINE/contact/email --data-binary 'laptop@example.com'
curl -s $MAC/debug/data/$MINE | jq
```

The point to say out loud: with no connection between them, neither node knows anything of the other's data — it does not even know that such an issuer exists.

---

## Act 3. The ceremony

First wait for the laptop's endpoint to settle and take a home relay: for the first seconds after the node comes up a code carries local addresses only, and reachability beyond one network is lost.

```sh
pdnwait 'curl -s -X POST "$MAC/debug/identities/$MINE/invite?lifetime_secs=10" | jq -re ".inviter_addr.addrs[]|select(.Relay)|.Relay"'
```

**Mint an invite and show it as a QR:**

```sh
curl -s -X POST "$MAC/debug/identities/$MINE/invite?lifetime_secs=180" > tmp/invite.json
jq '.inviter_addr.addrs' tmp/invite.json
pdnqr tmp/invite.png < tmp/invite.json
```

**Phone.** **Connections** → **Read a code** → the act **Accepting an invitation to connect** → point it at the laptop's screen. While the ceremony runs the screen says "Running the ceremony. It ends within 30 seconds either way".

**Both sides see each other.** Take the phone's identity into a variable — every act below needs it:

```sh
export PEER=$(pdnwait 'curl -s $MAC/debug/identities/$MINE/connections | jq -re ".connections[0]"')
echo "the phone: $PEER"
```

The point to say out loud: a connection proves that two devices held one secret, and nothing more. Not who the person is, and not that they told the truth.

---

## Act 4. The phone shares with the laptop

**Phone.** **Connections** → the laptop's row → the card **Share claims with this peer** → tap a path → **Grant read-only**. The path lights up and appears under "What I share with this peer".

**The laptop waits for the grant, then reads the value.** About ten seconds pass between the tap on the phone and the value on the laptop: the grant record travels first, and the payload separately after it.

```sh
pdnwait 'curl -s $MAC/debug/identities/$MINE/grants/$PEER | jq -ce "select(.grants|length>0)"' | jq
pdnwait 'curl -sf $MAC/debug/data/$PEER/name'
```

Replace `name` with the path you actually granted from the phone. What arrived under that issuer at all:

```sh
curl -s $MAC/debug/data/$PEER | jq
```

The point to say out loud: the grant named an issuer and exactly one claim; nothing else under that issuer replicates here at all. It can be shown like this:

```sh
curl -s -w ' [HTTP %{http_code}]\n' $MAC/debug/data/$PEER/notes/private
```

Name honestly what that shows, though: the answer is `404 no entry`, not a refusal. Before the grant the answer was a different one — `409 data namespace not bound on this node`, the namespace was not bound at all. With the grant in place the namespace is bound, and an ungranted path is indistinguishable from one that does not exist. The real test of the boundary is a third node that was granted nothing, and it is not shown on a phone.

---

## Act 5. The laptop shares with the phone

```sh
curl -s -X POST $MAC/debug/identities/$MINE/grants/$PEER \
  -H 'content-type: application/json' -d '{
    "issuer": "'$MINE'",
    "claims": [{"path": "contact/email", "write": false}]
  }' -w 'HTTP %{http_code}\n'
curl -s $MAC/debug/identities/$MINE/own-grants/$PEER | jq
```

`HTTP 204` means the publication was taken. In `own-grants` the claims appear as hashes: a claim's identity is derived one way from the issuer and the path, and the path does not come back out of it.

**Phone.** On the connection screen the card **What this peer shares with me** fills itself: the path, the read-only mark, and the value `laptop@example.com`.

---

## Act 6. Withdraw, then grant again

**Phone.** "What I share with this peer" → **Withdraw this grant**.

**Laptop.** No grants left, and the namespace is unbound — the same `409` as before the two ever met:

```sh
pdnwait 'curl -s $MAC/debug/identities/$MINE/grants/$PEER | jq -ce "select((.grants|length)==0)"'
curl -s -w ' [HTTP %{http_code}]\n' $MAC/debug/data/$PEER/name
```

**Phone.** **Grant read-only** again — the access opens back up:

```sh
pdnwait 'curl -sf $MAC/debug/data/$PEER/name'
```

The point to say out loud: a withdrawal closes further delivery and does not recall what was already delivered. The promise is careful, and it is honest.

---

## Act 7. The right to write

**The laptop grants the phone two claims, the second one writable:**

```sh
curl -s -X PUT $MAC/debug/data/$MINE/notes/shared --data-binary 'the first line, from the laptop'
curl -s -X POST $MAC/debug/identities/$MINE/grants/$PEER \
  -H 'content-type: application/json' -d '{
    "issuer": "'$MINE'",
    "claims": [
      {"path": "contact/email", "write": false},
      {"path": "notes/shared", "write": true}
    ]
  }' -w 'HTTP %{http_code}\n'
```

**Phone.** Under the writable claim a **write a new value** field appears — type something and send it. A read-only claim has no field at all.

**The laptop sees what the phone wrote, in its own data:**

```sh
pdnwait 'curl -sf $MAC/debug/data/$MINE/notes/shared'
```

---

## Act 8. The application is put away

**Phone.** Put it away with the home gesture. Do not lock the screen: a lock may end the process, and then something else is being measured.

**The laptop writes while the phone is "gone":**

```sh
curl -s -X PUT $MAC/debug/data/$MINE/contact/email --data-binary 'changed while the phone slept'
```

**Phone.** Come back to the application. The value arrives on its own, with no second bring-up.

---

## Act 9. Everything survives a restart

**Phone.** Close the application with a swipe, open it, **Bring the node up**. The node id and the identity are the same, the connection is there, the data is there.

**Laptop — the same.** The `pkill` pattern carries a path on purpose: a bare `pdn-node-http` also matches a build's command line and kills the wrong thing.

```sh
# The first block is required: without $PDN and $MAC the next line starts the wrong path
[ -x "$PDN/target/debug/pdn-node-http" ] || cargo build -p pdn-node-http
# The previous node has to release the directory lock, and that takes more than a second
pkill -f 'target/debug/pdn-node-http'
for i in $(seq 1 15); do pgrep -f 'target/debug/pdn-node-http' >/dev/null || break; sleep 1; done
PDN_DATA_DIR=$PDN/tmp/pdn-mac-node2 PDN_DEBUG=1 \
  nohup $PDN/target/debug/pdn-node-http > $PDN/tmp/pdn-mac-node2.log 2>&1 & disown
for i in $(seq 1 20); do curl -sf $MAC/ready >/dev/null && break; sleep 1; done
curl -sf $MAC/debug/status || { echo "the node did not come up, here is its log:"; tail -20 $PDN/tmp/pdn-mac-node2.log; }
```

The point to say out loud: a node comes back on its directory as itself. Only work in flight does not come back — a minted and unconsumed invite, an interrupted ceremony. The home relay is taken again too, and that costs about ten seconds.

---

## When something goes wrong

| What is seen | Why, and what to do |
| --- | --- |
| `REFUSED · MALFORMED-INPUT`, with "refused by this application before the node was called" | Bare JSON went into the QR. The phone expects base64url over it — draw a code only through `pdnqr` |
| `REFUSED · COUNTERPARTY-UNREACHABLE` | Local Network is off for the application (Settings → PDN), or the invite was minted before the home relay came up. Run the relay `pdnwait` from act 3 |
| The code on the screen does not read | Raise the scale in `pdnqr` (`-s 8`), kill the reflection, give the phone 20–30 cm |
| The ceremony hangs and times out | The invite expired: `lifetime_secs` ran out. Mint another |
| `pdnwait` prints "nothing arrived" | Watch `tail -f $PDN/tmp/pdn-mac-node2.log`: it shows whether a sync is running and with which peer |
| `FAILED · INTERNAL` on the screen | Relaunch the application through `devicectl … --console` and read its stderr |
| The code-reading screen is blank | The camera is refused to the application. Settings → PDN → Camera |

## A full reset between runs

```sh
pkill -f 'target/debug/pdn-node-http'
rm -rf $PDN/tmp/pdn-mac-node2 && mkdir -p $PDN/tmp/pdn-mac-node2
xcrun devicectl device uninstall app --device $DEV org.mee.pdn.app
APP=$(ls -dt ~/Library/Developer/Xcode/DerivedData/PDN-*/Build/Products/Release-iphoneos/PDN.app | head -1)
xcrun devicectl device install app --device $DEV "$APP"
```

Deleting the application erases the directory together with the node's key: that is the loss of the only copy, not a cache being cleared. Exactly what a clean run needs, and exactly what must not happen by accident.

## Pre-flight

```sh
# The tools are here
which jq qrencode

# The whole QR path: mint, draw, read back
curl -s -X POST "$MAC/debug/identities/$MINE/invite?lifetime_secs=60" > tmp/probe.json
pdnqr tmp/probe.png < tmp/probe.json
diff tmp/probe.json <(swift $PDN/tmp/qrdecode.swift tmp/probe.png 2>/dev/null | pdnraw) \
  && echo "the QR reads back into the invite it was made from" && rm -f tmp/probe.json tmp/probe.png

# The addresses the node puts into an invite
curl -s -X POST "$MAC/debug/identities/$MINE/invite" | jq '.inviter_addr.addrs'
```

What it should come to: a code of about 490 characters, a round-trip with no difference, and four addresses — the relay `euc1-1.relay.n0.iroh.link`, a public address found through the NAT, and two local ones.
