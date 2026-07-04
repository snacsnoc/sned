#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
TARGET_TRIPLE="${1:-}"
ARTIFACT_SUFFIX="${2:-}"
BUILD_MODE="release"

usage() {
    cat <<'EOF'
Usage: ./scripts/build-release-package.sh <target-triple> <artifact-suffix> [--debug|--release]

Builds sned for the requested target triple and packages the binary into a
tar.gz file suitable for GitHub release uploads.
EOF
}

if [[ -z "${TARGET_TRIPLE}" || -z "${ARTIFACT_SUFFIX}" ]]; then
    usage
    exit 1
fi

shift 2
while [[ $# -gt 0 ]]; do
    case "$1" in
        --debug)
            BUILD_MODE="debug"
            shift
            ;;
        --release)
            BUILD_MODE="release"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

VERSION="$(awk -F'"' '/^version = / { print $2; exit }' "${PROJECT_ROOT}/Cargo.toml")"
if [[ -z "${VERSION}" ]]; then
    printf '%s\n' "unable to read package version from Cargo.toml" >&2
    exit 1
fi

HOST_OS="$(uname -s | tr '[:upper:]' '[:lower:]')"

case "${TARGET_TRIPLE}" in
    *-unknown-linux-gnu)
        TARGET_OS="linux"
        ;;
    *-unknown-freebsd)
        TARGET_OS="freebsd"
        ;;
    *-apple-darwin)
        TARGET_OS="darwin"
        ;;
    *)
        printf '%s\n' "unsupported target triple: ${TARGET_TRIPLE}" >&2
        exit 1
        ;;
esac

case "${TARGET_OS}" in
    linux|freebsd|darwin)
        ;;
    *)
        printf '%s\n' "unsupported target OS in triple: ${TARGET_TRIPLE}" >&2
        exit 1
        ;;
esac

BUILD_FLAG=""
if [[ "${BUILD_MODE}" == "release" ]]; then
    BUILD_FLAG="--release"
fi
PROFILE_DIR="${BUILD_MODE}"
TARGET_BINARY="${PROJECT_ROOT}/target/${TARGET_TRIPLE}/${PROFILE_DIR}/sned"
ARTIFACT_DIR="${PROJECT_ROOT}/target/dist/${ARTIFACT_SUFFIX}"
TARBALL="${ARTIFACT_DIR}/sned-${VERSION}-${ARTIFACT_SUFFIX}.tar.gz"
STAGING_DIR="$(mktemp -d "${TMPDIR:-/tmp}/sned-package.XXXXXX")"

cleanup() {
    rm -rf "${STAGING_DIR}"
}
trap cleanup EXIT

mkdir -p "${ARTIFACT_DIR}"

if [[ "${HOST_OS}" == "${TARGET_OS}" ]]; then
    BUILD_CMD=(cargo build)
elif command -v cargo-zigbuild >/dev/null 2>&1; then
    BUILD_CMD=(cargo zigbuild)
else
    printf '%s\n' "cross-building ${TARGET_TRIPLE} from ${HOST_OS} requires cargo-zigbuild" >&2
    printf '%s\n' "install cargo-zigbuild or run this script on a native ${TARGET_OS} host" >&2
    exit 1
fi

printf '%s\n' "version: ${VERSION}"
printf '%s\n' "target: ${TARGET_TRIPLE}"
printf '%s\n' "mode: ${BUILD_MODE}"
printf '%s\n' "builder: ${BUILD_CMD[*]}"

rustup target add "${TARGET_TRIPLE}" >/dev/null 2>&1 || true

if [[ "${BUILD_CMD[1]}" == "zigbuild" ]]; then
    CARGO_INCREMENTAL=0 cargo zigbuild --target "${TARGET_TRIPLE}" ${BUILD_FLAG} --manifest-path "${PROJECT_ROOT}/Cargo.toml"
else
    CARGO_INCREMENTAL=0 cargo build --target "${TARGET_TRIPLE}" ${BUILD_FLAG} --manifest-path "${PROJECT_ROOT}/Cargo.toml"
fi

if [[ ! -f "${TARGET_BINARY}" ]]; then
    printf '%s\n' "expected binary not found at ${TARGET_BINARY}" >&2
    exit 1
fi

mkdir -p "${STAGING_DIR}"
cp "${TARGET_BINARY}" "${STAGING_DIR}/sned"
chmod +x "${STAGING_DIR}/sned"

tar -C "${STAGING_DIR}" -czf "${TARBALL}" sned

printf '%s\n' "packaged ${TARBALL}"
file "${STAGING_DIR}/sned" || true
