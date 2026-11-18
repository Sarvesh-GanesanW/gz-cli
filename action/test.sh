#!/usr/bin/env bash
# Tests the action's shell steps locally with stubbed gz, gh and releases:
#   action/test.sh [path/to/gz]     (default target/debug/gz)
# Nothing leaves the machine; everything happens in a temporary folder.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
gz_binary="${1:-$here/../target/debug/gz}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
failures=0

check() {
  local name="$1"
  shift
  if "$@"; then
    echo "ok    $name"
  else
    echo "FAIL  $name"
    failures=$((failures + 1))
  fi
}

# Succeeds when the command fails.
refuses() { ! "$@"; }

# ---------- install-gz.sh ----------

releases="$work/releases"
mkdir -p "$releases/v9.9.9" "$work/bin"
cp "$gz_binary" "$releases/v9.9.9/gz-macos-arm64"
(cd "$releases/v9.9.9" && shasum -a 256 gz-macos-arm64 >gz-macos-arm64.sha256)

install_gz() {
  env -i PATH="$PATH" HOME="$work" GZ_RELEASES="file://$releases" RUNNER_OS=macOS RUNNER_ARCH=ARM64 \
    GZ_INSTALL_DIR="$work/bin" GITHUB_PATH="$work/github_path" "$@" bash "$here/install-gz.sh" >"$work/install.log" 2>&1
}
check "installs the release the action is pinned to" install_gz ACTION_REF=v9.9.9
check "  and puts it on PATH" grep -qx "$work/bin" "$work/github_path"
check "  and it runs" "$work/bin/gz" --version
check "the version input wins" install_gz ACTION_REF=main GZ_VERSION=9.9.9
check "refuses an action not pinned to a tag" refuses install_gz ACTION_REF=main
check "  saying how to pin it" grep -q "not a release tag" "$work/install.log"
echo "0000 gz-macos-arm64" >"$releases/v9.9.9/gz-macos-arm64.sha256"
check "refuses a download whose checksum doesn't match" refuses install_gz ACTION_REF=v9.9.9
check "  saying so" grep -q "checksum mismatch" "$work/install.log"
rm "$releases/v9.9.9/gz-macos-arm64.sha256"
check "refuses a release without a checksum" refuses install_gz ACTION_REF=v9.9.9
check "refuses a runner gz has no release for" refuses install_gz ACTION_REF=v9.9.9 RUNNER_OS=Linux RUNNER_ARCH=ARM64

# ---------- sign-in.sh ----------

stubs="$work/stubs"
mkdir -p "$stubs"
cat >"$stubs/gz" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$@" >"$STUB_LOG.args"
cat >"$STUB_LOG.stdin"
exit "${STUB_EXIT:-0}"
STUB
chmod +x "$stubs/gz"

sign_in() {
  env PATH="$stubs:$PATH" STUB_LOG="$work/login" GZ_CLIENT=stage GZ_SITE=preview \
    GZ_USERNAME=ci-bot GZ_PASSWORD='p@ss word' "$@" bash "$here/sign-in.sh" >/dev/null 2>&1
}
check "signs in with the password on stdin" sign_in
check "  never on the command line" bash -c '! grep -q "p@ss" "$1"' _ "$work/login.args"
check "  exactly as given" bash -c '[ "$(head -n 1 "$1")" = "p@ss word" ]' _ "$work/login.stdin"
check "  with --password-stdin" grep -qx -- "--password-stdin" "$work/login.args"
check "sends the current authenticator code after it" sign_in GZ_MFA_SECRET=JBSWY3DPEHPK3PXP
check "  as six digits" bash -c 'sed -n 2p "$1" | grep -Eqx "[0-9]{6}"' _ "$work/login.stdin"
check "refuses to run without a password" refuses sign_in GZ_PASSWORD=

# ---------- changed-files.sh ----------

repo="$work/repo"
git init -q -b main "$repo"
(
  cd "$repo"
  git config user.email ci@example.com
  git config user.name ci
  mkdir -p jobs/etl other
  echo a >jobs/etl/a.py
  echo b >"jobs/etl/b c.py"
  echo x >other/x.py
  git add . && git commit -qm base
  git checkout -qb change
  echo a2 >jobs/etl/a.py
  git rm -q "jobs/etl/b c.py"
  echo y >other/x.py
  git commit -qam change
  git checkout -q main
  echo later >jobs/etl/later.py
  git add . && git commit -qm later
  git checkout -q change
)
changed() {
  (cd "$repo" && env BASE_REF="$(git -C "$repo" rev-parse main)" GZ_DIR="$1" bash "$here/changed-files.sh" "$work/files" >/dev/null)
}
listed() { tr '\0' '\n' <"$work/files"; }
check "lists the files changed under the folder, removed ones too" changed ./jobs/
check "  and nothing else" bash -c '[ "$(tr "\0" "\n" <"$1" | sort | paste -sd "|" -)" = "jobs/etl/a.py|jobs/etl/b c.py" ]' _ "$work/files"
check "the whole repository with dir ." changed .
check "  counts every changed file" bash -c '[ "$(tr -cd "\0" <"$1" | wc -c | tr -d " ")" = 3 ]' _ "$work/files"

# ---------- review.sh ----------

cat >"$stubs/gz" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$@" >"$STUB_LOG.args"
[ -n "${STUB_MARKDOWN:-}" ] && printf '%s\n' "$STUB_MARKDOWN"
exit "${STUB_EXIT:-0}"
STUB
review() {
  : >"$work/output"
  env PATH="$stubs:$PATH" STUB_LOG="$work/review" GITHUB_OUTPUT="$work/output" GZ_DIR=jobs "$@" \
    bash "$here/review.sh" "$work/files" "$work/review.md" >/dev/null 2>&1
}
printf 'jobs/etl/a.py\0jobs/etl/b c.py\0' >"$work/files"
check "reviews the changed files" review GZ_COMPUTE=1003 GZ_RTE=rte:1 GZ_STRICT=true STUB_MARKDOWN='## gz review: **PASS** x'
check "  each one as one argument" bash -c 'tail -n 2 "$1" | paste -sd "|" - | grep -qx "jobs/etl/a.py|jobs/etl/b c.py"' _ "$work/review.args"
check "  with the try and strict flags" bash -c 'grep -qx -- "--strict" "$1" && grep -qx -- "--compute" "$1" && grep -qx -- "markdown" "$1"' _ "$work/review.args"
check "  and reports pass" grep -qx "verdict=pass" "$work/output"
check "a warn that exits 0 is a warn" review STUB_MARKDOWN='## gz review: **WARN** x'
check "  reported as such" grep -qx "verdict=warn" "$work/output"
check "exit 2 is a fail" review STUB_EXIT=2 STUB_MARKDOWN='## gz review: **FAIL** x'
check "  reported as such" bash -c 'grep -qx "verdict=fail" "$1" && grep -qx "exit-code=2" "$1"' _ "$work/output"
check "a review that printed nothing still leaves a report" review STUB_EXIT=1
check "  saying it stopped" grep -q "stopped before it could report" "$work/review.md"
check "  reported as an error" grep -qx "verdict=error" "$work/output"
: >"$work/files"
check "no changed file passes without running gz" review STUB_EXIT=9
check "  saying so" grep -q "No file in" "$work/review.md"

# ---------- comment.sh ----------

cat >"$stubs/gh" <<'STUB'
#!/usr/bin/env bash
echo "$*" >>"$STUB_LOG"
for arg in "$@"; do
  case "$arg" in body=@*) cp "${arg#body=@}" "$STUB_LOG.body" ;; esac
done
case "$*" in *"--paginate"*) [ -n "${STUB_EXISTING:-}" ] && echo "$STUB_EXISTING" ;; esac
exit 0
STUB
chmod +x "$stubs/gh"
comment() {
  rm -f "$work/gh" "$work/gh.body"
  env PATH="$stubs:$PATH" STUB_LOG="$work/gh" GITHUB_REPOSITORY=acme/jobs PR_NUMBER=7 "$@" \
    bash "$here/comment.sh" "$work/review.md" >/dev/null 2>&1
}
printf '<!-- gz-review -->\n## gz review: **PASS**\n' >"$work/review.md"
check "adds a comment when there is none" comment
check "  on the pull request" grep -q "POST repos/acme/jobs/issues/7/comments" "$work/gh"
check "  with the review" cmp -s "$work/review.md" "$work/gh.body"
check "updates the comment it made before" comment STUB_EXISTING=42
check "  in place" bash -c 'grep -q "PATCH repos/acme/jobs/issues/comments/42" "$1" && ! grep -q POST "$1"' _ "$work/gh"
head -c 70000 /dev/zero | tr '\0' 'x' >"$work/review.md"
check "cuts a review too long for a comment" comment
check "  to GitHub's limit" bash -c '[ "$(wc -c <"$1")" -lt 65536 ]' _ "$work/gh.body"

echo
if [ "$failures" -gt 0 ]; then
  echo "$failures check(s) failed"
  exit 1
fi
echo "all checks passed"
