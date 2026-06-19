#!/bin/sh
# throwback installer
#
# Install or update throwback with:
#
#   curl -fsSL https://raw.githubusercontent.com/nathankellenicki/throwback/main/scripts/install.sh | sh
#
# Installs to ~/.throwback with a symlink on PATH.

set -e

REPO="nathankellenicki/throwback"
INSTALL_DIR="$HOME/.throwback"

# ── Detect platform ──────────────────────────────

OS=$(uname -s)
ARCH=$(uname -m)

case "$OS-$ARCH" in
    Darwin-arm64)     PLATFORM="macos-arm64" ;;
    Linux-aarch64)    PLATFORM="linux-arm64" ;;
    MINGW*|MSYS*)     echo "On Windows, download the release zip from https://github.com/$REPO/releases"; exit 1 ;;
    *)                echo "Unsupported platform: $OS $ARCH"; exit 1 ;;
esac

# ── Fetch latest release tag ─────────────────────

echo "Fetching latest release..."
VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name"' | cut -d'"' -f4)

if [ -z "$VERSION" ]; then
    echo "Failed to determine latest release version."
    exit 1
fi

echo "Installing throwback $VERSION for $PLATFORM..."

# ── Download and extract ─────────────────────────

URL="https://github.com/$REPO/releases/download/$VERSION/throwback-$VERSION-$PLATFORM.zip"
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

curl -fsSL "$URL" -o "$TMPDIR/throwback.zip"
unzip -q "$TMPDIR/throwback.zip" -d "$TMPDIR"

# ── Install to ~/.throwback ──────────────────────

rm -rf "$INSTALL_DIR"
mv "$TMPDIR/throwback" "$INSTALL_DIR"

chmod +x "$INSTALL_DIR/throwback"

# ── macOS: strip quarantine flag ─────────────────

if [ "$OS" = "Darwin" ]; then
    xattr -d com.apple.quarantine "$INSTALL_DIR/throwback" 2>/dev/null || true
fi

# ── Symlink onto PATH ────────────────────────────

LINK_DIR="/usr/local/bin"
if [ ! -w "$LINK_DIR" ]; then
    LINK_DIR="$HOME/.local/bin"
    mkdir -p "$LINK_DIR"
fi

ln -sf "$INSTALL_DIR/throwback" "$LINK_DIR/throwback"

# ── Done ─────────────────────────────────────────

echo ""
echo "throwback $VERSION installed successfully."
echo ""

# Check if the symlink dir is on PATH
case ":$PATH:" in
    *":$LINK_DIR:"*)
        echo "Run 'throwback' to get started."
        ;;
    *)
        echo "Note: $LINK_DIR is not on your PATH."
        echo "Add it with:  export PATH=\"$LINK_DIR:\$PATH\""
        echo "Then run 'throwback' to get started."
        ;;
esac
echo ""
