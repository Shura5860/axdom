#!/usr/bin/env sh
set -eu

REPO="Shura5860/axdom"
INSTALL_DIR="${AXDOM_INSTALL_DIR:-$HOME/.local/bin}"

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64|amd64) ASSET="axdom-linux-x86_64" ;;
      *) echo "Unsupported Linux architecture: $ARCH (build from source with: cargo build --release -p axdom-cli)" >&2; exit 1 ;;
    esac
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

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1"
  else
    echo "curl or wget is required to download axdom" >&2
    exit 1
  fi
}

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

BASE_URL="https://github.com/${REPO}/releases/latest/download"
BIN_TMP="${TMP_DIR}/${ASSET}"
SUMS_TMP="${TMP_DIR}/SHA256SUMS"

echo "Downloading ${BASE_URL}/${ASSET}..."
download "${BASE_URL}/${ASSET}" "$BIN_TMP"

if [ "${AXDOM_SKIP_VERIFY:-0}" != "1" ]; then
  echo "Downloading SHA256SUMS..."
  if ! download "${BASE_URL}/SHA256SUMS" "$SUMS_TMP"; then
    echo "Error: failed to download SHA256SUMS (refusing unverified install; set AXDOM_SKIP_VERIFY=1 to override)." >&2
    exit 1
  fi
  echo "Verifying SHA-256 checksum..."
  EXPECTED="$(grep " ${ASSET}\$" "$SUMS_TMP" | awk '{print $1}')"
  if [ -z "$EXPECTED" ]; then
    echo "Error: checksum entry for ${ASSET} not found in SHA256SUMS" >&2
    exit 1
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL="$(sha256sum "$BIN_TMP" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    ACTUAL="$(shasum -a 256 "$BIN_TMP" | awk '{print $1}')"
  else
    echo "Error: sha256sum or shasum is required to verify download integrity" >&2
    exit 1
  fi
  if [ "$EXPECTED" != "$ACTUAL" ]; then
    echo "Error: SHA-256 checksum mismatch for ${ASSET}!" >&2
    exit 1
  fi
  echo "Checksum OK. (For provenance verification, run: gh attestation verify ${ASSET} --repo ${REPO})"
fi

mkdir -p "$INSTALL_DIR"
DEST="${INSTALL_DIR}/axdom"
chmod +x "$BIN_TMP"
mv "$BIN_TMP" "$DEST"

echo "Installed axdom to ${DEST}"
if [ "${AXDOM_AUTO_INIT:-0}" = "1" ]; then
  "$DEST" init
else
  echo "Next: preview MCP & OS setup with:"
  echo "  ${DEST} init --dry-run"
  echo "Then apply with:"
  echo "  ${DEST} init"
fi
