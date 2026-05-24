#!/usr/bin/env bash
# TUI smoke test for sned ratatui-based interactive shell.
#
# Tests CLI behavior and ratatui TUI initialization.
# Note: Full interactive TUI testing requires a real terminal.
# These tests verify startup, error handling, and one-shot mode.
#
# Prerequisites:
#   - cargo build completed
#   - MINIMAX_API_KEY set
#
# Usage:
#   ./user-scripts/tui-smoke-test.sh            # run all tests
#   ./user-scripts/tui-smoke-test.sh --verbose   # show expect output
#   ./user-scripts/tui-smoke-test.sh --test startup  # run one test

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SNED_BIN="${REPO_ROOT}/target/debug/sned"
VERBOSE=0
RUN_TEST=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --verbose|-v) VERBOSE=1; shift ;;
        --test|-t)   RUN_TEST="$2"; shift 2 ;;
        --list|-l)
            echo "Test Name        Description"
            echo "---------------- ----------------------------------------------"
            echo "startup          Banner renders on launch"
            echo "one_shot         --prompt mode works (non-TUI path)"
            echo "yolo             --yolo flag accepted"
            echo "help             --help shows usage"
            echo "version          --version shows version"
            echo "invalid_flag     Invalid flag shows error"
            exit 0 ;;
        --help|-h)
            cat <<'HELP'
Usage: tui-smoke-test.sh [OPTIONS]

Smoke test for the sned ratatui TUI and CLI.

OPTIONS:
  --verbose, -v      Show expect interaction output
  --test NAME, -t    Run a single test by name (see --list)
  --list             List all test names
  --help, -h         Show this help

PREREQUISITES:
  - cargo build completed
  - MINIMAX_API_KEY environment variable set

EXIT CODES:
  0  All tests passed
  1  One or more test failures
  2  Setup error
HELP
            exit 0 ;;
        *) echo "Unknown argument: $1" >&2; exit 2 ;;
    esac
done

echo "Building sned (debug)..."
if ! cargo build 2>&1; then
    echo "FAIL: cargo build failed" >&2
    exit 2
fi
echo ""

if [[ -z "${MINIMAX_API_KEY:-}" ]]; then
    echo "FAIL: MINIMAX_API_KEY not set" >&2
    exit 2
fi

PASS_COUNT=0
FAIL_COUNT=0
FAIL_NAMES=()
RESULTS=()
TOTAL_TESTS=6
TEST_TIMEOUT=30

declare -A TEST_DESC TEST_SOURCE
TEST_DESC[startup]="TUI initializes and banner renders"
TEST_SOURCE[startup]="src/cli/interactive.rs ratatui::init()"

TEST_DESC[one_shot]="--prompt mode executes task without TUI"
TEST_SOURCE[one_shot]="src/cli/mod.rs run_task() non-interactive path"

TEST_DESC[yolo]="--yolo flag accepted for auto-approval"
TEST_SOURCE[yolo]="src/cli/mod.rs TaskOptions parsing"

TEST_DESC[help]="--help shows usage information"
TEST_SOURCE[help]="src/cli/mod.rs CLI argument parsing"

TEST_DESC[version]="--version shows version string"
TEST_SOURCE[version]="src/cli/mod.rs version handling"

TEST_DESC[invalid_flag]="Invalid flag shows error message"
TEST_SOURCE[invalid_flag]="src/cli/mod.rs argument validation"

run_test() {
    local name="$1"
    shift
    local test_func="$1"
    shift

    if [[ -n "$RUN_TEST" && "$RUN_TEST" != "$name" ]]; then
        return 0
    fi

    local desc="${TEST_DESC[$name]:-unknown}"
    local source="${TEST_SOURCE[$name]:-unknown}"

    printf "  [%d/%d] RUNNING %s\n" $((PASS_COUNT + FAIL_COUNT + 1)) "$TOTAL_TESTS" "$name"
    printf "         %s\n" "$desc"

    local result
    result=$("$test_func" "$@" 2>&1)

    if echo "$result" | grep -q "TUI_TEST_PASS"; then
        RESULTS+=("PASS  $name")
        printf "         -> PASS\n"
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        local reason
        reason=$(echo "$result" | grep "TUI_TEST_FAIL" | head -1 | cut -d' ' -f3- | xargs) || reason="test failed"
        RESULTS+=("FAIL  $name -- $reason")
        FAIL_NAMES+=("$name")
        FAIL_COUNT=$((FAIL_COUNT + 1))
        printf "         -> FAIL\n"
        printf "         Likely source: %s\n" "$source"
        printf "         Verbose re-run: ./user-scripts/tui-smoke-test.sh --verbose --test %s\n" "$name"
    fi
}

test_startup() {
    # Test that sned starts and produces output (agent response or banner)
    local output
    output=$(echo "test" | timeout 5 $SNED_BIN --provider minimax --model "MiniMax-M2.7" --yolo 2>&1 || true)
    if echo "$output" | grep -qE "(Sned|AI|help|Session|turn|tokens)"; then
        echo "TUI_TEST_PASS startup output rendered"
    else
        echo "TUI_TEST_FAIL no startup output"
    fi
}

test_one_shot() {
    # Test one-shot mode (non-TUI path with positional prompt)
    local output
    output=$(timeout 30 $SNED_BIN --provider minimax --model "MiniMax-M2.7" --yolo "say hi" 2>&1 || true)
    if echo "$output" | grep -qE "(Sned|AI|Hi|Hello|How can|session|turn)"; then
        echo "TUI_TEST_PASS one_shot mode executed"
    else
        echo "TUI_TEST_FAIL one_shot mode did not produce expected output"
    fi
}

test_yolo() {
    # Verify --yolo flag is accepted (doesn't crash on parse)
    local output
    output=$(echo "test" | timeout 5 $SNED_BIN --provider minimax --model "MiniMax-M2.7" --yolo 2>&1 || true)
    # Should not show "unknown flag" error
    if ! echo "$output" | grep -q "unknown flag\|unexpected argument"; then
        echo "TUI_TEST_PASS --yolo flag accepted"
    else
        echo "TUI_TEST_FAIL --yolo flag rejected"
    fi
}

test_help() {
    local output
    output=$($SNED_BIN --help 2>&1 || true)
    if echo "$output" | grep -qE "(Usage|OPTIONS|COMMANDS|help)"; then
        echo "TUI_TEST_PASS --help shows usage"
    else
        echo "TUI_TEST_FAIL --help output missing"
    fi
}

test_version() {
    local output
    output=$($SNED_BIN --version 2>&1 || true)
    if echo "$output" | grep -qE "[0-9]+\.[0-9]+\.[0-9]+|sned"; then
        echo "TUI_TEST_PASS --version shows version"
    else
        echo "TUI_TEST_FAIL --version output missing"
    fi
}

test_invalid_flag() {
    local output
    output=$($SNED_BIN --invalid-flag-xyz 2>&1 || true)
    if echo "$output" | grep -qE "(error|unknown|unexpected)"; then
        echo "TUI_TEST_PASS invalid flag rejected"
    else
        echo "TUI_TEST_FAIL invalid flag not rejected"
    fi
}

ALL_TEST_NAMES=(startup one_shot yolo help version invalid_flag)

if [[ -n "$RUN_TEST" ]]; then
    found=0
    for t in "${ALL_TEST_NAMES[@]}"; do
        if [[ "$RUN_TEST" == "$t" ]]; then found=1; break; fi
    done
    if [[ $found -eq 0 ]]; then
        echo "ERROR: Unknown test name '$RUN_TEST'" >&2
        echo "Available tests:" >&2
        for t in "${ALL_TEST_NAMES[@]}"; do
            echo "  $t" >&2
        done
        exit 2
    fi
fi

echo "=========================================="
echo "  sned Ratatui TUI Smoke Test"
echo "=========================================="
echo ""

run_test "startup" "test_startup"
run_test "one_shot" "test_one_shot"
run_test "yolo" "test_yolo"
run_test "help" "test_help"
run_test "version" "test_version"
run_test "invalid_flag" "test_invalid_flag"

echo ""
echo "=========================================="
echo "  Results"
echo "=========================================="

for r in "${RESULTS[@]}"; do
    echo "  $r"
done

echo ""
echo "  $PASS_COUNT passed, $FAIL_COUNT failed"

if [[ $FAIL_COUNT -gt 0 ]]; then
    echo ""
    echo "  Failed tests:"
    for name in "${FAIL_NAMES[@]}"; do
        printf "    %-22s -> %s\n" "$name" "${TEST_SOURCE[$name]}"
    done
    echo ""
    echo "  Re-run with: ./user-scripts/tui-smoke-test.sh --verbose --test <name>"
    exit 1
fi
