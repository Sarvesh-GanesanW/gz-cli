#!/bin/sh
# Renders the Homebrew formula, Scoop manifest and winget manifests for one
# gz release, with that release's sha256 checksums pinned.
#
#   packaging/render.sh <version> <out-dir> [checksum-dir]
#
# <version> is 0.10.0 or v0.10.0. Checksums come from <checksum-dir>/<asset>.sha256
# when given (the release workflow's build output), else from the published
# release on GitHub. Output:
#   <out-dir>/homebrew/Formula/gz.rb
#   <out-dir>/scoop/bucket/gz.json
#   <out-dir>/winget/manifests/g/Groundzero/gz/<version>/*.yaml
set -eu

REPO="Sarvesh-GanesanW/gz-cli"
WINGET_ID="Groundzero.gz"
TEMPLATES="$(cd "$(dirname "$0")" && pwd)/templates"

fail() { printf 'render.sh: %s\n' "$*" >&2; exit 1; }

[ $# -ge 2 ] && [ $# -le 3 ] || fail "usage: render.sh <version> <out-dir> [checksum-dir]"
VERSION="${1#v}"
OUT="$2"
SUMS="${3:-}"
TAG="v$VERSION"
printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$' ||
  fail "'$1' is not a release version like 0.10.0"

# The sha256 of one release asset, checked to be 64 hex digits.
checksum() {
  asset="$1"
  if [ -n "$SUMS" ]; then
    [ -f "$SUMS/$asset.sha256" ] || fail "missing $SUMS/$asset.sha256"
    line="$(cat "$SUMS/$asset.sha256")"
  else
    url="https://github.com/$REPO/releases/download/$TAG/$asset.sha256"
    line="$(curl --proto =https --tlsv1.2 -fsSL "$url")" || fail "cannot download $url (is $TAG published?)"
  fi
  sum="$(printf '%s' "$line" | awk '{ print tolower($1); exit }')"
  printf '%s' "$sum" | grep -Eq '^[0-9a-f]{64}$' || fail "$asset.sha256 does not hold a sha256 ('$line')"
  printf '%s' "$sum"
}

SHA_MACOS_ARM64="$(checksum gz-macos-arm64)"
SHA_MACOS_X86_64="$(checksum gz-macos-x86_64)"
SHA_LINUX_X86_64="$(checksum gz-linux-x86_64)"
SHA_WINDOWS_X86_64="$(checksum gz-windows-x86_64.exe)"
SHA_WINDOWS_X86_64_UPPER="$(printf '%s' "$SHA_WINDOWS_X86_64" | tr 'a-f' 'A-F')"

# Fills one template; refuses to leave a placeholder behind.
render() {
  template="$1"
  dest="$2"
  mkdir -p "$(dirname "$dest")"
  sed \
    -e "s/@VERSION@/$VERSION/g" \
    -e "s/@TAG@/$TAG/g" \
    -e "s/@SHA_MACOS_ARM64@/$SHA_MACOS_ARM64/g" \
    -e "s/@SHA_MACOS_X86_64@/$SHA_MACOS_X86_64/g" \
    -e "s/@SHA_LINUX_X86_64@/$SHA_LINUX_X86_64/g" \
    -e "s/@SHA_WINDOWS_X86_64@/$SHA_WINDOWS_X86_64/g" \
    -e "s/@SHA_WINDOWS_X86_64_UPPER@/$SHA_WINDOWS_X86_64_UPPER/g" \
    "$TEMPLATES/$template" >"$dest"
  if grep -Eq '@[A-Z0-9_]+@' "$dest"; then
    fail "$dest still has a placeholder: $(grep -Eo '@[A-Z0-9_]+@' "$dest" | head -n 1)"
  fi
  printf '%s\n' "$dest"
}

WINGET_DIR="$OUT/winget/manifests/g/Groundzero/gz/$VERSION"
render gz.rb "$OUT/homebrew/Formula/gz.rb"
render gz.json "$OUT/scoop/bucket/gz.json"
render "winget/$WINGET_ID.yaml" "$WINGET_DIR/$WINGET_ID.yaml"
render "winget/$WINGET_ID.installer.yaml" "$WINGET_DIR/$WINGET_ID.installer.yaml"
render "winget/$WINGET_ID.locale.en-US.yaml" "$WINGET_DIR/$WINGET_ID.locale.en-US.yaml"
