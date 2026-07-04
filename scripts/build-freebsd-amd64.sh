#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

exec "${SCRIPT_DIR}/build-release-package.sh" x86_64-unknown-freebsd freebsd-amd64 "$@"
