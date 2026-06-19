#!/bin/bash
set -e

# Get version from Cargo.toml
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')

# Detect platform and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
    darwin)  PLATFORM="macos" ;;
    linux)   PLATFORM="linux" ;;
    mingw*|msys*|cygwin*) PLATFORM="windows" ;;
    *)       PLATFORM="$OS" ;;
esac

case "$ARCH" in
    x86_64|amd64)  ARCH="x64" ;;
    aarch64|arm64) ARCH="arm64" ;;
    armv7l)        ARCH="armv7" ;;
esac

BINARY="throwback"
if [ "$PLATFORM" = "windows" ]; then
    BINARY="throwback.exe"
fi

ZIP_NAME="throwback-v${VERSION}-${PLATFORM}-${ARCH}.zip"

echo "Building Throwback v${VERSION} for ${PLATFORM}-${ARCH}..."
cargo build --release --bin throwback

# Generate third-party license notices for everything statically linked into
# the binary. Requires cargo-about; install with `cargo install cargo-about`.
echo "Generating THIRD_PARTY_NOTICES.md..."
if ! command -v cargo-about >/dev/null 2>&1; then
    echo "cargo-about not found, installing..."
    cargo install cargo-about --locked
fi
cargo about generate about.hbs -o THIRD_PARTY_NOTICES.md

echo "Creating ${ZIP_NAME}..."

# Create a temp directory for the zip contents
STAGING=$(mktemp -d)
mkdir -p "$STAGING/throwback"

cp "target/release/${BINARY}" "$STAGING/throwback/"
cp README.md "$STAGING/throwback/"
cp LICENSE "$STAGING/throwback/"
cp THIRD_PARTY_NOTICES.md "$STAGING/throwback/"

cd "$STAGING"
zip -r "${ZIP_NAME}" throwback/
cd -

mkdir -p releases
mv "$STAGING/${ZIP_NAME}" releases/
rm -rf "$STAGING"

echo "Done: releases/${ZIP_NAME}"
