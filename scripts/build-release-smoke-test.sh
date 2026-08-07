#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
SMOKE_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/sned-build-smoke.XXXXXX")"
FAKE_BIN_DIR="${SMOKE_ROOT}/bin"
FAKE_LOG="${SMOKE_ROOT}/commands.log"
VERSION="$(awk -F'"' '/^version = / { print $2; exit }' "${PROJECT_ROOT}/Cargo.toml")"

cleanup() {
    rm -rf "${SMOKE_ROOT}"
}
trap cleanup EXIT

mkdir -p "${FAKE_BIN_DIR}"

cat > "${FAKE_BIN_DIR}/rustc" <<'EOF'
#!/bin/bash
if [[ "${1:-}" == "-vV" ]]; then
    printf '%s\n' 'host: aarch64-apple-darwin'
    exit 0
fi
exit 1
EOF

cat > "${FAKE_BIN_DIR}/rustup" <<'EOF'
#!/bin/bash
printf 'rustup %s\n' "$*" >> "${FAKE_LOG}"
if [[ "${FAKE_RUSTUP_FAIL:-0}" == "1" ]]; then
    exit 1
fi
exit 0
EOF

cat > "${FAKE_BIN_DIR}/cargo-zigbuild" <<'EOF'
#!/bin/bash
exit 0
EOF

cat > "${FAKE_BIN_DIR}/cargo" <<'EOF'
#!/bin/bash
printf 'cargo %s\n' "$*" >> "${FAKE_LOG}"

target=''
target_dir=''
profile='debug'
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
    case "${args[i]}" in
        --target)
            i=$((i + 1))
            target="${args[i]}"
            ;;
        --target-dir)
            i=$((i + 1))
            target_dir="${args[i]}"
            ;;
        --release)
            profile='release'
            ;;
    esac
done

if [[ -z "${target}" || -z "${target_dir}" ]]; then
    exit 1
fi
mkdir -p "${target_dir}/${target}/${profile}"
printf '%s\n' fake-sned > "${target_dir}/${target}/${profile}/sned"
EOF

chmod +x "${FAKE_BIN_DIR}"/*

assert_file() {
    if [[ ! -f "$1" ]]; then
        printf 'expected file is missing: %s\n' "$1" >&2
        exit 1
    fi
}

assert_log_contains() {
    if ! grep -Fq -- "$1" "${FAKE_LOG}"; then
        printf 'expected command log entry is missing: %s\n' "$1" >&2
        cat "${FAKE_LOG}" >&2
        exit 1
    fi
}

if ! "${PROJECT_ROOT}/scripts/build-release-package.sh" --help >/dev/null; then
    printf '%s\n' 'build-release-package.sh --help failed' >&2
    exit 1
fi

MAC_TARGET="${SMOKE_ROOT}/mac-target"
PATH="${FAKE_BIN_DIR}:${PATH}" \
    FAKE_LOG="${FAKE_LOG}" \
    CARGO_TARGET_DIR="${MAC_TARGET}" \
    "${PROJECT_ROOT}/scripts/build-macos-arm64.sh" >/dev/null

MAC_ARCHIVE="${MAC_TARGET}/dist/macos-arm64/sned-${VERSION}-macos-arm64.tar.gz"
assert_file "${MAC_ARCHIVE}"
tar -tzf "${MAC_ARCHIVE}" | grep -Fx 'sned' >/dev/null
assert_log_contains '--target aarch64-apple-darwin'
assert_log_contains "--target-dir ${MAC_TARGET}"
assert_log_contains '--locked'

ARM_TARGET="${SMOKE_ROOT}/arm-target"
PATH="${FAKE_BIN_DIR}:${PATH}" \
FAKE_LOG="${FAKE_LOG}" \
CARGO_TARGET_DIR="${ARM_TARGET}" \
    "${PROJECT_ROOT}/scripts/build-linux-armv7l.sh" >/dev/null

ARM_ARCHIVE="${ARM_TARGET}/dist/linux-armv7l/sned-${VERSION}-linux-armv7l.tar.gz"
assert_file "${ARM_ARCHIVE}"
tar -tzf "${ARM_ARCHIVE}" | grep -Fx 'sned' >/dev/null
assert_log_contains 'cargo zigbuild'
assert_log_contains '--target armv7-unknown-linux-gnueabihf'

FAIL_TARGET="${SMOKE_ROOT}/failed-target"
if PATH="${FAKE_BIN_DIR}:${PATH}" \
    FAKE_LOG="${FAKE_LOG}" \
    FAKE_RUSTUP_FAIL=1 \
    CARGO_TARGET_DIR="${FAIL_TARGET}" \
    "${PROJECT_ROOT}/scripts/build-macos-arm64.sh" >/dev/null 2>&1; then
    printf '%s\n' 'target installation failure did not stop the build' >&2
    exit 1
fi

if [[ -e "${FAIL_TARGET}/dist/macos-arm64/sned-${VERSION}-macos-arm64.tar.gz" ]]; then
    printf '%s\n' 'failed target installation left a release archive' >&2
    exit 1
fi

printf '%s\n' 'build release smoke test passed'
