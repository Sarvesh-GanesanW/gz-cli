#!/usr/bin/env bash
# Runs `gz review` on the changed files listed (NUL separated) in $1 and
# writes its markdown to $2. Sets the step outputs `exit-code` and `verdict`;
# never fails itself, so the summary and comment steps still run.
#
# Env: GZ_DIR, GZ_PROJECT, GZ_COMPUTE, GZ_RTE, GZ_SQL_COMPUTE, GZ_FAIL_ON,
#      GZ_STRICT, GZ_KEEP_BRANCHES (inputs), GITHUB_OUTPUT.
set -uo pipefail

files="${1:?usage: review.sh FILES MARKDOWN}"
markdown="${2:?usage: review.sh FILES MARKDOWN}"
marker='<!-- gz-review -->'

finish() {
  echo "exit-code=$1" >>"${GITHUB_OUTPUT:?}"
  echo "verdict=$2" >>"$GITHUB_OUTPUT"
  exit 0
}

changed=()
while IFS= read -r -d '' file; do changed+=("$file"); done <"$files"
if [ "${#changed[@]}" -eq 0 ]; then
  printf '%s\n## gz review: **PASS**\n\nNo file in `%s` changed.\n' "$marker" "${GZ_DIR:-.}" >"$markdown"
  finish 0 pass
fi

args=(review --dir "${GZ_DIR:-.}" --format markdown)
[ -n "${GZ_PROJECT:-}" ] && args+=(--project "$GZ_PROJECT")
if [ -n "${GZ_COMPUTE:-}" ] || [ -n "${GZ_RTE:-}" ]; then
  args+=(--compute "${GZ_COMPUTE:-}" --rte "${GZ_RTE:-}")
fi
[ -n "${GZ_SQL_COMPUTE:-}" ] && args+=(--sql-compute "$GZ_SQL_COMPUTE")
[ -n "${GZ_FAIL_ON:-}" ] && args+=(--fail-on "$GZ_FAIL_ON")
[ "${GZ_STRICT:-false}" = true ] && args+=(--strict)
[ "${GZ_KEEP_BRANCHES:-false}" = true ] && args+=(--keep-branches)

gz "${args[@]}" -- "${changed[@]}" >"$markdown"
code=$?
case "$code" in
  0) verdict="$(grep -q '^## gz review: \*\*WARN\*\*' "$markdown" && echo warn || echo pass)" ;;
  2) verdict=fail ;;
  3) verdict=warn ;;
  *) verdict=error ;;
esac
if [ ! -s "$markdown" ]; then
  printf '%s\n## gz review: **ERROR**\n\ngz review stopped before it could report (exit code %s); the job log says why.\n' \
    "$marker" "$code" >"$markdown"
fi
finish "$code" "$verdict"
