#!/bin/bash
# A full reset between runs. Erases the node's directory and the application on
# the phone together with its key: the loss of the only copy, not a cache clear.
source "$(dirname "$0")/lib.sh"

read -r -p "erase the laptop node and the application on the phone? [y/N] " answer
[ "$answer" = "y" ] || { echo "cancelled"; exit 0; }

pkill -f 'target/debug/pdn-node-http'
for i in $(seq 1 15); do pgrep -f 'target/debug/pdn-node-http' >/dev/null || break; sleep 1; done
rm -rf "$NODE_DIR" "$PDN/tmp/peer-id" "$PDN/tmp/mac-identity"
mkdir -p "$NODE_DIR"
echo "the laptop directory is clean"

xcrun devicectl device uninstall app --device "$DEV" "$BUNDLE" 2>&1 | tail -1
APP=$(ls -dt ~/Library/Developer/Xcode/DerivedData/PDN-*/Build/Products/Release-iphoneos/PDN.app 2>/dev/null | head -1)
if [ -z "$APP" ]; then
  echo "no built PDN.app found — see below for how to build one" >&2
  APP=""
else
  # A free provisioning profile lives seven days. An expired one fails the
  # install with a message about an embedded profile, which reads like a
  # signing misconfiguration rather than a clock running out.
  expires=$(security cms -D -i "$APP/embedded.mobileprovision" 2>/dev/null | plutil -extract ExpirationDate raw - 2>/dev/null)
  if [ -n "$expires" ] && [ "$expires" \< "$(date -u +%Y-%m-%dT%H:%M:%SZ)" ]; then
    echo "the profile in that build expired at $expires — it will not install" >&2
    APP=""
  fi
fi

if [ -n "$APP" ]; then
  xcrun devicectl device install app --device "$DEV" "$APP" 2>&1 | grep -E "App installed|error"
else
  echo "rebuild with a renewed profile, then run this script again:" >&2
  echo "  cd $PDN/pdn-app/ios && xcodebuild -workspace PDN.xcworkspace -scheme PDN \\" >&2
  echo "    -configuration Release -destination 'generic/platform=iOS' -allowProvisioningUpdates build" >&2
fi
