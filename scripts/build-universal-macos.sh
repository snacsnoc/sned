#!/bin/bash
# Build a universal macOS binary for sned
# Produces target/universal/sned (fat binary with x86_64 + arm64)
#
# Usage:
#   ./scripts/build-universal-macos.sh [--release]
#
# Prerequisites:
#   - macOS with Xcode Command Line Tools
#   - Rust toolchain with aarch64-apple-darwin and x86_64-apple-darwin targets

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
RELEASE_FLAG=""
BUILD_MODE="debug"
TARGET_DIR="${PROJECT_ROOT}/target"

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --release)
            RELEASE_FLAG="--release"
            BUILD_MODE="release"
            shift
            ;;
        *)
            echo "Unknown option: $1"
            echo "Usage: $0 [--release]"
            exit 1
            ;;
    esac
done

# Detect build profile directory
if [[ -n "${RELEASE_FLAG}" ]]; then
    PROFILE_DIR="release"
else
    PROFILE_DIR="debug"
fi

# Detect if we can build both architectures
HOST_ARCH="$(uname -m)"
echo "🔧 Host architecture: ${HOST_ARCH}"

# Ensure both targets are installed
echo "📦 Checking Rust targets..."
rustup target add aarch64-apple-darwin 2>/dev/null || true
rustup target add x86_64-apple-darwin 2>/dev/null || true

# Build for ARM64
echo "🏗️  Building for aarch64-apple-darwin..."
CARGO_INCREMENTAL=0 cargo build --target aarch64-apple-darwin ${RELEASE_FLAG} --manifest-path "${PROJECT_ROOT}/Cargo.toml"

# Build for x86_64
echo "🏗️  Building for x86_64-apple-darwin..."
CARGO_INCREMENTAL=0 cargo build --target x86_64-apple-darwin ${RELEASE_FLAG} --manifest-path "${PROJECT_ROOT}/Cargo.toml"

# Create universal binary output directory
UNIVERSAL_DIR="${TARGET_DIR}/universal"
mkdir -p "${UNIVERSAL_DIR}"

# Use lipo to create universal binary
echo "🔗 Creating universal binary with lipo..."
lipo -create \
    "${TARGET_DIR}/aarch64-apple-darwin/${PROFILE_DIR}/sned" \
    "${TARGET_DIR}/x86_64-apple-darwin/${PROFILE_DIR}/sned" \
    -output "${UNIVERSAL_DIR}/sned"

# Verify the universal binary
echo "✅ Verifying universal binary..."
lipo -info "${UNIVERSAL_DIR}/sned"
file "${UNIVERSAL_DIR}/sned"

# Check binary size
echo "📊 Binary size:"
ls -lh "${UNIVERSAL_DIR}/sned"

# Smoke test - verify it runs and shows help
echo "🧪 Smoke test (help output):"
"${UNIVERSAL_DIR}/sned" --help || true

echo ""
echo "🎉 Universal binary built successfully!"
echo "   Location: ${UNIVERSAL_DIR}/sned"
