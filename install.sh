#!/bin/bash
set -e

# AL installer script
# Usage: curl -fsSL al.alistair.sh/install.sh | bash

INSTALL_DIR="$HOME/.al/bin"
BINARY_NAME="al"
REPO="alii/al"

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

ASSET_NAME="al-$OS-$ARCH"

# Create install directory
mkdir -p "$INSTALL_DIR"

if ! command -v gh &> /dev/null; then
    echo "GitHub CLI (gh) is required: https://cli.github.com"
    exit 1
fi

if ! gh auth status &> /dev/null; then
    echo "Please authenticate with GitHub first: gh auth login"
    exit 1
fi

gh release download canary --repo "$REPO" --pattern "$ASSET_NAME" --dir "$INSTALL_DIR" --clobber

# Install
mv "$INSTALL_DIR/$ASSET_NAME" "$INSTALL_DIR/$BINARY_NAME"
chmod +x "$INSTALL_DIR/$BINARY_NAME"

echo "Installed AL to $INSTALL_DIR/$BINARY_NAME"

# Add to PATH if not already there
add_to_path() {
    local rc_file="$1"
    local shell_name="$2"

    if [ -f "$rc_file" ]; then
        if ! grep -q 'export PATH="$HOME/.al/bin:$PATH"' "$rc_file" 2>/dev/null; then
            echo "" >> "$rc_file"
            echo '# AL' >> "$rc_file"
            echo 'export PATH="$HOME/.al/bin:$PATH"' >> "$rc_file"
            echo "Added ~/.al/bin to PATH in $rc_file"
            return 0
        else
            return 1  # Already in PATH
        fi
    fi
    return 2  # File doesn't exist
}

PATH_ADDED=false
SHELL_NAME=$(basename "$SHELL")

case $SHELL_NAME in
    zsh)
        if add_to_path "$HOME/.zshrc" "zsh"; then
            PATH_ADDED=true
        fi
        ;;
    bash)
        # Try .bashrc first, then .bash_profile
        if add_to_path "$HOME/.bashrc" "bash"; then
            PATH_ADDED=true
        elif add_to_path "$HOME/.bash_profile" "bash"; then
            PATH_ADDED=true
        fi
        ;;
    fish)
        FISH_CONFIG="$HOME/.config/fish/config.fish"
        if [ -f "$FISH_CONFIG" ]; then
            if ! grep -q 'set -gx PATH $HOME/.al/bin $PATH' "$FISH_CONFIG" 2>/dev/null; then
                echo "" >> "$FISH_CONFIG"
                echo "# AL" >> "$FISH_CONFIG"
                echo 'set -gx PATH $HOME/.al/bin $PATH' >> "$FISH_CONFIG"
                echo "Added ~/.al/bin to PATH in $FISH_CONFIG"
                PATH_ADDED=true
            fi
        fi
        ;;
esac

echo ""
if [ "$PATH_ADDED" = true ]; then
    echo "Restart your shell or run:"
    echo "  source ~/.$SHELL_NAME*rc"
    echo ""
    echo "Then run 'al' to get started"
elif echo "$PATH" | grep -q "$HOME/.al/bin"; then
    echo "Run 'al' to get started"
else
    echo "Add ~/.al/bin to your PATH:"
    echo "  export PATH=\"\$HOME/.al/bin:\$PATH\""
    echo ""
    echo "Then run 'al' to get started"
fi
