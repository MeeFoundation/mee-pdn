# Demo scripts

Half the laptop, laid out step by step. Order is in the names: run left to right, one per act of the scenario (`../demo-script-en.md`).

The cast is four nodes, one of them a phone. Three `pdn-node-http` processes live on this machine: **alice** (3011) — the device where Alice creates her identity and writes her first entries; **bob** (3012) — the peer she establishes a connection with; **carol** (3013) — the one who holds nothing of Alice's, and shows it. The phone joins Alice's identity through the linking ceremony and remains the only physical device in the staging.

Every node can be driven from a browser through the same screens as the phone, and the scripts open the right one themselves: `pdnbrowser` (in `lib.sh`) switches the tab to the act's node if it is not already the one showing, and does nothing if it already is — the page's own reads keep it current. All that is needed is `cd pdn-app && npm run web` running once, before the scripts start. Behind the screens in this case is not the facade but the HTTP host's debug surface, and two things differ: the node came up where the process was started, so the page neither brings it up nor stops it, and a refusal arrives as a status and a phrase, from which the kind of refusal is read back out.

State between runs lives in files, not tab variables: each node's identity in `tmp/<name>-id`, Alice's second identity in `tmp/alice-other-id`, the node whose screen is open in the browser in `tmp/browser-node`. So steps can be run from any tab, and with breaks between them.

| Script | What it does | What happens on the phone meanwhile |
| --- | --- | --- |
| `00-start.sh` | Brings up the three nodes, mints each one's identity and Alice's second identity, seeds base entries for Alice (3) and Bob (2) | — |
| `preflight.sh` | Confirms the tooling is in place, the QR path is intact, and every node has a relay address | — |
| `01-alice.sh` | Opens the browser on Alice, shows the node id, both identities, and the entries `00-start.sh` seeded | — (the phone has joined nothing yet) |
| `02-link-phone.sh` | Mints a linking payload for Alice's identity and draws it as a QR | Bring the node up — and nothing else; then Add an identity → Receive one from another device → Joining this device to an identity |
| `03-connect-bob.sh` | Bob mints an invite and draws it; waits for the connection on Bob's side, switches the browser to Alice, and waits for Bob to appear on her laptop | Connections → Connect to someone → Read a code → Accepting an invitation to connect |
| `04-alice-grants.sh` | Switches the browser to Bob; waits for Alice's grant, reads the value on his node, shows the ungranted path | Share claims with this peer → Grant read-only |
| `05-bob-grants.sh` | Bob publishes 2 claims over the seeded entries, the second with the right to write; his granting is watched on his own browser screen; waits for what the phone writes | The card What this peer shares with me, the write a new value field |
| `06-withdraw.sh` | Withdrawal and re-grant in both directions, Bob's side watched on his own browser screen | Withdraw this grant, then Grant read-only again |
| `07-grant-follows-identity.sh` | Switches the browser to Alice; her laptop, which took no part in either grant, already reads what Bob shares and picks up a change he makes to it on its own | — |
| `08-stand-in.sh` | Writes from Alice's laptop while the phone is in airplane mode; the browser on Bob shows the value arriving | Airplane mode, application in view |
| `09-outsider.sh` | Switches the browser to Carol; she receives nothing, while Bob reads the granted claim on his own screen alongside | — |
| `phone-log.sh` | Launches the application and holds its stderr | The application restarts |
| `reset.sh` | Erases the three directories and reinstalls the application | The application is removed along with the node's key |

Node connectivity is chosen by the `CONNECTIVITY` variable, `product` by default — relays and address lookup, so the phone is reachable from any network. When the phone itself is the one sharing internet, all 4 nodes are on one network, and `CONNECTIVITY=direct demo/00-start.sh` reaches every peer directly, with no third party. That also removes what has to be said aloud under `product`: the published node id answers anyone asking whether the device is up and which relay it lives on.

Places where a script waits, and this is normal:

- `02-link-phone.sh` and `03-connect-bob.sh` wait up to half a minute for a home relay. In the first seconds after a node comes up its code carries only local addresses, and the phone is usually not on the laptop's network.
- `04-alice-grants.sh` waits around ten seconds for the grant and the value: the grant record travels first, and the payload separately after it.
- `03-connect-bob.sh`'s second wait is for a periodic pass: the connection the phone established reaches Alice's laptop on its own.

`06-withdraw.sh` and `08-stand-in.sh` stop and wait for enter — at the point where the next step is a person acting on the phone.

`reset.sh` asks for confirmation: it erases the three nodes' directories and the application along with its key, which is the loss of the only copy, not a cache clear.

Nodes are not restarted mid-run. A node returns to its own directory on its own, but not to its own address — the port is ephemeral — and a restarted node is unreachable to everyone it was talking to, which the screens have no way to say: a replica holds the last value that arrived and does not report the age of a value.

## The signing profile lives seven days

A free provisioning profile lasts a week. An expired one fails the install with a message about an embedded profile, which reads like a signing-configuration error though it is simply an expired one. `reset.sh` checks the expiry before installing and, if it has passed, prints the rebuild command:

```sh
cd pdn-app/ios && xcodebuild -workspace PDN.xcworkspace -scheme PDN \
  -configuration Release -destination 'generic/platform=iOS' -allowProvisioningUpdates build
```

`-allowProvisioningUpdates` renews the profile on its own. The build lands in `~/Library/Developer/Xcode/DerivedData/PDN-*/Build/Products/Release-iphoneos/PDN.app`, and `reset.sh` takes the newest one from there.
