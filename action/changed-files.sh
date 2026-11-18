#!/usr/bin/env bash
# Writes the files a pull request changed inside the gz sync folder, NUL
# separated, to $1: the diff between the base and HEAD from where they
# forked, so commits made to the base since then are not counted.
#
# Env: BASE_REF (the base commit, e.g. the pull request's base sha),
#      GZ_DIR (the sync folder, relative to the repository root).
set -euo pipefail

out="${1:?usage: changed-files.sh OUTPUT}"
fail() { echo "::error title=gz review::$*" >&2; exit 1; }
[ -n "${BASE_REF:-}" ] || fail "no base to compare with: run the action on pull_request events, or set base-ref"

if ! git cat-file -e "${BASE_REF}^{commit}" 2>/dev/null; then
  git fetch --no-tags --quiet origin "$BASE_REF" ||
    fail "cannot fetch $BASE_REF; check the repository out with fetch-depth: 0"
fi
fork="$(git merge-base "$BASE_REF" HEAD)" ||
  fail "cannot find where HEAD left $BASE_REF; check the repository out with fetch-depth: 0 (actions/checkout)"

dir="${GZ_DIR:-.}"
dir="${dir#./}"
dir="${dir%/}"
: >"$out"
while IFS= read -r -d '' file; do
  case "$dir" in
    "" | .) printf '%s\0' "$file" >>"$out" ;;
    *) case "$file" in "$dir"/*) printf '%s\0' "$file" >>"$out" ;; esac ;;
  esac
done < <(git diff -z --name-only --no-renames "$fork" HEAD)
echo "$(tr -cd '\0' <"$out" | wc -c | tr -d ' ') changed file(s) under ${dir:-.}"
