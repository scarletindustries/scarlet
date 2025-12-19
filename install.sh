#!/bin/bash
set -e

# AL installer script
# Usage: curl -fsSL https://raw.githubusercontent.com/alii/al/master/install.sh | bash

REPO="alii/al"
INSTALL_DIR="/usr/local/bin"
BINARY_NAME="al"

echo "Installing AL..."

# Detect architecture
ARCH=$(uname -m)
case $ARCH in
    x86_64) ARCH="x86_64" ;;
    arm64|aarch64) ARCH="arm64" ;;
    *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

# Detect OS
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
case $OS in
    darwin) OS="macos" ;;
    linux) OS="linux" ;;
    *) echo "Unsupported OS: $OS"; exit 1 ;;
esac

# For now we only have macOS builds
if [ "$OS" != "macos" ]; then
    echo "Only macOS is supported currently"
    exit 1
fi

# Download URL
DOWNLOAD_URL="https://github.com/$REPO/releases/download/canary/al-$OS-$ARCH"

echo "Downloading from $DOWNLOAD_URL..."

# Create temp file
TMP_FILE=$(mktemp)
trap "rm -f $TMP_FILE" EXIT

# Download
if command -v curl &> /dev/null; then
    curl -fsSL "$DOWNLOAD_URL" -o "$TMP_FILE"
elif command -v wget &> /dev/null; then
    wget -q "$DOWNLOAD_URL" -O "$TMP_FILE"
else
    echo "curl or wget required"
    exit 1
fi

# Install
chmod +x "$TMP_FILE"

if [ -w "$INSTALL_DIR" ]; then
    mv "$TMP_FILE" "$INSTALL_DIR/$BINARY_NAME"
else
    echo "Need sudo to install to $INSTALL_DIR"
    sudo mv "$TMP_FILE" "$INSTALL_DIR/$BINARY_NAME"
fi

echo "Installed AL to $INSTALL_DIR/$BINARY_NAME"
echo ""
echo "Run 'al --help' to get started"
