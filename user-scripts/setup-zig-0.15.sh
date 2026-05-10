#!/bin/bash
# Helper script to set up Zig 0.15.x for building libghostty-rs
# 
# Usage: source user-scripts/setup-zig-0.15.sh
# Or: export PATH="/opt/homebrew/opt/zig@0.15/bin:$PATH"

set -e

# Check if zig is available
if command -v zig &> /dev/null; then
    ZIG_VERSION=$(zig version)
    echo "Found zig version: $ZIG_VERSION"
    if [[ "$ZIG_VERSION" == 0.15.* ]]; then
        echo "✓ Zig 0.15.x is already in PATH"
    else
        echo "⚠ Warning: zig version $ZIG_VERSION is not 0.15.x"
        echo "  libghostty-rs requires Zig 0.15.x"
    fi
else
    echo "Zig not found in PATH"
fi

# Check for homebrew installation
if [ -d "/opt/homebrew/opt/zig@0.15" ]; then
    echo "✓ Zig 0.15 found at /opt/homebrew/opt/zig@0.15"
    echo "To add to PATH, run:"
    echo '  export PATH="/opt/homebrew/opt/zig@0.15/bin:$PATH"'
    echo ""
    echo "Or add to your shell profile (~/.zshrc or ~/.bashrc):"
    echo '  export PATH="/opt/homebrew/opt/zig@0.15/bin:$PATH"'
else
    echo "Zig 0.15 not found at /opt/homebrew/opt/zig@0.15"
    echo "To install, run:"
    echo "  brew install zig@0.15"
fi
