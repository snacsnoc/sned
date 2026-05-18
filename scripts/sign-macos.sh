#!/bin/bash
# Sign the sned universal binary with Hardened Runtime.
#
# Usage:
#   ./scripts/sign-macos.sh [identity]
#
# Arguments:
#   identity - Apple Developer ID Application identity (default: "Developer ID Application: Sned")
#
# Prerequisites:
#   - macOS with Xcode Command Line Tools
#   - Apple Developer ID certificate installed in Keychain
#   - Binary already built (run build-universal-macos.sh first)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
UNIVERSAL_BINARY="${PROJECT_ROOT}/target/universal/sned"
ENTITLEMENTS_FILE="${SCRIPT_DIR}/sned.entitlements"

# Default signing identity
IDENTITY="${1:-Developer ID Application: Sned}"

echo "🔏 Signing sned with Hardened Runtime..."
echo "   Identity: ${IDENTITY}"
echo "   Binary: ${UNIVERSAL_BINARY}"

# Check if binary exists
if [[ ! -f "${UNIVERSAL_BINARY}" ]]; then
    echo "❌ Error: Universal binary not found at ${UNIVERSAL_BINARY}"
    echo "   Run ./scripts/build-universal-macos.sh first"
    exit 1
fi

# Create entitlements file for Hardened Runtime
cat > "${ENTITLEMENTS_FILE}" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <!-- Allow execution of JIT-compiled code (if needed for tree-sitter) -->
    <key>com.apple.security.cs.allow-jit</key>
    <true/>
    
    <!-- Allow unsigned executable memory (for tree-sitter parsers) -->
    <key>com.apple.security.cs.allow-unsigned-executable-memory</key>
    <true/>
    
    <!-- Disable library validation (for libghostty dynamic linking) -->
    <key>com.apple.security.cs.disable-library-validation</key>
    <true/>
    
    <!-- Allow dyld environment variables (for development) -->
    <key>com.apple.security.cs.allow-dyld-environment-variables</key>
    <true/>
</dict>
</plist>
EOF

# Sign the binary
echo "📜 Signing with codesign..."
codesign \
    --sign "${IDENTITY}" \
    --force \
    --options runtime \
    --entitlements "${ENTITLEMENTS_FILE}" \
    --timestamp \
    "${UNIVERSAL_BINARY}"

# Verify signature
echo "✅ Verifying signature..."
codesign --verify --verbose "${UNIVERSAL_BINARY}"
codesign --display --verbose=4 "${UNIVERSAL_BINARY}"

# Clean up entitlements file
rm "${ENTITLEMENTS_FILE}"

echo ""
echo "🎉 Binary signed successfully!"
echo "   Location: ${UNIVERSAL_BINARY}"
echo ""
echo "Next steps:"
echo "   1. Notarize: ./scripts/notarize-macos.sh"
echo "   2. Create Homebrew formula: ./scripts/homebrew-formula.sh"
