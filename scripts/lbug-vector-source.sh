#!/bin/sh
# LadybugDB's sources with its vector extension linked in statically, for
# crates/graph-query-engine. The lbug crate ships the engine without its
# extensions and downloads them at run time, for desktop systems only;
# linked in, the vector extension loads with the database on every target,
# Android among them. The tree is assembled once, outside git, in
# crates/graph-query-engine/.lbug-src, which the workspace's
# .cargo/config.toml points the lbug crate's build at: the crate's own
# engine sources, LadybugDB's extensions repository at the commit v0.22.1
# pins, and a configuration that links `vector` and nothing else.
set -eu

version=0.22.1
extensions=119912e604ebbaad64072a701698d6fcf4140051
root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/crates/graph-query-engine/.lbug-src"

crate=$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/lbug-$version 2>/dev/null | head -1)
if [ -z "$crate" ]; then
  (cd "$root" && cargo fetch -q)
  crate=$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/lbug-$version | head -1)
fi

mkdir -p "$out"
rsync -a --delete --exclude /extension --exclude /build "$crate/lbug-src/" "$out/"

if [ "$(git -C "$out/extension" rev-parse HEAD 2>/dev/null || true)" != "$extensions" ]; then
  rm -rf "$out/extension"
  git init -q "$out/extension"
  git -C "$out/extension" remote add origin https://github.com/ladybugdb/extensions.git
  git -C "$out/extension" fetch -q --depth 1 origin "$extensions"
  git -C "$out/extension" checkout -q FETCH_HEAD
fi

printf '%s\n' \
  '# Written by scripts/lbug-vector-source.sh: the vector extension linked into the engine, no other built.' \
  'add_static_link_extension(vector)' \
  > "$out/extension/extension_config.cmake"

# The extensions' CMake writes an extension's archive into the source tree,
# where one target's build overwrites another's: into the build's own
# directory instead, beside liblbug.a, under the name a linker looks for.
marker='# graph-query-engine: the vector archive beside liblbug.a'
if ! grep -q "$marker" "$out/extension/CMakeLists.txt"; then
  printf '%s\n' \
    '' \
    "$marker" \
    'if (TARGET lbug_vector_static_extension)' \
    '    set_target_properties(lbug_vector_static_extension PROPERTIES' \
    '            ARCHIVE_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/src"' \
    '            OUTPUT_NAME "vector_static" PREFIX "lib" SUFFIX ".a")' \
    'endif ()' \
    >> "$out/extension/CMakeLists.txt"
fi

python3 -I "$root/scripts/lbug_ndk_atomic_ref.py" "$out"

echo "LadybugDB $version with the vector extension linked in: $out"
