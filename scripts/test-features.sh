#!/bin/sh
# The test-util feature flag for a cargo arg list. `pdn-node/test-util`
# enables the runtime's fault and pause hooks and turns on
# `data-layer/test-util`, data-layer's observation surface, under it. A
# selection gets the flag of the package it names, because cargo rejects a feature of a package it leaves
# out; one naming neither gets nothing — the scenarios that need the surface
# live in these two crates. Dev builds only — never a product build.
set -eu
case " $* " in
  *" -p pdn-node "*|*" --package pdn-node "*|*" --package=pdn-node "*) echo '--features pdn-node/test-util' ;;
  *" -p data-layer "*|*" --package data-layer "*|*" --package=data-layer "*) echo '--features data-layer/test-util' ;;
  *" -p "*|*" --package "*|*" --package="*) ;;
  *) echo '--features pdn-node/test-util' ;;
esac
