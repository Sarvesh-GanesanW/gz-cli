#!/bin/sh
# gz installer: ./install.sh [version]   (version like v0.1.0, default: latest)
# Env: GZ_INSTALL_DIR (default ~/.local/bin), GITHUB_TOKEN (required while
# the repo is private, unless `gh` is authenticated).
set -e

REPO="Sarvesh-GanesanW/gz-cli"
VERSION="${1:-${GZ_VERSION:-latest}}"
INSTALL_DIR="${GZ_INSTALL_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os-$arch" in
  Linux-x86_64) ASSET="gz-linux-x86_64" ;;
  Darwin-arm64) ASSET="gz-macos-arm64" ;;
  Darwin-x86_64) ASSET="gz-macos-x86_64" ;;
  *) echo "gz: unsupported platform $os/$arch (need Linux x64 or macOS)" >&2; exit 1 ;;
esac

if [ "$VERSION" = "latest" ]; then
  BASE="https://github.com/$REPO/releases/latest/download"
  API_REL="https://api.github.com/repos/$REPO/releases/latest"
else
  BASE="https://github.com/$REPO/releases/download/$VERSION"
  API_REL="https://api.github.com/repos/$REPO/releases/tags/$VERSION"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

have_gh() {
  command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1
}

# Private repos reject tokens on github.com download URLs; use the API.
api_asset_url() {
  name="$1"
  curl --proto '=https' --tlsv1.2 -fsSL \
    -H "Authorization: Bearer $GITHUB_TOKEN" -H "Accept: application/vnd.github+json" \
    "$API_REL" \
  | tr ',' '\n' | grep -B8 "\"name\": *\"$name\"" | grep -o '"url": *"[^"]*"' | tail -1 | cut -d'"' -f4
}

download() {
  name="$1"; out="$2"
  if have_gh; then
    if [ "$VERSION" = "latest" ]; then
      gh release download -R "$REPO" -p "$name" -O "$out" --clobber
    else
      gh release download "$VERSION" -R "$REPO" -p "$name" -O "$out" --clobber
    fi
  elif [ -n "$GITHUB_TOKEN" ]; then
    asset_url="$(api_asset_url "$name")"
    [ -n "$asset_url" ] || return 1
    curl --proto '=https' --tlsv1.2 -fsSL \
      -H "Authorization: Bearer $GITHUB_TOKEN" -H "Accept: application/octet-stream" \
      -o "$out" "$asset_url"
  else
    curl --proto '=https' --tlsv1.2 -fsSL -o "$out" "$BASE/$name"
  fi
}

echo "gz: installing $ASSET ($VERSION) to $INSTALL_DIR" >&2
if ! download "$ASSET" "$tmp/gz"; then
  echo "gz: download failed. The repo is private: export GITHUB_TOKEN=<token> or run 'gh auth login' first." >&2
  exit 1
fi
if download "$ASSET.sha256" "$tmp/gz.sha256" 2>/dev/null && command -v sha256sum >/dev/null 2>&1; then
  (cd "$tmp" && sha256sum -c gz.sha256) || { echo "gz: checksum mismatch, aborting" >&2; exit 1; }
  echo "gz: checksum ok" >&2
fi

mkdir -p "$INSTALL_DIR"
mv "$tmp/gz" "$INSTALL_DIR/gz"
chmod +x "$INSTALL_DIR/gz"
trap - EXIT INT TERM

if ! command -v gz >/dev/null 2>&1; then
  echo "gz: installed to $INSTALL_DIR/gz. Add to PATH: export PATH=\"\$PATH:$INSTALL_DIR\"" >&2
else
  echo "gz: installed to $INSTALL_DIR/gz" >&2
fi
"$INSTALL_DIR/gz" --version
