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

repack_release() {
    local target_dir="$1"
    local artifact_suffix="$2"
    local version="$3"
    local artifact_dir="${target_dir}/dist/${artifact_suffix}"
    local package_dir="${artifact_dir}/sned-${version}-${artifact_suffix}"
    local tarball="${artifact_dir}/sned-${version}-${artifact_suffix}.tar.gz"

    if [[ ! -f "${package_dir}/sned" ]]; then
        printf '%s\n' "expected package binary not found at ${package_dir}/sned" >&2
        return 1
    fi

    mkdir -p "${artifact_dir}"
    rm -f "${tarball}"
    tar -C "${package_dir}" -czf "${tarball}" sned
    printf '%s\n' "packaged ${tarball}"
}
