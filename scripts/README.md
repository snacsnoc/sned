# sned Scripts

This directory contains utility scripts for building, testing, benchmarking, and distributing the `sned` CLI.

## Quick Reference

| Script | Purpose | When to Use |
|--------|---------|-------------|
| `build-universal-macos.sh` | Build universal macOS binary | Preparing a release |
| `sign-macos.sh` | Code-sign with Hardened Runtime | Pre-Notarization |
| `notarize-macos.sh` | Notarize binary with Apple | Pre-Release distribution |
| `homebrew-formula.sh` | Generate Homebrew formula | Publishing to Homebrew tap |
| `bench-vs-opencode.sh` | Benchmark vs opencode | Performance comparison |
| `smoke-test-terminals.sh` | Terminal smoke tests | Pre-release validation |

---

## Build & Distribution Scripts

### `build-universal-macos.sh`

Build a universal macOS binary (x86_64 + arm64) for distribution.

**Usage:**
```bash
./scripts/build-universal-macos.sh [--release]
```

**Options:**
- `--release` — Build in release mode (optimized)

**Output:**
- `target/universal/sned` — Universal fat binary

**Prerequisites:**
- macOS with Xcode Command Line Tools
- Rust toolchain with `aarch64-apple-darwin` and `x86_64-apple-darwin` targets

**Install targets:**
```bash
rustup target add aarch64-apple-darwin
rustup target add x86_64-apple-darwin
```

---

### `sign-macos.sh`

Sign the universal binary with Apple Developer ID and Hardened Runtime entitlements.

**Usage:**
```bash
./scripts/sign-macos.sh [identity]
```

**Arguments:**
- `identity` — Apple Developer ID Application identity (default: `"Developer ID Application: Sned"`)

**Prerequisites:**
- macOS with Xcode Command Line Tools
- Apple Developer ID certificate installed in Keychain
- Binary already built (run `build-universal-macos.sh` first)

**Example:**
```bash
./scripts/build-universal-macos.sh --release
./scripts/sign-macos.sh "Developer ID Application: Your Name"
```

---

### `notarize-macos.sh`

Notarize the signed binary with Apple and staple the notarization ticket.

**Usage:**
```bash
./scripts/notarize-macos.sh [bundle-id]
```

**Arguments:**
- `bundle-id` — Bundle identifier for notarization (default: `run.sned.cli`)

**Environment Variables:**
```bash
export APPLE_ID="your.apple.id@example.com"
export APPLE_APP_SPECIFIC_PASSWORD="app-specific-password"
export APPLE_TEAM_ID="YOUR_TEAM_ID"
```

**Prerequisites:**
- macOS with Xcode Command Line Tools
- Apple Developer account with App Store Connect API key
- Binary already signed (run `sign-macos.sh` first)

**Example:**
```bash
export APPLE_ID="your.apple.id@example.com"
export APPLE_APP_SPECIFIC_PASSWORD="app-specific-password"
export APPLE_TEAM_ID="YOUR_TEAM_ID"

./scripts/sign-macos.sh
./scripts/notarize-macos.sh
```

---

### `homebrew-formula.sh`

Generate a Homebrew formula for the `sned` CLI.

**Usage:**
```bash
./scripts/homebrew-formula.sh [version] [sha256]
```

**Arguments:**
- `version` — Release version (default: reads from `Cargo.toml`)
- `sha256` — SHA256 of the universal binary (default: computed)

**Output:**
- Prints the formula to stdout
- Saves to `target/universal/sned.rb`

**Prerequisites:**
- Universal binary built and signed (run `build-universal-macos.sh` and `sign-macos.sh`)

**Example:**
```bash
./scripts/build-universal-macos.sh --release
./scripts/sign-macos.sh
./scripts/homebrew-formula.sh 0.1.0 > sned.rb

# Submit to Homebrew tap
gh pr create --repo sned/homebrew-sned --title "Update sned to 0.1.0" --body ""
```

---

## Testing & Benchmarking Scripts

### `bench-vs-opencode.sh`

Compare `sned` performance against `opencode` CLI.

**Usage:**
```bash
./scripts/bench-vs-opencode.sh [OPTIONS]
```

**Options:**
- `--fixture <name>` — Run specific fixture: `trivial`, `medium`, `long` (default: all)
- `--provider <name>` — Provider to use (default: `anthropic`)
- `--model <name>` — Model to use (default: `claude-sonnet-4-20250514`)
- `--iterations <n>` — Number of iterations per fixture (default: 3)
- `--output <file>` — Output JSON file (default: stdout)
- `--help` — Show help message

**Requirements:**
- `sned` binary (`cargo build --release`)
- `opencode` binary (`npm install -g opencode-ai`)
- `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` set

**Example:**
```bash
# Run all fixtures
./scripts/bench-vs-opencode.sh

# Run specific fixture with custom provider
./scripts/bench-vs-opencode.sh --fixture medium --provider openai --model gpt-4o

# Save results to file
./scripts/bench-vs-opencode.sh --output benchmark-results.json
```

**Output:**
```json
{
  "fixtures": {
    "trivial": {
      "sned": { "mean_ms": 123, "memory_kb": 45678 },
      "opencode": { "mean_ms": 456, "memory_kb": 123456 },
      "speedup": 3.7
    }
  }
}
```

---

### `smoke-test-terminals.sh`

Run terminal smoke tests across different macOS terminal emulators.

**Usage:**
```bash
./scripts/smoke-test-terminals.sh
```

**Environment:**
- `CARGO_TARGET_DIR` — Custom target directory (optional)

**Test Matrix:**
- Terminal.app
- iTerm2
- Ghostty
- Alacritty

**Tests:**
1. `--help` output — Checks for ANSI corruption, duplicate prompts, missing content
2. Mock provider execution — Verifies no crash, clean exit
3. Non-TTY behavior — ANSI leak detection

**Example:**
```bash
# Run all tests
./scripts/smoke-test-terminals.sh

# Run with custom target dir
CARGO_TARGET_DIR=/tmp/sned-target ./scripts/smoke-test-terminals.sh
```

**Output:**
- Console summary with pass/fail/skip counts
- `docs/terminal-smoke-results.md` — Detailed results with date and commit

**Notes:**
- Gracefully skips unavailable terminals (allow-failure behavior)
- Ideal for CI pre-release validation

---

## Development Scripts

## Script Conventions

All scripts follow these conventions:

1. **Shebang:** `#!/bin/bash` or `#!/usr/bin/env bash`
2. **Error handling:** `set -euo pipefail` (except where noted)
3. **Directory resolution:** Use `SCRIPT_DIR` and `PROJECT_ROOT` for portability
4. **Help text:** Include usage, options, and prerequisites in comments
5. **Exit codes:** 0 for success, non-zero for failure
6. **Output:** Use colored output for status messages where applicable

---

## CI/CD Integration

These scripts are designed for CI/CD integration:

```yaml
# Example GitHub Actions workflow
jobs:
  build:
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v4
      
      - name: Build universal binary
        run: ./scripts/build-universal-macos.sh --release
      
      - name: Run smoke tests
        run: ./scripts/smoke-test-terminals.sh
        continue-on-error: true  # Allow failure for missing terminals
      
      - name: Benchmark
        run: ./scripts/bench-vs-opencode.sh --fixture trivial
        env:
          ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
```

---

## Support

For issues or questions about these scripts:

1. Check the script's help text: `./scripts/<script-name>.sh --help`
2. Review the script source for detailed comments
3. Open an issue in the Sned repository
