#!/usr/bin/env bash
# Installs the gz release the action is pinned to (or GZ_VERSION) for this
# runner, refusing it unless its sha256 matches the release's checksum file.
#
# Env: GZ_VERSION (a tag like v0.13.0; empty: the action's own tag),
#      ACTION_REF (the ref the workflow pinned the action to),
#      GZ_INSTALL_DIR (where gz goes), GZ_RELEASES (release download base,
#      for tests), RUNNER_OS, RUNNER_ARCH, GITHUB_PATH.
set -euo pipefail

releases="${GZ_RELEASES:-https://github.com/Sarvesh-GanesanW/gz-cli/releases/download}"
fail() { echo "::error title=gz install::$*" >&2; exit 1; }

version="${GZ_VERSION:-}"
# github.action_ref can be empty in composite actions; the runner also sets
# GITHUB_ACTION_REF.
action_ref="${ACTION_REF:-${GITHUB_ACTION_REF:-}}"
if [ -z "$version" ]; then
  case "$action_ref" in
    v[0-9]*) version="$action_ref" ;;
    *) fail "the action is used at '${action_ref:-?}', not a release tag; pin it to one (uses: Sarvesh-GanesanW/gz-cli@v0.13.0) or set the version input" ;;
  esac
fi
case "$version" in
  v[0-9]*) ;;
  [0-9]*) version="v$version" ;;
  *) fail "version '$version' is not a release tag like v0.13.0" ;;
esac

binary=gz
case "${RUNNER_OS:-}-${RUNNER_ARCH:-}" in
  Linux-X64) asset=gz-linux-x86_64 ;;
  macOS-ARM64) asset=gz-macos-arm64 ;;
  macOS-X64) asset=gz-macos-x86_64 ;;
  Windows-X64) asset=gz-windows-x86_64.exe; binary=gz.exe ;;
  *) fail "no gz release for ${RUNNER_OS:-?}/${RUNNER_ARCH:-?}: gz runs on Linux x64, macOS (arm64, x64) and Windows x64 runners" ;;
esac

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl="curl --proto =https --tlsv1.2 -fsSL --retry 3"
case "$releases" in file://*) curl="curl -fsSL" ;; esac

echo "Installing gz $version ($asset)"
$curl -o "$tmp/$asset" "$releases/$version/$asset" ||
  fail "cannot download $releases/$version/$asset; is $version a published release?"
$curl -o "$tmp/$asset.sha256" "$releases/$version/$asset.sha256" ||
  fail "cannot download the checksum of $asset for $version; nothing was installed"
expected="$(cut -d ' ' -f 1 <"$tmp/$asset.sha256")"
actual="$(sha256_of "$tmp/$asset")"
[ -n "$expected" ] && [ "$expected" = "$actual" ] ||
  fail "checksum mismatch for $asset (expected ${expected:-nothing}, got $actual); nothing was installed"
echo "sha256 matches the release: $actual"

install_dir="${GZ_INSTALL_DIR:?GZ_INSTALL_DIR is not set}"
mkdir -p "$install_dir"
mv "$tmp/$asset" "$install_dir/$binary"
chmod 755 "$install_dir/$binary"
"$install_dir/$binary" --version
echo "$install_dir" >>"${GITHUB_PATH:?GITHUB_PATH is not set}"
