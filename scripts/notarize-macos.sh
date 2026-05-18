#!/bin/bash
# Notarize the sned binary and staple the ticket.
#
# Usage:
#   ./scripts/notarize-macos.sh [bundle-id]
#
# Arguments:
#   bundle-id - Bundle identifier for notarization (default: run.sned.cli)
#
# Prerequisites:
#   - macOS with Xcode Command Line Tools
#   - Apple Developer account with App Store Connect API key
#   - Binary already signed (run sign-macos.sh first)
#   - Environment variables set:
#     - APPLE_ID: Your Apple ID email
#     - APPLE_APP_SPECIFIC_PASSWORD: App-specific password for notarization
#     - APPLE_TEAM_ID: Apple Developer Team ID

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
UNIVERSAL_BINARY="${PROJECT_ROOT}/target/universal/sned"
ZIP_PATH="${PROJECT_ROOT}/target/universal/sned.zip"

BUNDLE_ID="${1:-run.sned.cli}"

echo "📦 Notarizing sned..."
echo "   Bundle ID: ${BUNDLE_ID}"
echo "   Binary: ${UNIVERSAL_BINARY}"

# Check prerequisites
if [[ ! -f "${UNIVERSAL_BINARY}" ]]; then
    echo "❌ Error: Universal binary not found at ${UNIVERSAL_BINARY}"
    exit 1
fi

if [[ -z "${APPLE_ID:-}" ]]; then
    echo "❌ Error: APPLE_ID environment variable not set"
    echo "   Set it to your Apple ID email address"
    exit 1
fi

if [[ -z "${APPLE_APP_SPECIFIC_PASSWORD:-}" ]]; then
    echo "❌ Error: APPLE_APP_SPECIFIC_PASSWORD environment variable not set"
    echo "   Generate an app-specific password at appleid.apple.com"
    exit 1
fi

if [[ -z "${APPLE_TEAM_ID:-}" ]]; then
    echo "❌ Error: APPLE_TEAM_ID environment variable not set"
    echo "   Find it in Apple Developer Portal"
    exit 1
fi

# Create a ZIP for notarization
echo "🗜️  Creating ZIP for notarization..."
ditto -c -k --keepParent "${UNIVERSAL_BINARY}" "${ZIP_PATH}"

# Submit for notarization
echo "📤 Submitting for notarization..."
xcrun notarytool submit "${ZIP_PATH}" \
    --apple-id "${APPLE_ID}" \
    --password "${APPLE_APP_SPECIFIC_PASSWORD}" \
    --team-id "${APPLE_TEAM_ID}" \
    --wait \
    --output-format json \
    | tee "${PROJECT_ROOT}/target/universal/notarization-log.json"

# Extract submission ID
SUBMISSION_ID=$(cat "${PROJECT_ROOT}/target/universal/notarization-log.json" | grep -o '"id": "[^"]*"' | head -1 | cut -d'"' -f4)

echo ""
echo "📋 Notarization submitted with ID: ${SUBMISSION_ID}"

# Check status
STATUS=$(cat "${PROJECT_ROOT}/target/universal/notarization-log.json" | grep -o '"status": "[^"]*"' | head -1 | cut -d'"' -f4)

if [[ "${STATUS}" == "Accepted" ]]; then
    echo "✅ Notarization accepted!"
    
    # Staple the ticket
    echo "📎 Stapling notarization ticket..."
    xcrun stapler staple "${UNIVERSAL_BINARY}"
    
    # Verify staple
    echo "🔍 Verifying staple..."
    xcrun stapler validate "${UNIVERSAL_BINARY}"
    
    echo ""
    echo "🎉 Binary notarized and stapled successfully!"
    echo "   Location: ${UNIVERSAL_BINARY}"
    
    # Clean up
    rm "${ZIP_PATH}"
    
    echo ""
    echo "Next steps:"
    echo "   1. Create Homebrew formula: ./scripts/homebrew-formula.sh"
    echo "   2. Test installation on a clean macOS machine"
else
    echo "❌ Notarization failed with status: ${STATUS}"
    echo "   Check log: ${PROJECT_ROOT}/target/universal/notarization-log.json"
    exit 1
fi
