# Demonstration run-sheet: the acts in order

The cast is 4 nodes, one of them a phone. **Alice** lives on 2 devices: a node on a laptop, where she creates her identity and writes her first entries, and a phone that joins that same identity through the linking ceremony. **Bob** is a separate node on a laptop with an identity of his own. **Carol** is a fourth node, which holds nothing of Alice's and is here to show it.

The phone is the only screen in the staging. Bob's and Carol's nodes are `pdn-node-http` processes on the presenter's machine, and their terminal output is the counterparty's behaviour, never a stand-in for what a person would see. Every act whose subject is what a person sees happens on the phone, which is why granting is shown in both directions.

Every command is a script in `demo/` — one per act, `demo/00-start.sh`, `demo/01-alice.sh` and so on; `demo/README.md` lists them. The waiting is built in: where a value crosses the network the script waits for it rather than showing the audience an empty screen.

## 0. Before the audience

```sh
cd ~/vsprojects/mee/mee-pdn
demo/00-start.sh      # 3 nodes, an identity for each, a second identity for Alice
demo/preflight.sh     # the tools, the QR path, a relay address on every node
```

A second tab, where the ceremonies and the sync are visible:

```sh
tail -f tmp/pdn-alice.log
```

A third, to show what the phone says. The command launches the application itself and holds its stderr; `devicectl` offers no way to attach to one already running:

```sh
demo/phone-log.sh
```

By this point the phone is a clean install: no node up, no identity. `demo/reset.sh` puts it in that state.

---

## Act 1. Alice starts on her laptop

**Presenter.** `demo/01-alice.sh` — the node id, Alice's 2 identities, 3 entries under the one her phone will join, and the same path under the other.

**Phone.** Nothing. It has joined nothing yet, and that is worth showing: the identity exists before the device, not the other way round.

Said out loud: 2 identities on one node are 2 lives, not 2 accounts. The same path under each holds a different value, and neither knows anything of the other.

---

## Act 2. Alice joins her phone

**Presenter.** `demo/02-link-phone.sh` — waits for a home relay, mints the linking payload and draws it as a code that lives 180 seconds.

**Phone.** **Bring the node up** → **Read a code** → the act **A device joining an identity** → point it at the laptop's screen.

**What is visible afterwards.** The phone hosts Alice's identity, and only that one — her other identity does not appear on it. **My entries** already lists `contact/email`, `contact/phone` and `notes/private`, none of which were written on this phone.

Said out loud: a device belongs to an identity, not to a person. The catch-up takes the state whole; a failed link rolls back and leaves no half.

The refusal beside it: a code read under the wrong act — as an invitation to connect — is refused by the runtime, and the phone shows the refusal as what it is.

---

## Act 3. Bob connects to Alice

**Presenter.** `demo/03-connect-bob.sh` — Bob's node mints an invite and draws it as a code.

**Phone.** **Connections** → **Read a code** → the act **Accepting an invitation to connect** → point it at the screen. While the ceremony runs the screen says "Running the ceremony. It ends within 30 seconds either way".

**The script waits for 2 things.** First the connection appears on Bob's node. Then on Alice's laptop, where nobody performed an act: a connection belongs to an identity, and the identity's other device comes to hold it on its own.

**Burn the code there and then.** Present the same code to the phone a second time: a refusal on the screen, and Bob still lists exactly one connection to Alice.

Said out loud: a connection proves that 2 devices held one secret, and nothing else. Not who the person is, and not that they told the truth.

---

## Act 4. Alice grants Bob one claim

**Phone.** **Connections** → Bob's row → the card **Share claims with this peer** → tap `contact/email` → **Grant read-only**.

**Presenter.** `demo/04-alice-grants.sh` — waits for the grant, reads the value on Bob's node, then asks it for a path Alice did not grant.

Said out loud: a grant names the issuer and exactly the claims listed in it. The rest is not on Bob's node — not hidden, absent: no content, no path, no count. The `404` on the ungranted path means "no such entry", not a refusal; before the grant the answer was `409`, the namespace not bound at all.

---

## Act 5. Bob grants Alice 2 claims, the second writable

**Presenter.** `demo/05-bob-grants.sh` — Bob writes 2 entries of his own and publishes a grant: `contact/email` read-only, `notes/shared` with the right to write.

**Phone.** The card **What this peer shares with me** fills itself. Under the writable claim there is a **write a new value** field — type and send, and the script prints what landed in Bob's own entry. The read-only claim has no field at all, and a write attempted against it is refused with what was refused named, the previous value still in place.

Said out loud: this is the direction of granting that has a screen on the receiving end. The boundary is visible on a device rather than in an argument: the phone in the hand declines.

---

## Act 6. Withdrawn and granted again, in both directions

**Phone.** "What I share with this peer" → **Withdraw this grant**.

**Presenter.** `demo/06-withdraw.sh` — waits until the grants are gone and reads the same path on Bob's node: `409`, the namespace unbound. Alice's own read is unaffected.

**Phone.** **Grant read-only** again — the access reopens.

**Then the script withdraws from Bob's side.** On the phone the claims leave the card with a line saying the peer no longer shares them — plain text, no error banner. Press enter and Bob grants again.

Said out loud: withdrawal closes further delivery and does not recall what was delivered. The promise is careful and honest. And nothing that remembers a withdrawal blocks what follows it.

---

## Act 7. The device left, the identity stayed

**Phone.** Airplane mode, the application still in view. Not the lock screen: on iOS a lock can end the process, and then something else is being measured.

**Presenter.** `demo/07-stand-in.sh` — Alice's laptop writes a new value of the granted claim with the time in the line, and the script waits for Bob to read it.

Said out loud: the phone established the connection and the phone published the grant — and availability does not depend on it. The identity's other device carries the value on its own.

Then take the phone out of airplane mode: it catches up by itself, with no second bring-up.

---

## Act 8. A party holding nothing obtains nothing

**Presenter.** `demo/08-outsider.sh` — Carol reads the very claim Bob reads and gets `409`; she tries to list anything at all of Alice's and gets `409`. Beside it, in the same output, Bob reads the value.

Said out loud: this is the tightest denial of the claim the whole demonstration is delivered to make. The other acts pair a connected party against what lies outside its own grant; this one pairs a party that holds nothing against everything.

Why a fourth node rather than a second identity on Bob's: access to a namespace is decided by the issuer and by the node, not by the identity a screen is set to. An outsider hosted on Bob's node would read the granted claim, and the denial would prove nothing.

---

## What is not shown — say it out loud

1. What the screens show lives in this device's storage, which holds the only copy. Deleting the application erases the directory together with the key.
2. An identity carries no key material: nothing here proves who a peer is.
3. The reconcile interval is a configured number, not a property of the network.
4. 2 of the 4 nodes are not phones, and their side of every act is shown in a terminal rather than on a screen.
5. Withdrawal closes further delivery and does not recall what was already delivered.

And what carries the traffic: the endpoint binds with the N0 preset — addresses are published under the node id to n0's public name servers, and a session that finds no direct path travels through n0's relay. Those servers see a node id, the addresses, and the times and sizes of traffic between 2 node ids. They see no content and hold nothing that names a person.

The conditions this run does not cover, also said out loud: a device that restarts and returns with its state; a disk that fills; a connection that degrades rather than ends; a capability narrowed and widened rather than closed and reopened; a process killed for memory; a withdrawal from a device other than the one that published the grant; a device joining after a connection already exists.

---

## When something goes wrong

| Symptom | Cause and what to do |
| --- | --- |
| `REFUSED · MALFORMED-INPUT`, subtitled "refused by this application before the node was called" | Bare JSON went into the QR. The phone expects base64url over it — draw codes only with the scripts, which do it themselves |
| `REFUSED · COUNTERPARTY-UNREACHABLE` | Local Network is off for the application (Settings → PDN), or the payload was minted before the home relay came up. The scripts wait for the relay; if the wait was long, mint again |
| The code on the screen will not read | Raise the scale in `pdnqr` (`-s 8`), kill the reflection, give the phone 20–30 cm |
| The ceremony hangs and times out | The payload expired: `lifetime_secs` ran out. Run the act's script again |
| A script prints "nothing arrived within 48 seconds" | Watch `tail -f tmp/pdn-alice.log` and `tmp/pdn-bob.log`: they show whether sync is running and with which peer |
| `FAILED · INTERNAL` on the screen | Restart the application through `demo/phone-log.sh` and watch its stderr |
| The reading screen is blank | The camera is denied to the application. Settings → PDN → Camera |

No node is restarted once the run has begun. A node comes back on its directory as itself but not on its address: the port is ephemeral, a restarted node is unreachable to every peer it had, and the screens cannot say so.

## A full reset between runs

```sh
demo/reset.sh   # the 3 directories and the application with its key; it asks first
demo/00-start.sh
```

Deleting the application erases the directory together with the node's key: the loss of the only copy, not a cache clear. Exactly what a clean run needs, and exactly what must not happen by accident.
