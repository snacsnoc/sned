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
if [[ "${FAKE_CARGO_FAIL_TARGET:-}" == "${target}" ]]; then
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

assert_not_exists() {
    if [[ -e "$1" ]]; then
        printf 'unexpected path exists: %s\n' "$1" >&2
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

if ! "${PROJECT_ROOT}/scripts/build-all-releases.sh" --help >/dev/null; then
    printf '%s\n' 'build-all-releases.sh --help failed' >&2
    exit 1
fi

SINGLE_TARGET="${SMOKE_ROOT}/single-target"
PATH="${FAKE_BIN_DIR}:${PATH}" \
    FAKE_LOG="${FAKE_LOG}" \
    CARGO_TARGET_DIR="${SINGLE_TARGET}" \
    "${PROJECT_ROOT}/scripts/build-macos-arm64.sh" --debug >/dev/null

MAC_ARCHIVE="${SINGLE_TARGET}/release-artifacts/sned-${VERSION}-macos-arm64.tar.gz"
assert_file "${MAC_ARCHIVE}"
tar -tzf "${MAC_ARCHIVE}" | grep -Fx 'sned' >/dev/null
assert_not_exists "${SINGLE_TARGET}/dist/macos-arm64"
assert_log_contains '--target aarch64-apple-darwin'
assert_log_contains "--target-dir ${SINGLE_TARGET}"
assert_log_contains '--locked'
assert_log_contains '--target aarch64-apple-darwin --target-dir'

PATH="${FAKE_BIN_DIR}:${PATH}" \
    FAKE_LOG="${FAKE_LOG}" \
    CARGO_TARGET_DIR="${SINGLE_TARGET}" \
    "${PROJECT_ROOT}/scripts/build-linux-armv7l.sh" --debug >/dev/null

ARM_ARCHIVE="${SINGLE_TARGET}/release-artifacts/sned-${VERSION}-linux-armv7l.tar.gz"
assert_file "${ARM_ARCHIVE}"

PATH="${FAKE_BIN_DIR}:${PATH}" \
    FAKE_LOG="${FAKE_LOG}" \
    CARGO_TARGET_DIR="${SINGLE_TARGET}" \
    "${PROJECT_ROOT}/scripts/build-macos-arm64.sh" --debug >/dev/null
assert_file "${MAC_ARCHIVE}"
assert_file "${ARM_ARCHIVE}"

AGGREGATE_TARGET="${SMOKE_ROOT}/aggregate-target"
PATH="${FAKE_BIN_DIR}:${PATH}" \
    FAKE_LOG="${FAKE_LOG}" \
    CARGO_TARGET_DIR="${AGGREGATE_TARGET}" \
    "${PROJECT_ROOT}/scripts/build-all-releases.sh" >/dev/null

for suffix in macos-arm64 macos-x86_64 linux-amd64 linux-armv7l freebsd-amd64; do
    archive="${AGGREGATE_TARGET}/release-artifacts/sned-${VERSION}-${suffix}.tar.gz"
    assert_file "${archive}"
    tar -tzf "${archive}" | grep -Fx 'sned' >/dev/null
done
assert_file "${AGGREGATE_TARGET}/release-artifacts/SHA256SUMS"
[[ "$(wc -l < "${AGGREGATE_TARGET}/release-artifacts/SHA256SUMS" | tr -d ' ')" == 5 ]]
if command -v sha256sum >/dev/null 2>&1; then
    (cd "${AGGREGATE_TARGET}/release-artifacts" && sha256sum -c SHA256SUMS >/dev/null)
else
    (cd "${AGGREGATE_TARGET}/release-artifacts" && shasum -a 256 -c SHA256SUMS >/dev/null)
fi
assert_not_exists "${AGGREGATE_TARGET}/dist"
assert_log_contains 'cargo zigbuild'
assert_log_contains '--target armv7-unknown-linux-gnueabihf'

PATH="${FAKE_BIN_DIR}:${PATH}" \
    FAKE_LOG="${FAKE_LOG}" \
    CARGO_TARGET_DIR="${AGGREGATE_TARGET}" \
    "${PROJECT_ROOT}/scripts/build-macos-arm64.sh" --debug >/dev/null
assert_file "${AGGREGATE_TARGET}/release-artifacts/sned-${VERSION}-macos-arm64.tar.gz"
assert_not_exists "${AGGREGATE_TARGET}/release-artifacts/SHA256SUMS"

SELECTED_TARGET="${SMOKE_ROOT}/selected-target"
PATH="${FAKE_BIN_DIR}:${PATH}" \
    FAKE_LOG="${FAKE_LOG}" \
    CARGO_TARGET_DIR="${SELECTED_TARGET}" \
    "${PROJECT_ROOT}/scripts/build-all-releases.sh" --debug \
        --target macos-arm64 --target linux-amd64 >/dev/null
assert_file "${SELECTED_TARGET}/release-artifacts/sned-${VERSION}-macos-arm64.tar.gz"
assert_file "${SELECTED_TARGET}/release-artifacts/sned-${VERSION}-linux-amd64.tar.gz"
assert_not_exists "${SELECTED_TARGET}/release-artifacts/sned-${VERSION}-freebsd-amd64.tar.gz"
assert_file "${SELECTED_TARGET}/release-artifacts/SHA256SUMS"

FAIL_TARGET="${SMOKE_ROOT}/aggregate-failure-target"
PATH="${FAKE_BIN_DIR}:${PATH}" \
    FAKE_LOG="${FAKE_LOG}" \
    CARGO_TARGET_DIR="${FAIL_TARGET}" \
    "${PROJECT_ROOT}/scripts/build-macos-arm64.sh" >/dev/null
FAIL_MAC_ARCHIVE="${FAIL_TARGET}/release-artifacts/sned-${VERSION}-macos-arm64.tar.gz"
assert_file "${FAIL_MAC_ARCHIVE}"
CHECKSUM_SENTINEL="${FAIL_TARGET}/release-artifacts/SHA256SUMS"
printf '%s\n' sentinel > "${CHECKSUM_SENTINEL}"

if PATH="${FAKE_BIN_DIR}:${PATH}" \
    FAKE_LOG="${FAKE_LOG}" \
    FAKE_CARGO_FAIL_TARGET='x86_64-unknown-linux-gnu' \
    CARGO_TARGET_DIR="${FAIL_TARGET}" \
    "${PROJECT_ROOT}/scripts/build-all-releases.sh" \
        --target macos-arm64 --target linux-amd64 >/dev/null 2>&1; then
    printf '%s\n' 'aggregate target failure did not stop the build' >&2
    exit 1
fi
assert_file "${FAIL_MAC_ARCHIVE}"
assert_not_exists "${CHECKSUM_SENTINEL}"

if PATH="${FAKE_BIN_DIR}:${PATH}" \
    FAKE_LOG="${FAKE_LOG}" \
    FAKE_RUSTUP_FAIL=1 \
    CARGO_TARGET_DIR="${SMOKE_ROOT}/rustup-failure-target" \
    "${PROJECT_ROOT}/scripts/build-macos-arm64.sh" >/dev/null 2>&1; then
    printf '%s\n' 'target installation failure did not stop the build' >&2
    exit 1
fi

if [[ -e "${SMOKE_ROOT}/rustup-failure-target/release-artifacts/sned-${VERSION}-macos-arm64.tar.gz" ]]; then
    printf '%s\n' 'failed target installation left a release archive' >&2
    exit 1
fi

printf '%s\n' 'build release smoke test passed'
