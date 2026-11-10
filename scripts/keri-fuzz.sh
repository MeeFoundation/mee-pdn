#!/bin/sh
# Fuzzes each target of crates/kerita/fuzz for the seconds given, on the
# nightly toolchain FUZZ_TOOLCHAIN names (the workspace's own toolchain is
# stable, and cargo-fuzz needs nightly). Each target starts from the inputs
# kept under tests/corpus/fuzz/; what it learns goes to fuzz/corpus/, and a
# crashing input to fuzz/artifacts/, where it stops the run. The targets build
# from the committed Cargo.lock, which `just audit` checks: cargo-fuzz takes no
# --locked, so a fetch that does fails first when the lock no longer fits.
set -eu
cd "$(dirname "$0")/../crates/kerita/fuzz"
seconds="${1:-60}"
toolchain="${FUZZ_TOOLCHAIN:-nightly}"
cargo "+$toolchain" fetch --locked
for target in primitive attachments event stream; do
  mkdir -p "corpus/$target"
  cargo "+$toolchain" fuzz run "$target" "corpus/$target" "../tests/corpus/fuzz/$target" \
    -- -max_total_time="$seconds"
done
