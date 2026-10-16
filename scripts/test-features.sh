#!/bin/sh
# The dev feature flags for a cargo arg list. `pdn-node/test-util` enables
# the runtime's fault and pause hooks and turns on `data-layer/test-util`,
# data-layer's observation surface, under it; `pdn-node-http/observation`
# serves that surface over HTTP, and `pdn-mobile/observation` on a phone's
# loopback. A selection gets the flag of the package it names, because cargo
# rejects a feature of a package it leaves out; one naming none of them gets
# nothing. Dev builds only — never a product build.
set -eu
case " $* " in
  *" -p pdn-node "*|*" --package pdn-node "*|*" --package=pdn-node "*) echo '--features pdn-node/test-util' ;;
  *" -p data-layer "*|*" --package data-layer "*|*" --package=data-layer "*) echo '--features data-layer/test-util' ;;
  *" -p pdn-node-http "*|*" --package pdn-node-http "*|*" --package=pdn-node-http "*) echo '--features pdn-node-http/observation' ;;
  *" -p pdn-mobile "*|*" --package pdn-mobile "*|*" --package=pdn-mobile "*) echo '--features pdn-mobile/observation' ;;
  *" -p "*|*" --package "*|*" --package="*) ;;
  *) echo '--features pdn-node/test-util,pdn-node-http/observation,pdn-mobile/observation' ;;
esac
