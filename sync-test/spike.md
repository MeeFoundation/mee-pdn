# Network spike

`plan.md` step 1 and task 3.1 of the `sync-observation` change: whether a phone joins a pod on the invite of a laptop node on direct paths, and whether the coordinator reaches a server on the phone's loopback over the cable. Run on 2026-10-15.

## Setup

- Phone: iPhone 15 Pro Max, on the cable and on the laptop's Wi-Fi. `pdn-app` built in the release configuration against `pdn-sdk` 0.2.3-dev, packaged from `mee-pdn` 14ad3c5; the facade brings its node up with the product connectivity, relays and address lookup.
- Laptop: one `pdn-node-http` built with `--release --features observation` from the same commit, `PDN_CONNECTIVITY=direct`, `PDN_BIND_ADDR` unset, on 192.168.1.18 of the network 192.168.1.0/24.
- The phone's loopback check used a server added to the native module for the spike and removed after it: a TCP listener bound to `127.0.0.1` at the port `PDN_SPIKE_PORT` names, answering `pong` and the device's wall clock in microseconds.

## Findings

1. **A direct-mode node's invite carries its local network address.** The pod invite's `inviter_addr` held `192.168.1.18:51308`, beside `192.168.64.1:51308` and 2 IPv6 addresses.
2. **The phone joined on that invite.** The code — base64url, unpadded, of the invite's JSON — was read in the application's join screen, and the laptop node then listed the phone's identity as a member of the pod.
3. **Sessions run on direct paths.** The laptop node's relay is disabled, so nothing reaches it but directly; its event stream showed the phone's node dialing it on both stores, every session finishing `ok`.
4. **A record crosses from the laptop to the phone.** A record placed on the laptop node showed on the phone's pod screen. The other direction was not checked: no record was placed on the phone. Test P needs only this direction, since its measured node writes nothing.
5. **The launch environment reaches the application.** `xcrun devicectl device process launch --terminate-existing --environment-variables '{"PDN_SPIKE_PORT":"3011"}' --device <udid> org.mee.pdn.app` started the application with the variable in `ProcessInfo.processInfo.environment`, and the server came up with no touch on the phone.
6. **A usbmux tunnel reaches the phone's loopback, and the local network does not.** With `iproxy -l -u <udid> 3311:3011` running, a connection to the laptop's `127.0.0.1:3311` was answered by the server on the phone's `127.0.0.1:3011`; a connection to `192.168.1.11:3011`, the phone's local network address, was refused, while the phone answered ICMP there.
7. **The phone's clock needs correcting, and the cable bounds the correction tightly.** 20 pings through the tunnel, each a new TCP connection, gave a best round trip of 2.3 ms and the phone's clock 51.6 ms behind the laptop's, within ±1.2 ms. ICMP round trips to the phone over the Wi-Fi ranged from 9 to 53 ms. An offset of 52 ms is the size of the laptop's own steady latencies, so test P's latency without it would describe the clocks.

## What took a person

Signing into Xcode with the account that signs the application, and trusting the developer profile on the phone, both once per machine and profile; a free profile lives 7 days, and this one expires on 2026-10-22. Joining the pod and reading the laptop's record on the phone were done by hand for the spike only: in test P the coordinator does both through the test surface.
