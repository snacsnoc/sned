#!/bin/bash
# Generate a Homebrew formula for sned.
#
# Usage:
#   ./scripts/homebrew-formula.sh [version] [sha256]
#
# Arguments:
#   version - Release version (default: reads from Cargo.toml)
#   sha256  - SHA256 of the universal binary (default: computed)
#
# Outputs:
#   - Prints the formula to stdout
#   - Saves to target/universal/sned.rb

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
UNIVERSAL_BINARY="${PROJECT_ROOT}/target/universal/sned"

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
    if [[ -f "${UNIVERSAL_BINARY}" ]]; then
        SHA256=$(shasum -a 256 "${UNIVERSAL_BINARY}" | cut -d' ' -f1)
    else
        echo "❌ Error: Universal binary not found at ${UNIVERSAL_BINARY}"
        echo "   Run ./scripts/build-universal-macos.sh first"
        exit 1
    fi
fi

FORMULA_PATH="${PROJECT_ROOT}/target/universal/sned.rb"

echo "🍺 Generating Homebrew formula..."
echo "   Version: ${VERSION}"
echo "   SHA256: ${SHA256}"

cat > "${FORMULA_PATH}" <<EOF
class Sned < Formula
  desc "AI coding assistant in your terminal"
  homepage "https://github.com/sned-run/sned"
  version "${VERSION}"
  url "https://github.com/sned-run/sned/releases/download/v#{version}/sned-#{version}-macos-universal.tar.gz"
  sha256 "${SHA256}"
  license "MIT"

  depends_on arch: [:x86_64, :arm64]
  depends_on macos: ">= :catalina"

  def install
    bin.install "sned" => "sned"
  end

  test do
    assert_match "Sned CLI", shell_output("#{bin}/sned --help")
    assert_match "0.1.0", shell_output("#{bin}/sned --version")
  end
end
EOF

echo ""
echo "✅ Formula generated!"
echo "   Location: ${FORMULA_PATH}"
echo ""
echo "To test locally:"
echo "   brew install --formula ${FORMULA_PATH}"
echo ""
echo "To publish to a tap:"
echo "   1. Create a GitHub repo: sned-run/homebrew-tap"
echo "   2. Add formula to Formula/sned.rb"
echo "   3. Push to the tap repo"
echo ""
echo "Users can then install with:"
echo "   brew tap sned-run/tap"
echo "   brew install sned"
