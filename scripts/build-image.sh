#!/bin/sh
# The node image, tagged with the name given, built with the host's features
# named second (none for the stand's image). A caller whose TLS goes
# through an intercepting proxy names the bundle holding its CA in
# SSL_CERT_FILE, as the devcontainer does; the build's cargo steps then trust
# that bundle instead of the image's own roots.
set -eu
cd "$(dirname "$0")/.."
image=$1
features=${2:-}
if [ -n "${SSL_CERT_FILE:-}" ] && [ -f "$SSL_CERT_FILE" ]; then
  set -- --secret "id=ca-bundle,src=$SSL_CERT_FILE" --build-arg CARGO_HTTP_CAINFO=/run/secrets/ca-bundle
else
  set --
fi
set -x
DOCKER_BUILDKIT=1 docker build -f ops/Dockerfile -t "$image" --build-arg "FEATURES=$features" "$@" .
