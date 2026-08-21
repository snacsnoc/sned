release_version() {
    local project_root="$1"
    awk -F'"' '/^version = / { print $2; exit }' "${project_root}/Cargo.toml"
}

release_target_dir() {
    local project_root="$1"
    local configured_target_dir="${CARGO_TARGET_DIR:-}"

    if [[ -z "${configured_target_dir}" ]]; then
        printf '%s/target\n' "${project_root}"
    elif [[ "${configured_target_dir}" = /* ]]; then
        printf '%s\n' "${configured_target_dir}"
    else
        printf '%s/%s\n' "${project_root}" "${configured_target_dir}"
    fi
}

release_artifact_dir() {
    local target_dir="$1"
    printf '%s/release-artifacts\n' "${target_dir}"
}

release_metadata_dir() {
    local target_dir="$1"
    printf '%s/release-metadata\n' "${target_dir}"
}

repack_release() {
    local package_dir="$1"
    local tarball="$2"
    local artifact_dir
    artifact_dir="$(dirname "${tarball}")"

    if [[ ! -f "${package_dir}/sned" ]]; then
        printf '%s\n' "expected package binary not found at ${package_dir}/sned" >&2
        return 1
    fi

    mkdir -p "${artifact_dir}"
    local staged_tarball
    staged_tarball="$(mktemp "${artifact_dir}/.$(basename "${tarball}").tmp.XXXXXX")"
    if ! tar -C "${package_dir}" -czf "${staged_tarball}" sned; then
        rm -f "${staged_tarball}"
        return 1
    fi
    # A single-target rebuild changes the archive bytes, so an aggregate
    # checksum manifest must not survive with hashes for the previous archive.
    rm -f "${artifact_dir}/SHA256SUMS"
    mv -f "${staged_tarball}" "${tarball}"
    printf '%s\n' "packaged ${tarball}"
}

write_release_checksums() {
    local target_dir="$1"
    local version="$2"
    shift 2
    local artifact_dir
    artifact_dir="$(release_artifact_dir "${target_dir}")"
    local checksum_file="${artifact_dir}/SHA256SUMS"

    mkdir -p "${artifact_dir}"
    local staged_checksum_file
    staged_checksum_file="$(mktemp "${artifact_dir}/.SHA256SUMS.tmp.XXXXXX")"
    local archives=()
    local suffix
    for suffix in "$@"; do
        archives+=("${artifact_dir}/sned-${version}-${suffix}.tar.gz")
    done

    if [[ "${#archives[@]}" -eq 0 ]]; then
        rm -f "${staged_checksum_file}"
        printf '%s\n' "no release archives found for version ${version}" >&2
        return 1
    fi

    for archive_path in "${archives[@]}"; do
        if [[ ! -f "${archive_path}" ]]; then
            rm -f "${staged_checksum_file}"
            printf '%s\n' "release archive not found: ${archive_path}" >&2
            return 1
        fi
    done

    if ! (
        cd "${artifact_dir}"
        for archive_path in "${archives[@]}"; do
            archive="$(basename "${archive_path}")"
            if command -v sha256sum >/dev/null 2>&1; then
                sha256sum "${archive}"
            elif command -v shasum >/dev/null 2>&1; then
                shasum -a 256 "${archive}"
            else
                printf '%s\n' 'neither sha256sum nor shasum is available' >&2
                exit 1
            fi
        done | LC_ALL=C sort
    ) >"${staged_checksum_file}"; then
        rm -f "${staged_checksum_file}"
        printf '%s\n' 'unable to generate release checksums' >&2
        return 1
    fi

    mv -f "${staged_checksum_file}" "${checksum_file}"
    printf '%s\n' "wrote ${checksum_file}"
}
