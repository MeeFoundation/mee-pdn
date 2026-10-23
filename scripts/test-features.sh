#!/bin/sh
# The dev feature flags for a cargo arg list. `pdn-node/test-util` enables
# the runtime's fault and pause hooks and turns on `data-layer/test-util`,
# data-layer's observation surface, which turns on the store's ordered
# subscription (`pdn-store/observation`); `pdn-node-http/observation`
# serves that surface over HTTP, and `pdn-mobile/observation` on a phone's
# loopback. A selection gets the flag of every package it names, because
# cargo rejects a feature of a package it leaves out; one naming none of
# them gets nothing. Dev builds only — never a product build.
set -eu
features=
selected=false
expecting=false
for arg in "$@"; do
  if $expecting; then
    expecting=false
    name="$arg"
  else
    case "$arg" in
      -p|--package) expecting=true; selected=true; continue ;;
      --package=*) name="${arg#--package=}"; selected=true ;;
      *) continue ;;
    esac
  fi
  case "$name" in
    pdn-node) flag=pdn-node/test-util ;;
    data-layer) flag=data-layer/test-util ;;
    pdn-store|pdn-node-http|pdn-mobile) flag="$name/observation" ;;
    *) continue ;;
  esac
  features="${features:+$features,}$flag"
done
if ! $selected; then
  features=pdn-node/test-util,pdn-node-http/observation,pdn-mobile/observation
fi
if [ -n "$features" ]; then
  echo "--features $features"
fi
