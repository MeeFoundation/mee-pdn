#!/bin/sh
# The pinned toolchain (rust-toolchain.toml) and every tool tools.toml pins.
# A tool goes into cargo's bin directory with `cargo install`; a copy another
# manager put elsewhere is left to that manager — a copy installed beside it
# loses or wins by PATH order, not by version — and the report names the
# commands that bring it to the pin.
set -eu
cd "$(dirname "$0")/.."
# Channel, components and targets from rust-toolchain.toml.
(set -x; rustup toolchain install)
bin="${CARGO_HOME:-$HOME/.cargo}/bin"
failed=0
summary=""

# A cargo subcommand's binary takes its own name as the first argument.
version_of() {
  case "$1" in
    cargo-*) "$2" "${1#cargo-}" --version 2>/dev/null | awk 'NR == 1 { print $2 }' ;;
    *) "$2" --version 2>/dev/null | awk 'NR == 1 { print $2 }' ;;
  esac
}

# Who put a copy there: homebrew, cargo, or another manager.
owner_of() {
  case "$(realpath "$1" 2>/dev/null || echo "$1")" in
    */Cellar/*) echo homebrew ;;
    "$(realpath "$bin" 2>/dev/null || echo "$bin")"/*) echo cargo ;;
    *) echo other ;;
  esac
}

# The command that removes a copy without breaking its manager's bookkeeping.
removal() {  # tool, path
  case "$(owner_of "$2")" in
    homebrew) echo "brew uninstall $1" ;;
    cargo) echo "cargo uninstall $1" ;;
    *) echo "rm $2" ;;
  esac
}

# Every other executable of that name on PATH, each once.
shadowed() {
  (IFS=:; for d in $PATH; do [ -x "$d/$1" ] && [ "$d/$1" != "$2" ] && echo "$d/$1"; done) | awk '!seen[$0]++'
}

# The details go out at once; the one-line gist again at the end, where a
# later `cargo install` has not scrolled it away.
report() {  # fatal|warning, details, gist
  printf '\n%s\n' "$2" >&2
  summary="$summary
  $3"
  [ "$1" = warning ] || failed=1
}

# Assigned first: a failure inside `for … in $(…)` leaves an empty loop and a clean exit.
specs=$(sh scripts/tool-specs.sh)
for spec in $specs; do
  tool="${spec%@*}"; want="${spec#*@}"
  path="$(command -v "$tool" || true)"
  have=""
  [ -z "$path" ] || have="$(version_of "$tool" "$path")"
  for other in $(shadowed "$tool" "$path"); do
    report warning "$tool: a second copy is on PATH, $other ($(version_of "$tool" "$other")).
  It stays behind $path only while PATH keeps this order; remove it: $(removal "$tool" "$other")" \
      "$tool: second copy at $other — $(removal "$tool" "$other")"
  done
  if [ "$have" = "$want" ]; then echo "$tool $want: present ($path)"; continue; fi
  if [ -n "$path" ] && [ "$path" != "$bin/$tool" ]; then
    # With `just` itself gone, the recipe is gone too.
    rerun="just setup-tooling"; [ "$tool" = just ] && rerun="sh scripts/setup-tooling.sh"
    state="is $have"; [ -n "$have" ] || state="reports no version"
    case "$(owner_of "$path")" in
      homebrew) how="Homebrew installed it, so this does not replace it. Either:
    brew upgrade $tool
      Homebrew's current version, which matches the pin only if the two agree
    brew uninstall $tool && $rerun
      exactly $want, installed with cargo" ;;
      *) how="It was not installed with cargo, so this does not replace it. Either:
    update it to $want with whatever installed it
    remove it, then run $rerun
      exactly $want, installed with cargo" ;;
    esac
    report fatal "$tool: $path $state, tools.toml pins $want.
  $how" "$tool at $path $state, pinned $want — see above"
    continue
  fi
  # `--force`: cargo refuses to replace a binary it did not put there (a pre-built nextest).
  (set -x; cargo install "$spec" --locked --force) \
    || report fatal "$tool: cargo install $spec failed; cargo's output is above." "$tool: cargo install failed — see above"
done
if [ -n "$summary" ]; then
  [ "$failed" = 0 ] && verdict="done, with warnings" || verdict="not done"
  printf '\nsetup-tooling: %s:%s\n' "$verdict" "$summary" >&2
fi
exit "$failed"
