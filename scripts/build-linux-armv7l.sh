#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

exec "${SCRIPT_DIR}/build-release-package.sh" armv7-unknown-linux-gnueabihf linux-armv7l "$@"
