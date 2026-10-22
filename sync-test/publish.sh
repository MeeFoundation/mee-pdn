#!/bin/sh
# One load run against the history branch, which GitHub Pages serves.
#
#   sh publish.sh <run dir>          report.md against the latest earlier run of the same profile, printed
#   sh publish.sh <run dir> push     the same, then the run and the rebuilt index.html committed and pushed
#
# The branch (HISTORY_BRANCH, default load-history) is checked out detached into a temporary worktree, or begun as an
# orphan when origin has none yet, so the caller's own checkout and branches stay as they are. With
# GITHUB_STEP_SUMMARY set the report goes there too.
set -eu
run=$(cd "$1" && pwd)
cd "$(dirname "$0")"
push=${2:-}
branch=${HISTORY_BRANCH:-load-history}
export HISTORY_BRANCH="$branch"
tmp=$(mktemp -d)
tree="$tmp/history"
orphan="publish-$$"
cleanup() {
  git worktree remove --force "$tree" 2>/dev/null || true
  git branch -q -D "$orphan" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT

if git fetch -q origin "+refs/heads/$branch:refs/remotes/origin/$branch" 2>/dev/null; then
  git worktree add -q --detach "$tree" "origin/$branch"
else
  echo "origin has no $branch: this run begins it"
  git worktree add -q --detach "$tree"
  git -C "$tree" switch -q --orphan "$orphan"
fi

id=$(basename "$run")
python3 -I report.py --synthetic
python3 -I report.py report "$run" --history "$tree" --out "$tree/runs/$id" > "$tmp/report.md"
cat "$tmp/report.md"
[ -n "${GITHUB_STEP_SUMMARY:-}" ] && cat "$tmp/report.md" >> "$GITHUB_STEP_SUMMARY"
python3 -I report.py index "$tree"

[ "$push" = push ] || exit 0
git -C "$tree" add -A
git -C "$tree" -c user.name='github-actions[bot]' \
  -c user.email='41898282+github-actions[bot]@users.noreply.github.com' commit -q -m "load: $id"
git -C "$tree" push -q origin "HEAD:refs/heads/$branch"
echo "pushed $id to $branch"
