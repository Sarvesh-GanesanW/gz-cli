#!/bin/sh
# Pushes rendered packaging files (a Homebrew tap's Formula/, a Scoop
# bucket's bucket/) to their repository, committing only when they changed.
#
#   PACKAGE_REPOS_TOKEN=... packaging/publish-repo.sh <owner/repo> <dir> <commit message>
#
# The token goes in an HTTP header, never in the remote URL, so it is not
# written to the clone's config. PACKAGE_REPOS_GIT_BASE (default
# https://github.com) exists so tests can push to a local repository.
set -eu

fail() { printf 'publish-repo.sh: %s\n' "$*" >&2; exit 1; }

[ $# -eq 3 ] || fail "usage: publish-repo.sh <owner/repo> <dir> <commit message>"
repo="$1"
src="$2"
message="$3"
base="${PACKAGE_REPOS_GIT_BASE:-https://github.com}"
[ -n "${PACKAGE_REPOS_TOKEN:-}" ] || fail "PACKAGE_REPOS_TOKEN is not set"
[ -d "$src" ] || fail "no rendered files at $src"

auth="$(printf 'x-access-token:%s' "$PACKAGE_REPOS_TOKEN" | base64 | tr -d '\n')"
git_authed() { git -c "http.extraheader=AUTHORIZATION: basic $auth" "$@"; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM
git_authed clone --quiet --depth 1 --no-checkout "$base/$repo.git" "$work/repo" ||
  fail "cannot clone $base/$repo (does it exist, and can the token write to it?)"
cd "$work/repo"
# Build on main whatever the remote's default branch is; a repository
# created empty has no main yet, and the first push makes it.
if git_authed fetch --quiet --depth 1 origin main 2>/dev/null; then
  git checkout --quiet -B main FETCH_HEAD
else
  git checkout --quiet --orphan main
fi
cp -R "$src"/. .
git add -A
if git diff --cached --quiet; then
  printf '%s already has these files; nothing to push\n' "$repo"
  exit 0
fi
git -c user.name="gz release" -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
  commit --quiet -m "$message"
git_authed push --quiet origin main
printf 'pushed %s to %s\n' "$message" "$repo"
