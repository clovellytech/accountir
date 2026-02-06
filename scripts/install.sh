#!/usr/bin/env bash
set -euo pipefail

REPO="zakvdm/accountir"
INSTALL_DIR="${HOME}/.local/bin"

# Detect OS
case "$(uname -s)" in
  Linux*)  OS="unknown-linux-gnu" ;;
  Darwin*) OS="apple-darwin" ;;
  *)       echo "Unsupported OS: $(uname -s)"; exit 1 ;;
esac

# Detect architecture
case "$(uname -m)" in
  x86_64)  ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *)       echo "Unsupported architecture: $(uname -m)"; exit 1 ;;
esac

TARGET="${ARCH}-${OS}"
echo "Detected platform: ${TARGET}"

# Get latest release tag
echo "Fetching latest release..."
LATEST=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')

if [ -z "$LATEST" ]; then
  echo "Failed to fetch latest release"
  exit 1
fi

echo "Latest version: ${LATEST}"

# Download
ARCHIVE="accountir-${LATEST}-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${LATEST}/${ARCHIVE}"

echo "Downloading ${URL}..."
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT
curl -fsSL "$URL" -o "${TMPDIR}/${ARCHIVE}"

# Extract
echo "Installing to ${INSTALL_DIR}..."
mkdir -p "$INSTALL_DIR"
tar xzf "${TMPDIR}/${ARCHIVE}" -C "$INSTALL_DIR"
chmod +x "${INSTALL_DIR}/accountir"

echo "Installed accountir ${LATEST} to ${INSTALL_DIR}/accountir"

# Check PATH
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
  echo ""
  echo "NOTE: ${INSTALL_DIR} is not in your PATH."
  echo "Add it with:"
  echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
fi
