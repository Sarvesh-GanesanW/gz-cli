#!/bin/sh
# gz installer for macOS and Linux.
#
#   install:         curl -fsSL https://raw.githubusercontent.com/Sarvesh-GanesanW/gz-cli/main/install.sh | sh
#   one version:     ... | sh -s -- v0.8.1
#   remove:          ... | sh -s -- --uninstall
#   remove all:      ... | sh -s -- --uninstall --purge   (also deletes settings and sign-in)
#
# Env: GZ_INSTALL_DIR (default ~/.local/bin), GZ_VERSION (default latest).
set -eu

REPO="Sarvesh-GanesanW/gz-cli"
SCRIPT_URL="https://raw.githubusercontent.com/$REPO/main/install.sh"
INSTALL_DIR="${GZ_INSTALL_DIR:-$HOME/.local/bin}"
# The folder gz keeps its settings in (see config_path in src/config.rs).
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/gz"
# Where gz 0.2.1 and older kept them on macOS.
LEGACY_MAC_CONFIG_DIR="$HOME/Library/Application Support/cloud.Groundzero.gz"
CURL="curl --proto =https --tlsv1.2 -fL"

say() { printf '%s\n' "$*" >&2; }
# A path as people read it: ~ for the home folder.
pretty() {
  case "$1" in
    "$HOME"/*) printf '~/%s' "${1#"$HOME"/}" ;;
    *) printf '%s' "$1" ;;
  esac
}
# A path for a command someone copies: $HOME so it works in any shell.
for_shell() {
  case "$1" in
    "$HOME"/*) printf '$HOME/%s' "${1#"$HOME"/}" ;;
    *) printf '%s' "$1" ;;
  esac
}
step() { printf '\n[%s] %s\n' "$1" "$2" >&2; }
detail() { printf '      %s\n' "$*" >&2; }
fail() { printf '\ngz: %s\n' "$*" >&2; exit 1; }

usage() {
  cat >&2 <<EOF
gz installer

  sh install.sh [version]          install gz (latest release unless a version like v0.8.1 is given)
  sh install.sh --uninstall        remove gz, keep its settings
  sh install.sh --uninstall --purge
                                   remove gz and its settings and sign-in

Installs to \$GZ_INSTALL_DIR (default ~/.local/bin).
EOF
}

VERSION="${GZ_VERSION:-latest}"
ACTION=install
PURGE=0
for arg in "$@"; do
  case "$arg" in
    --uninstall) ACTION=uninstall ;;
    --purge) PURGE=1 ;;
    -h | --help) usage; exit 0 ;;
    v[0-9]*) VERSION="$arg" ;;
    [0-9]*) VERSION="v$arg" ;;
    *) usage; fail "unknown option '$arg'" ;;
  esac
done
if [ "$PURGE" = 1 ] && [ "$ACTION" != uninstall ]; then
  fail "--purge only goes with --uninstall"
fi

# The command that runs this script again with other options, for the hints
# printed at the end. It carries a custom install folder along.
rerun() {
  if [ -n "${GZ_INSTALL_DIR:-}" ]; then
    printf 'curl -fsSL %s | GZ_INSTALL_DIR="%s" sh -s -- %s' "$SCRIPT_URL" "$(for_shell "$INSTALL_DIR")" "$1"
  else
    printf 'curl -fsSL %s | sh -s -- %s' "$SCRIPT_URL" "$1"
  fi
}

installed_version() {
  "$1" --version 2>/dev/null | head -n 1 || true
}

human_size() {
  awk -v b="$1" 'BEGIN { if (b >= 1048576) printf "%.1f MB", b / 1048576; else printf "%.0f KB", b / 1024 }'
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

uninstall() {
  say "Removing gz"
  removed=0
  if [ -e "$INSTALL_DIR/gz" ]; then
    detail "program   $(pretty "$INSTALL_DIR/gz") ($(installed_version "$INSTALL_DIR/gz"))"
    rm -f "$INSTALL_DIR/gz"
    removed=1
  else
    detail "program   not found in $(pretty "$INSTALL_DIR")"
    other="$(command -v gz 2>/dev/null || true)"
    if [ -n "$other" ]; then
      detail "          the gz on your PATH is $other; to remove that one, run again"
      detail "          with GZ_INSTALL_DIR=\"$(dirname "$other")\""
    fi
  fi
  # A copy `gz update` keeps only while it swaps in a new version.
  rm -f "$INSTALL_DIR/gz.old"

  if [ "$PURGE" = 1 ]; then
    for dir in "$CONFIG_DIR" "$LEGACY_MAC_CONFIG_DIR"; do
      case "$dir" in
        */gz | */cloud.Groundzero.gz) ;;
        *) fail "refusing to delete '$dir': it is not a gz settings folder" ;;
      esac
      if [ -d "$dir" ]; then
        detail "settings  $(pretty "$dir") (deleted: sign-in, profiles, gz code history)"
        rm -rf "$dir"
        removed=1
      fi
    done
  elif [ -d "$CONFIG_DIR" ]; then
    detail "settings  $(pretty "$CONFIG_DIR") (kept: sign-in, profiles, gz code history)"
    detail "          delete them too with: $(rerun '--uninstall --purge')"
  fi

  if [ "$removed" = 1 ]; then
    say ""
    say "gz is removed."
  else
    say ""
    say "Nothing to remove."
  fi
  say "If you connected AI tools with 'gz mcp', remove the groundzero server from them as well"
  say "(for example: claude mcp remove groundzero)."
}

platform() {
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) ASSET="gz-linux-x86_64"; PLATFORM="Linux (x86_64)" ;;
    Darwin-arm64) ASSET="gz-macos-arm64"; PLATFORM="macOS (Apple silicon)" ;;
    Darwin-x86_64) ASSET="gz-macos-x86_64"; PLATFORM="macOS (Intel)" ;;
    *) fail "unsupported platform $(uname -s)/$(uname -m): gz runs on Linux x86_64 and macOS" ;;
  esac
}

# The tag `latest` points at, read from the redirect GitHub answers with, so
# the installer can say which version it installs.
latest_tag() {
  url="$($CURL -sS -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")" ||
    fail "cannot reach github.com to find the latest release; check your connection and try again"
  tag="${url##*/}"
  case "$tag" in
    v[0-9]*) printf '%s' "$tag" ;;
    *) fail "cannot tell the latest release from $url" ;;
  esac
}

path_hint() {
  dir="$(for_shell "$INSTALL_DIR")"
  case "$(basename "${SHELL:-sh}")" in
    zsh) printf "echo 'export PATH=\"%s:\$PATH\"' >> ~/.zshrc && exec zsh" "$dir" ;;
    bash)
      if [ "$(uname -s)" = Darwin ]; then rc="~/.bash_profile"; else rc="~/.bashrc"; fi
      printf "echo 'export PATH=\"%s:\$PATH\"' >> %s && exec bash" "$dir" "$rc" ;;
    fish) printf 'fish_add_path %s' "$INSTALL_DIR" ;;
    *) printf "echo 'export PATH=\"%s:\$PATH\"' >> ~/.profile" "$dir" ;;
  esac
}

install() {
  platform
  say "Installing gz for $PLATFORM"

  step 1/4 "Finding the release"
  if [ "$VERSION" = latest ]; then
    VERSION="$(latest_tag)"
    detail "latest is $VERSION"
  else
    detail "$VERSION, as asked"
  fi
  base="https://github.com/$REPO/releases/download/$VERSION"

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT INT TERM

  step 2/4 "Downloading $ASSET"
  detail "from $base/$ASSET"
  # A progress bar when someone is watching, nothing in logs.
  if [ -t 2 ]; then progress="--progress-bar"; else progress="-sS"; fi
  # shellcheck disable=SC2086
  $CURL $progress -o "$tmp/$ASSET" "$base/$ASSET" ||
    fail "download failed: $base/$ASSET (is $VERSION a published release? see https://github.com/$REPO/releases)"
  detail "$(human_size "$(wc -c <"$tmp/$ASSET" | tr -d ' ')") downloaded"

  step 3/4 "Verifying the download"
  if ! $CURL -sS -o "$tmp/$ASSET.sha256" "$base/$ASSET.sha256" 2>/dev/null; then
    detail "skipped: $VERSION has no checksum file"
  else
    expected="$(cut -d ' ' -f 1 <"$tmp/$ASSET.sha256")"
    actual="$(sha256_of "$tmp/$ASSET")"
    if [ -z "$actual" ]; then
      detail "skipped: this machine has neither sha256sum nor shasum"
    elif [ "$expected" != "$actual" ]; then
      fail "checksum mismatch for $ASSET (expected $expected, got $actual); nothing was installed"
    else
      detail "sha256 matches the release ($(printf '%s' "$actual" | cut -c 1-12)…)"
    fi
  fi

  step 4/4 "Installing"
  mkdir -p "$INSTALL_DIR"
  if [ -e "$INSTALL_DIR/gz" ]; then
    detail "replacing $(installed_version "$INSTALL_DIR/gz") at $(pretty "$INSTALL_DIR/gz")"
  fi
  # Moved in under a temporary name first so a running gz is never half-written.
  mv "$tmp/$ASSET" "$INSTALL_DIR/.gz.new"
  chmod 755 "$INSTALL_DIR/.gz.new"
  mv -f "$INSTALL_DIR/.gz.new" "$INSTALL_DIR/gz"
  trap - EXIT INT TERM
  rm -rf "$tmp"
  now="$(installed_version "$INSTALL_DIR/gz")"
  [ -n "$now" ] || fail "$INSTALL_DIR/gz was written but does not run; try again, or download it by hand from https://github.com/$REPO/releases"
  detail "placed at $(pretty "$INSTALL_DIR/gz")"

  say ""
  say "$now is installed."
  say "  program   $(pretty "$INSTALL_DIR/gz")"
  if [ -d "$CONFIG_DIR" ]; then
    say "  settings  $(pretty "$CONFIG_DIR") (already there, kept as they were)"
  else
    say "  settings  $(pretty "$CONFIG_DIR") (made when you first sign in)"
  fi

  case ":$PATH:" in
    *":$INSTALL_DIR:"*)
      first="$(command -v gz 2>/dev/null || true)"
      if [ -n "$first" ] && [ "$first" != "$INSTALL_DIR/gz" ]; then
        say ""
        say "Note: another gz at $first comes first on your PATH, so 'gz' runs that one."
        say "Remove it, or put $(pretty "$INSTALL_DIR") earlier in PATH."
      fi
      ;;
    *)
      say ""
      say "$(pretty "$INSTALL_DIR") is not on your PATH yet. Add it with:"
      say "  $(path_hint)"
      ;;
  esac

  say ""
  say "Next:    gz configure, then gz auth login"
  say "Update:  gz update"
  say "Remove:  $(rerun --uninstall)"
  say "         (add --purge to delete the settings and sign-in too)"
}

case "$ACTION" in
  install) install ;;
  uninstall) uninstall ;;
esac
