#!/bin/sh
# The mobile facade for one target — what `pdn-sdk` packages, built here
# because the crate lives here. The default target is the Android one, the
# only of the two a machine that is not a Mac can produce.
#
# Both halves need a target C compiler for `ring`'s build script: on Apple
# platforms `xcrun` finds it, on Android `cc-rs` looks for a bare
# `aarch64-linux-android-clang` and the NDK ships only API-level-suffixed
# wrappers, so the caller names one. TARGET_API selects it.
set -eux
cd "$(dirname "$0")/.."
target="${1:-aarch64-linux-android}"
case "$target" in
  *android*)
    : "${ANDROID_NDK_HOME:?set it to the NDK the build compiles ring with}"
    api="${TARGET_API:-24}"
    host="$(uname -s | tr '[:upper:]' '[:lower:]')-x86_64"
    bin="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$host/bin"
    CC_aarch64_linux_android="$bin/aarch64-linux-android$api-clang"
    AR_aarch64_linux_android="$bin/llvm-ar"
    export CC_aarch64_linux_android AR_aarch64_linux_android
    # The shared library an application loads.
    kind=cdylib
    ;;
  # An iOS application links the static library. What it also needs is
  # `-framework Network -framework SystemConfiguration`, which Xcode does
  # not add for a framework referenced only from an opaque archive — a
  # link setting of the application's, not of this build.
  *apple*) kind=staticlib ;;
  *) echo "not a mobile target: $target"; exit 1 ;;
esac
cargo rustc -p pdn-mobile --release --target "$target" --crate-type "$kind"
