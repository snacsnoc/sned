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
        echo "Usage: ./scripts/notarize-macos.sh [arm64|x86_64] [bundle-id]"
        exit 1
        ;;
esac

VERSION=$(grep "^version" "${PROJECT_ROOT}/Cargo.toml" | head -1 | cut -d'"' -f2)
PACKAGE_DIR="${PROJECT_ROOT}/target/dist/${BINARY_SUFFIX}/sned-${VERSION}-${BINARY_SUFFIX}"
BINARY_PATH="${PACKAGE_DIR}/sned"
ZIP_PATH="${PROJECT_ROOT}/target/dist/${BINARY_SUFFIX}/sned.zip"
LOG_PATH="${PROJECT_ROOT}/target/dist/${BINARY_SUFFIX}/notarization-log.json"
BUNDLE_ID="${2:-run.sned.cli}"

echo "Notarizing sned..."
echo "Bundle ID: ${BUNDLE_ID}"
echo "Binary: ${BINARY_PATH}"

if [[ ! -f "${BINARY_PATH}" ]]; then
    echo "Error: binary not found at ${BINARY_PATH}"
    exit 1
fi

if [[ -z "${APPLE_ID:-}" ]]; then
    echo "Error: APPLE_ID environment variable not set"
    exit 1
fi

if [[ -z "${APPLE_APP_SPECIFIC_PASSWORD:-}" ]]; then
    echo "Error: APPLE_APP_SPECIFIC_PASSWORD environment variable not set"
    exit 1
fi

if [[ -z "${APPLE_TEAM_ID:-}" ]]; then
    echo "Error: APPLE_TEAM_ID environment variable not set"
    exit 1
fi

echo "Creating ZIP for notarization..."
ditto -c -k --keepParent "${BINARY_PATH}" "${ZIP_PATH}"

echo "Submitting for notarization..."
xcrun notarytool submit "${ZIP_PATH}" \
    --apple-id "${APPLE_ID}" \
    --password "${APPLE_APP_SPECIFIC_PASSWORD}" \
    --team-id "${APPLE_TEAM_ID}" \
    --wait \
    --output-format json \
    | tee "${LOG_PATH}"

SUBMISSION_ID=$(grep -o '"id": "[^"]*"' "${LOG_PATH}" | head -1 | cut -d'"' -f4)

echo ""
echo "Notarization submitted with ID: ${SUBMISSION_ID}"

STATUS=$(grep -o '"status": "[^"]*"' "${LOG_PATH}" | head -1 | cut -d'"' -f4)

if [[ "${STATUS}" == "Accepted" ]]; then
    echo "Notarization accepted!"
    echo "Stapling notarization ticket..."
    xcrun stapler staple "${BINARY_PATH}"
    echo "Verifying staple..."
    xcrun stapler validate "${BINARY_PATH}"

    echo ""
    echo "Binary notarized and stapled successfully:"
    echo "Location: ${BINARY_PATH}"
    rm "${ZIP_PATH}"
else
    echo "Notarization failed with status: ${STATUS}"
    echo "Check log: ${LOG_PATH}"
    exit 1
fi
