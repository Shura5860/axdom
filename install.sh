#!/usr/bin/env sh
set -eu

REPO="Shura5860/axdom"
INSTALL_DIR="${AXDOM_INSTALL_DIR:-$HOME/.local/bin}"

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)
    ASSET="axdom-linux-x86_64"
    ;;
  Darwin)
    case "$ARCH" in
      arm64|aarch64) ASSET="axdom-macos-arm64" ;;
      x86_64)        ASSET="axdom-macos-x86_64" ;;
      *) echo "Unsupported macOS architecture: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  *)
    echo "Unsupported OS: $OS (download axdom-windows-x86_64.exe from GitHub Releases)" >&2
    exit 1
    ;;
esac

mkdir -p "$INSTALL_DIR"
URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"
DEST="${INSTALL_DIR}/axdom"

echo "Downloading ${URL}..."
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$URL" -o "$DEST"
elif command -v wget >/dev/null 2>&1; then
  wget -qO "$DEST" "$URL"
else
  echo "curl or wget is required to download axdom" >&2
  exit 1
fi

chmod +x "$DEST"
echo "Installed axdom to ${DEST}"
echo "Running one-time setup (axdom init)..."
"$DEST" init
