#!/bin/bash
# Generate a Homebrew formula for sned.
#
# Usage:
#   ./scripts/homebrew-formula.sh [version] [sha256]
#
# Arguments:
#   version - Release version (default: reads from Cargo.toml)
#   sha256  - SHA256 of the macOS arm64 tarball (default: computed)
#
# Outputs:
#   - Prints the formula to stdout
#   - Saves to target/dist/macos-arm64/sned.rb

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
VERSION_FROM_MANIFEST=$(grep "^version" "${PROJECT_ROOT}/Cargo.toml" | head -1 | cut -d'"' -f2)
MACOS_ARM64_TARBALL="${PROJECT_ROOT}/target/dist/macos-arm64/sned-${VERSION_FROM_MANIFEST}-macos-arm64.tar.gz"

# Get version from Cargo.toml if not provided
if [[ -n "${1:-}" ]]; then
    VERSION="$1"
else
    VERSION=$(grep "^version" "${PROJECT_ROOT}/Cargo.toml" | head -1 | cut -d'"' -f2)
fi

# Compute SHA256 if not provided
if [[ -n "${2:-}" ]]; then
    SHA256="$2"
else
    if [[ -f "${MACOS_ARM64_TARBALL}" ]]; then
        SHA256=$(shasum -a 256 "${MACOS_ARM64_TARBALL}" | cut -d' ' -f1)
    else
        echo "Error: macOS arm64 tarball not found at ${MACOS_ARM64_TARBALL}"
        echo "Run ./scripts/build-macos-arm64.sh first"
        exit 1
    fi
fi

FORMULA_PATH="${PROJECT_ROOT}/target/dist/macos-arm64/sned.rb"

echo "Generating Homebrew formula..."
echo "Version: ${VERSION}"
echo "SHA256: ${SHA256}"

cat > "${FORMULA_PATH}" <<EOF
class Sned < Formula
  desc "Rust CLI for code editing"
  homepage "https://github.com/snacsnoc/sned"
  version "${VERSION}"
  url "https://github.com/snacsnoc/sned/releases/download/v#{version}/sned-#{version}-macos-arm64.tar.gz"
  sha256 "${SHA256}"
  license any_of: ["GPL-3.0-only", "Apache-2.0"]

  depends_on arch: :arm64
  depends_on macos: ">= :catalina"

  def install
    bin.install "sned" => "sned"
  end

  test do
    assert_match "Sned CLI", shell_output("#{bin}/sned --help")
    assert_match version.to_s, shell_output("#{bin}/sned --version")
  end
end
EOF

echo ""
echo "Formula generated:"
echo "Location: ${FORMULA_PATH}"
