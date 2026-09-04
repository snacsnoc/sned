#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
source "${SCRIPT_DIR}/release-package-common.sh"

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

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    usage
    exit 0
fi

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
            printf 'Unknown option: %s\n' "$1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

VERSION="$(release_version "${PROJECT_ROOT}")"
if [[ -z "${VERSION}" ]]; then
    printf '%s\n' "unable to read package version from Cargo.toml" >&2
    exit 1
fi

HOST_OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
HOST_TARGET="$(rustc -vV | awk '/^host:/ { print $2 }')"
TARGET_DIR="$(release_target_dir "${PROJECT_ROOT}")"
ARTIFACT_DIR="$(release_artifact_dir "${TARGET_DIR}")"

case "${TARGET_TRIPLE}" in
    *-unknown-linux-gnu|*-unknown-linux-gnueabihf)
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

PROFILE_DIR="${BUILD_MODE}"
if [[ "${BUILD_MODE}" == "release" ]]; then
    PROFILE_DIR="dist"
fi
TARGET_BINARY="${TARGET_DIR}/${TARGET_TRIPLE}/${PROFILE_DIR}/sned"
STAGING_DIR=""
PACKAGE_DIR=""
TARBALL="${ARTIFACT_DIR}/sned-${VERSION}-${ARTIFACT_SUFFIX}.tar.gz"

mkdir -p "${TARGET_DIR}" "${ARTIFACT_DIR}"
STAGING_DIR="$(mktemp -d "${TARGET_DIR}/.sned-release-staging.XXXXXX")"
PACKAGE_DIR="${STAGING_DIR}/sned-${VERSION}-${ARTIFACT_SUFFIX}"
cleanup() {
    rm -rf "${STAGING_DIR}"
}
trap cleanup EXIT

if [[ "${HOST_TARGET}" == "${TARGET_TRIPLE}" ]]; then
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
printf '%s\n' "target directory: ${TARGET_DIR}"

if ! command -v rustup >/dev/null 2>&1; then
    printf '%s\n' "rustup is required to install target ${TARGET_TRIPLE}" >&2
    exit 1
fi

if ! rustup target add "${TARGET_TRIPLE}"; then
    printf '%s\n' "unable to install Rust target ${TARGET_TRIPLE}" >&2
    exit 1
fi

BUILD_ARGS=(
    --target "${TARGET_TRIPLE}"
    --target-dir "${TARGET_DIR}"
    --locked
    --manifest-path "${PROJECT_ROOT}/Cargo.toml"
    --bin sned
)
if [[ "${BUILD_MODE}" == "release" ]]; then
    BUILD_ARGS+=(--profile dist)
fi

if [[ "${BUILD_CMD[1]}" == "zigbuild" ]]; then
    CARGO_INCREMENTAL=0 cargo zigbuild "${BUILD_ARGS[@]}"
else
    CARGO_INCREMENTAL=0 cargo build "${BUILD_ARGS[@]}"
fi

if [[ ! -f "${TARGET_BINARY}" ]]; then
    printf '%s\n' "expected binary not found at ${TARGET_BINARY}" >&2
    exit 1
fi

mkdir -p "${PACKAGE_DIR}"
cp "${TARGET_BINARY}" "${PACKAGE_DIR}/sned"
chmod +x "${PACKAGE_DIR}/sned"

repack_release "${PACKAGE_DIR}" "${TARBALL}"
file "${PACKAGE_DIR}/sned" || true
