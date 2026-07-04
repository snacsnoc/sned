#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
TARGET_ARCH="${1:-arm64}"
case "${TARGET_ARCH}" in
    arm64)
        BINARY_SUFFIX="macos-arm64"
        ;;
    x86_64)
        BINARY_SUFFIX="macos-x86_64"
        ;;
    *)
        echo "Usage: ./scripts/sign-macos.sh [arm64|x86_64] [identity]"
        exit 1
        ;;
esac

VERSION=$(grep "^version" "${PROJECT_ROOT}/Cargo.toml" | head -1 | cut -d'"' -f2)
PACKAGE_DIR="${PROJECT_ROOT}/target/dist/${BINARY_SUFFIX}/sned-${VERSION}-${BINARY_SUFFIX}"
BINARY_PATH="${PACKAGE_DIR}/sned"
ENTITLEMENTS_FILE="${SCRIPT_DIR}/sned.entitlements"
IDENTITY="${2:-Developer ID Application: Sned}"

echo "Signing sned with Hardened Runtime..."
echo "Identity: ${IDENTITY}"
echo "Binary: ${BINARY_PATH}"

if [[ ! -f "${BINARY_PATH}" ]]; then
    echo "Error: binary not found at ${BINARY_PATH}"
    echo "Run ./scripts/build-${BINARY_SUFFIX}.sh first"
    exit 1
fi

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

echo "Signing with codesign..."
codesign \
    --sign "${IDENTITY}" \
    --force \
    --options runtime \
    --entitlements "${ENTITLEMENTS_FILE}" \
    --timestamp \
    "${BINARY_PATH}"

echo "Verifying signature..."
codesign --verify --verbose "${BINARY_PATH}"
codesign --display --verbose=4 "${BINARY_PATH}"

rm "${ENTITLEMENTS_FILE}"

echo ""
echo "Binary signed successfully:"
echo "Location: ${BINARY_PATH}"
