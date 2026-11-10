#!/bin/sh
# The KERI oracle (crates/kerita/oracle): keripy at the commit KERIPY_PIN
# names, in an image of its own, writing into the corpora of kerita and kerita-cesr.
#
#   keri-oracle.sh corpus   regenerate both crates' tests/corpus/keripy, and kerita-cesr's tests/corpus/cesr-rs
#   keri-oracle.sh fold     write kerita's KELs to its tests/corpus/crate and fold them in keripy
#   keri-oracle.sh check    both, failing on any difference from the corpora as they stood
#
# The image is tagged by the pin, so a moved pin builds a new image; the
# generators are its last layer, so an edit to one rebuilds that layer alone.
# The container runs as the caller, so what it writes is the caller's.
set -eu
cd "$(dirname "$0")/.."
crate=crates/kerita
cesr=crates/kerita-cesr
pin="$(cat "$crate/oracle/KERIPY_PIN")"
image="kerita-oracle:$pin"
docker build -q --build-arg "KERIPY_PIN=$pin" -t "$image" "$crate/oracle" >/dev/null
run() {
  docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp \
    -v "$PWD/$crate/tests/corpus:/corpus" -v "$PWD/$cesr/tests/corpus:/corpus-cesr" \
    "$image" "$@"
}
case "${1:-}" in
  corpus) run corpus /corpus /corpus-cesr ;;
  fold)
    # The crate writes its KELs first, then keripy folds them.
    KERITA_WRITE_CORPUS=1 cargo nextest run -p kerita -E 'test(crate_kels_are_the_committed_ones)' >/dev/null
    run fold /corpus/crate
    ;;
  check)
    # Copies of the corpora as they stood, so the comparison holds whatever git has committed.
    before="$(mktemp -d)"
    trap 'rm -rf "$before"' EXIT
    mkdir "$before/kerita" "$before/kerita-cesr"
    cp -R "$crate/tests/corpus/." "$before/kerita"
    cp -R "$cesr/tests/corpus/." "$before/kerita-cesr"
    sh "$0" corpus
    sh "$0" fold
    status=0
    diff -r "$before/kerita" "$crate/tests/corpus" || status=1
    diff -r "$before/kerita-cesr" "$cesr/tests/corpus" || status=1
    if [ "$status" -ne 0 ]; then
      echo "the corpora regenerated from keripy at $pin differ from the ones that stood" >&2
      exit 1
    fi
    ;;
  *) sed -n '5,7p' "$0" | sed 's/^# *//' >&2; exit 2 ;;
esac
