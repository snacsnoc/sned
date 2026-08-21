#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
source "${SCRIPT_DIR}/release-package-common.sh"

BUILD_MODE="release"
REQUESTED_TARGETS=()
ALL_TARGETS=(
    macos-arm64
    macos-x86_64
    linux-amd64
    linux-armv7l
    freebsd-amd64
)

usage() {
    cat <<'EOF'
Usage: ./scripts/build-all-releases.sh [--release|--debug] [--target <suffix>]...

Builds the configured release targets and writes their archives to
${CARGO_TARGET_DIR:-target}/release-artifacts/.

When no --target options are supplied, all supported targets are built.
SHA256SUMS is written only after every selected target succeeds.
EOF
}

target_is_supported() {
    local requested="$1"
    local target
    for target in "${ALL_TARGETS[@]}"; do
        if [[ "${target}" == "${requested}" ]]; then
            return 0
        fi
    done
    return 1
}

target_was_requested() {
    local requested="$1"
    local target
    if [[ "${#REQUESTED_TARGETS[@]}" -eq 0 ]]; then
        return 1
    fi
    for target in "${REQUESTED_TARGETS[@]}"; do
        if [[ "${target}" == "${requested}" ]]; then
            return 0
        fi
    done
    return 1
}

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
        --target)
            if [[ $# -lt 2 ]]; then
                printf '%s\n' '--target requires an architecture suffix' >&2
                usage >&2
                exit 1
            fi
            if ! target_is_supported "$2"; then
                printf 'unsupported release target: %s\n' "$2" >&2
                usage >&2
                exit 1
            fi
            if target_was_requested "$2"; then
                printf 'duplicate release target: %s\n' "$2" >&2
                exit 1
            fi
            REQUESTED_TARGETS+=("$2")
            shift 2
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

TARGETS=()
if [[ "${#REQUESTED_TARGETS[@]}" -gt 0 ]]; then
    TARGETS=("${REQUESTED_TARGETS[@]}")
else
    TARGETS=("${ALL_TARGETS[@]}")
fi

VERSION="$(release_version "${PROJECT_ROOT}")"
if [[ -z "${VERSION}" ]]; then
    printf '%s\n' 'unable to read package version from Cargo.toml' >&2
    exit 1
fi

TARGET_DIR="$(release_target_dir "${PROJECT_ROOT}")"
ARTIFACT_DIR="$(release_artifact_dir "${TARGET_DIR}")"
mkdir -p "${ARTIFACT_DIR}"

completed_targets=()
for target in "${TARGETS[@]}"; do
    printf '%s\n' "building release target ${target} (${BUILD_MODE})"
    if ! "${SCRIPT_DIR}/build-${target}.sh" "--${BUILD_MODE}"; then
        printf '%s\n' "release target failed: ${target}" >&2
        if [[ "${#completed_targets[@]}" -gt 0 ]]; then
            printf 'completed targets: %s\n' "${completed_targets[*]}" >&2
        else
            printf '%s\n' 'completed targets: none' >&2
        fi
        exit 1
    fi
    completed_targets+=("${target}")
done

write_release_checksums "${TARGET_DIR}" "${VERSION}" "${TARGETS[@]}"
printf '%s\n' "release artifacts ready in ${ARTIFACT_DIR}"
