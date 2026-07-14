#!/usr/bin/env bash
# Canonical smoke coverage for the ratatui interactive shell and nearby CLI dispatch.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SNED_BIN="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}/debug/sned"
VERBOSE=0
RUN_TEST=""

ALL_TEST_NAMES="tui-startup-exit tui-user-echo tui-turn-indicators tui-approval-scroll tui-approval-under-backpressure tui-long-completion-navigation tui-history-navigation tui-slash-commands tui-auto-scroll tui-model-switch tui-busy-exit help version invalid-flag yolo-help json-no-prompt ctrlc-quit-empty"
TOTAL_TESTS=17
PASS_COUNT=0
FAIL_COUNT=0
RESULTS=""
FAIL_NAMES=""

usage() {
    cat <<'HELP'
Usage: tui-smoke-test.sh [OPTIONS]

Smoke test for the sned ratatui TUI and CLI dispatch.

OPTIONS:
  --verbose, -v      Show captured pty/command output
  --test NAME, -t    Run a single test by name (see --list)
  --list             List all test names
  --help, -h         Show this help

NOTES:
  - Uses the mock provider; no API key or network is required.
  - The TUI probe runs sned inside a pty with SNED_NO_ALTERNATE_SCREEN=1.
  - This is a smoke test, not a complete state-machine interaction suite.

EXIT CODES:
  0  All selected tests passed
  1  One or more test failures
  2  Setup error
HELP
}

list_tests() {
    cat <<'LIST'
Test Name              Description
--------------------- --------------------------------------------------
tui-startup-exit      Start ratatui in a pty, render banner, send /exit
tui-user-echo         Type a prompt, verify ❯ prefix appears in transcript
tui-turn-indicators   Type a prompt, verify ♦ and ─ turn markers appear
tui-approval-scroll   Scroll away, then verify approval prompt stays visible
tui-approval-under-backpressure Flood output, block input, then approve after render
tui-long-completion-navigation Scroll completion and transcript at both boundaries
tui-history-navigation Type prompts, press Up arrow, verify previous prompt appears
tui-slash-commands    Search /help, then reject an unknown slash command locally
tui-auto-scroll       Type multiple prompts, verify output scrolls to show latest
tui-model-switch      Type /model mock/mock-model, verify switch message renders
tui-busy-exit         While mock provider streams output, send /exit and verify prompt shutdown
help                  --help shows usage
version               --version shows version
invalid-flag          Invalid flag returns an error
yolo-help             --yolo is accepted by CLI parsing
json-no-prompt        --json with no prompt does not start the TUI
ctrlc-quit-empty      Ctrl+C on empty input quits cleanly
LIST
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --verbose|-v)
            VERBOSE=1
            shift
            ;;
        --test|-t)
            if [ "$#" -lt 2 ]; then
                echo "ERROR: --test requires a test name" >&2
                exit 2
            fi
            RUN_TEST="$2"
            TOTAL_TESTS=1
            shift 2
            ;;
        --list|-l)
            list_tests
            exit 0
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

test_description() {
    case "$1" in
        tui-startup-exit) echo "Start ratatui in a pty, render banner, send /exit" ;;
        tui-user-echo) echo "Type a prompt, verify ❯ prefix appears in transcript" ;;
        tui-turn-indicators) echo "Type a prompt, verify ✦ and ─ turn markers appear" ;;
        tui-approval-scroll) echo "Scroll away, then verify approval prompt stays visible" ;;
        tui-approval-under-backpressure) echo "Flood output, block input, then approve after render" ;;
        tui-long-completion-navigation) echo "Scroll completion and transcript at both boundaries" ;;
        tui-history-navigation) echo "Type prompts, press Up arrow, verify previous prompt appears in input" ;;
        tui-slash-commands) echo "Search /help, then reject an unknown slash command locally" ;;
        tui-auto-scroll) echo "Type multiple prompts, verify output scrolls to show latest" ;;
        tui-model-switch) echo "Type /model mock/mock-model, verify switch message renders" ;;
        tui-busy-exit) echo "While mock provider streams output, send /exit and verify prompt shutdown" ;;
        help) echo "--help shows usage" ;;
        version) echo "--version shows version" ;;
        invalid-flag) echo "Invalid flag returns an error" ;;
        yolo-help) echo "--yolo is accepted by CLI parsing" ;;
        json-no-prompt) echo "--json with no prompt does not start the TUI" ;;
        ctrlc-quit-empty) echo "Ctrl+C on empty input quits cleanly" ;;
        *) echo "unknown" ;;
    esac
}

test_source() {
    case "$1" in
        tui-startup-exit) echo "src/cli/interactive.rs run_interactive_shell_inner" ;;
        tui-user-echo) echo "src/cli/tui/app.rs push_user_message / src/cli/interactive.rs Enter handler" ;;
        tui-turn-indicators) echo "src/core/agent_loop.rs assistant turn indicator / src/cli/tui/app.rs push_turn_separator" ;;
        tui-approval-scroll) echo "src/cli/interactive.rs drain_output approval scroll path / src/core/approval.rs begin_approval_prompt" ;;
        tui-approval-under-backpressure) echo "src/cli/output.rs priority delivery / src/cli/interactive.rs approval input routing" ;;
        tui-long-completion-navigation) echo "src/cli/tui/app.rs completion scroll boundaries / src/cli/interactive.rs navigation routing" ;;
        tui-history-navigation) echo "src/cli/interactive.rs handle_key_event Up/Down arrow history / src/cli/tui/history.rs FileHistory" ;;
        tui-slash-commands) echo "src/cli/interactive.rs help overlay and unknown-command routing / src/cli/slash_commands.rs registry" ;;
        tui-auto-scroll) echo "src/cli/tui/app.rs scroll_mode / src/cli/interactive.rs drain_output auto-scroll" ;;
        tui-model-switch) echo "src/cli/interactive.rs handle_cli_only_command ModelSwitch / src/core/agent_loop.rs set_provider" ;;
        tui-busy-exit) echo "src/cli/interactive.rs busy-state shutdown path / src/providers/mock.rs busy_stream_scenario" ;;
        help|version|invalid-flag|yolo-help|json-no-prompt) echo "src/cli/mod.rs CLI dispatch" ;;
        ctrlc-quit-empty) echo "src/cli/interactive.rs handle_key_event Ctrl+C on empty input" ;;
        *) echo "unknown" ;;
    esac
}

is_known_test() {
    local wanted="$1"
    local name
    for name in $ALL_TEST_NAMES; do
        if [ "$name" = "$wanted" ]; then
            return 0
        fi
    done
    return 1
}

if [ -n "$RUN_TEST" ] && ! is_known_test "$RUN_TEST"; then
    echo "ERROR: Unknown test name '$RUN_TEST'" >&2
    list_tests >&2
    exit 2
fi

echo "Building sned (debug)..."
if ! cargo build; then
    echo "FAIL: cargo build failed" >&2
    exit 2
fi
echo ""

test_pty_scenario() {
    if ! command -v python3 >/dev/null 2>&1; then
        echo "TUI_TEST_FAIL python3 is required for pty smoke test"
        return 0
    fi

    SNED_BIN="$SNED_BIN" REPO_ROOT="$REPO_ROOT" VERBOSE="$VERBOSE" \
        python3 "$SCRIPT_DIR/tui_smoke_harness.py" "$1"
}

test_help() {
    local output
    output="$("$SNED_BIN" --help 2>&1 || true)"
    [ "$VERBOSE" -eq 1 ] && printf '%s\n' "$output"
    if printf '%s\n' "$output" | grep -q "Usage: sned"; then
        echo "TUI_TEST_PASS --help shows usage"
    else
        echo "TUI_TEST_FAIL --help output missing"
    fi
}

test_version() {
    local output
    output="$("$SNED_BIN" --version 2>&1 || true)"
    [ "$VERBOSE" -eq 1 ] && printf '%s\n' "$output"
    if printf '%s\n' "$output" | grep -Eq "sned [0-9]+\\.[0-9]+\\.[0-9]+"; then
        echo "TUI_TEST_PASS --version shows version"
    else
        echo "TUI_TEST_FAIL --version output missing"
    fi
}

test_invalid_flag() {
    local output status
    set +e
    output="$("$SNED_BIN" --invalid-flag-xyz 2>&1)"
    status=$?
    set -e
    [ "$VERBOSE" -eq 1 ] && printf '%s\n' "$output"
    if [ "$status" -ne 0 ] && printf '%s\n' "$output" | grep -qiE "error|unexpected|unknown"; then
        echo "TUI_TEST_PASS invalid flag rejected"
    else
        echo "TUI_TEST_FAIL invalid flag not rejected"
    fi
}

test_yolo_help() {
    local output
    output="$("$SNED_BIN" --yolo --help 2>&1 || true)"
    [ "$VERBOSE" -eq 1 ] && printf '%s\n' "$output"
    if printf '%s\n' "$output" | grep -q -- "--yolo"; then
        echo "TUI_TEST_PASS --yolo accepted by parser"
    else
        echo "TUI_TEST_FAIL --yolo rejected or missing from help"
    fi
}

run_one() {
    local name="$1"
    local result reason

    if [ -n "$RUN_TEST" ] && [ "$RUN_TEST" != "$name" ]; then
        return 0
    fi

    printf "  [%d/%d] RUNNING %s\n" $((PASS_COUNT + FAIL_COUNT + 1)) "$TOTAL_TESTS" "$name"
    printf "         %s\n" "$(test_description "$name")"

    case "$name" in
        tui-*|json-no-prompt|ctrlc-quit-empty) result="$(test_pty_scenario "$name" 2>&1)" ;;
        help) result="$(test_help 2>&1)" ;;
        version) result="$(test_version 2>&1)" ;;
        invalid-flag) result="$(test_invalid_flag 2>&1)" ;;
        yolo-help) result="$(test_yolo_help 2>&1)" ;;
        *) result="TUI_TEST_FAIL unknown test" ;;
    esac

    if printf '%s\n' "$result" | grep -q "TUI_TEST_PASS"; then
        RESULTS="${RESULTS}PASS  ${name}
"
        PASS_COUNT=$((PASS_COUNT + 1))
        printf "         -> PASS\n"
    else
        reason="$(printf '%s\n' "$result" | grep "TUI_TEST_FAIL" | head -1 | cut -d' ' -f3-)"
        [ -z "$reason" ] && reason="test failed"
        RESULTS="${RESULTS}FAIL  ${name} -- ${reason}
"
        FAIL_NAMES="${FAIL_NAMES}${name}
"
        FAIL_COUNT=$((FAIL_COUNT + 1))
        printf "         -> FAIL\n"
        printf "         Likely source: %s\n" "$(test_source "$name")"
        printf "         Verbose re-run: ./scripts/tui-smoke-test.sh --verbose --test %s\n" "$name"
        if [ "$VERBOSE" -eq 1 ]; then
            printf '%s\n' "$result"
        fi
    fi
}

echo "=========================================="
echo "  sned Ratatui TUI Smoke Test"
echo "=========================================="
echo ""

for test_name in $ALL_TEST_NAMES; do
    run_one "$test_name"
done

echo ""
echo "=========================================="
echo "  Results"
echo "=========================================="
printf '%s' "$RESULTS" | sed '/^$/d; s/^/  /'
echo ""
echo "  $PASS_COUNT passed, $FAIL_COUNT failed"

if [ "$FAIL_COUNT" -gt 0 ]; then
    echo ""
    echo "  Failed tests:"
    printf '%s' "$FAIL_NAMES" | sed '/^$/d; s/^/    /'
    exit 1
fi
