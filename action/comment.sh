#!/usr/bin/env bash
# Puts the review on the pull request as one comment, updated on every run:
# the comment that starts with the review's marker is edited, else one is
# added. Needs GH_TOKEN with pull-requests: write.
#
# Env: GH_TOKEN, GITHUB_REPOSITORY, PR_NUMBER.
set -euo pipefail

markdown="${1:?usage: comment.sh MARKDOWN}"
marker='<!-- gz-review -->'
# GitHub refuses comment bodies over 65,536 characters.
limit=65000

if [ -z "${PR_NUMBER:-}" ]; then
  echo "Not a pull request: no comment to update."
  exit 0
fi
body="$(mktemp)"
trap 'rm -f "$body"' EXIT
if [ "$(wc -c <"$markdown")" -gt "$limit" ]; then
  head -c "$limit" "$markdown" >"$body"
  printf '\n\n… cut to fit a comment; the job summary has the whole review.\n' >>"$body"
else
  cp "$markdown" "$body"
fi

repo="${GITHUB_REPOSITORY:?}"
existing="$(gh api --paginate "repos/$repo/issues/$PR_NUMBER/comments" \
  --jq ".[] | select(.body | startswith(\"$marker\")) | .id" | head -n 1)"
if [ -n "$existing" ]; then
  gh api --silent -X PATCH "repos/$repo/issues/comments/$existing" -F "body=@$body"
  echo "Updated the review comment ($existing)."
else
  gh api --silent -X POST "repos/$repo/issues/$PR_NUMBER/comments" -F "body=@$body"
  echo "Added the review comment."
fi
