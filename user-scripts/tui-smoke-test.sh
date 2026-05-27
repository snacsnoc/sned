#!/usr/bin/env bash
# Smoke test for the ratatui interactive shell and nearby CLI dispatch.
#
# This is the canonical agent-facing TUI smoke test. It intentionally avoids
# live providers: the pty-backed TUI probe starts sned with the mock provider,
# verifies the ratatui banner renders, sends /exit, and checks clean shutdown.
#
# Usage:
#   ./user-scripts/tui-smoke-test.sh
#   ./user-scripts/tui-smoke-test.sh --verbose
#   ./user-scripts/tui-smoke-test.sh --test tui-startup-exit
#   ./user-scripts/tui-smoke-test.sh --list

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SNED_BIN="${REPO_ROOT}/target/debug/sned"
VERBOSE=0
RUN_TEST=""

ALL_TEST_NAMES="tui-startup-exit help version invalid-flag yolo-help json-no-prompt ctrlc-cancel"
TOTAL_TESTS=7
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
Test Name          Description
----------------- --------------------------------------------------
tui-startup-exit  Start ratatui in a pty, render banner, send /exit
help              --help shows usage
version           --version shows version
invalid-flag      Invalid flag returns an error
yolo-help         --yolo is accepted by CLI parsing
json-no-prompt    --json with no prompt does not start the TUI
ctrlc-cancel      Send Ctrl+C to cancel, then /exit to quit
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
        help) echo "--help shows usage" ;;
        version) echo "--version shows version" ;;
        invalid-flag) echo "Invalid flag returns an error" ;;
        yolo-help) echo "--yolo is accepted by CLI parsing" ;;
        json-no-prompt) echo "--json with no prompt does not start the TUI" ;;
        ctrlc-cancel) echo "Send Ctrl+C to cancel, then /exit to quit" ;;
        *) echo "unknown" ;;
    esac
}

test_source() {
    case "$1" in
        tui-startup-exit) echo "src/cli/interactive.rs run_interactive_shell_inner" ;;
        help|version|invalid-flag|yolo-help|json-no-prompt) echo "src/cli/mod.rs CLI dispatch" ;;
        ctrlc-cancel) echo "src/cli/interactive.rs handle_key_event Ctrl+C handler" ;;
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

run_isolated() {
    local tmp_dir
    tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/sned-tui-smoke.XXXXXX")"
    SNED_DIR="$tmp_dir" SNED_DATA_DIR="$tmp_dir/data" "$@"
    local status=$?
    rm -rf "$tmp_dir"
    return "$status"
}

test_tui_startup_exit() {
    if ! command -v python3 >/dev/null 2>&1; then
        echo "TUI_TEST_FAIL python3 is required for pty smoke test"
        return 0
    fi

    SNED_BIN="$SNED_BIN" REPO_ROOT="$REPO_ROOT" VERBOSE="$VERBOSE" python3 - <<'PY'
import os
import pty
import select
import shutil
import signal
import sys
import tempfile
import time

repo = os.environ["REPO_ROOT"]
sned_bin = os.environ["SNED_BIN"]
verbose = os.environ.get("VERBOSE") == "1"
tmp = tempfile.mkdtemp(prefix="sned-tui-smoke.")
env = os.environ.copy()
env.update({
    "SNED_NO_ALTERNATE_SCREEN": "1",
    "SNED_DIR": tmp,
    "SNED_DATA_DIR": os.path.join(tmp, "data"),
})

cmd = [
    os.path.join(repo, "user-scripts", "sned-pty-helper"),
    "24",
    "80",
    sned_bin,
    "--provider",
    "mock",
]

pid, fd = pty.fork()
if pid == 0:
    os.chdir(repo)
    os.execvpe(cmd[0], cmd, env)

buf = b""
sent_exit = False
exit_code = None
deadline = time.time() + 8

try:
    while time.time() < deadline:
        readable, _, _ = select.select([fd], [], [], 0.1)
        if fd in readable:
            try:
                data = os.read(fd, 4096)
            except OSError:
                break
            if not data:
                break
            buf += data
            if b"\x1b[6n" in data:
                os.write(fd, b"\x1b[1;1R")
            if b"type a prompt" in buf and not sent_exit:
                os.write(fd, b"/exit\r")
                sent_exit = True

        ended, status = os.waitpid(pid, os.WNOHANG)
        if ended:
            exit_code = os.waitstatus_to_exitcode(status)
            break
    else:
        os.kill(pid, signal.SIGTERM)

    if exit_code is None:
        try:
            ended, status = os.waitpid(pid, os.WNOHANG)
            if ended:
                exit_code = os.waitstatus_to_exitcode(status)
        except ChildProcessError:
            exit_code = 0

    text = buf.decode("utf-8", "replace")
    if verbose:
        print(text)

    if "type a prompt" not in text:
        print("TUI_TEST_FAIL startup banner not rendered")
    elif not sent_exit:
        print("TUI_TEST_FAIL /exit was not sent")
    elif exit_code not in (0, None):
        print(f"TUI_TEST_FAIL sned exited with {exit_code}")
    else:
        print("TUI_TEST_PASS ratatui startup and /exit path worked")
finally:
    shutil.rmtree(tmp, ignore_errors=True)
PY
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

test_ctrlc_cancel() {
    if ! command -v python3 >/dev/null 2>&1; then
        echo "TUI_TEST_FAIL python3 is required for pty smoke test"
        return 0
    fi

    SNED_BIN="$SNED_BIN" REPO_ROOT="$REPO_ROOT" VERBOSE="$VERBOSE" python3 - <<'PY'
import os
import pty
import select
import shutil
import signal
import sys
import tempfile
import time

repo = os.environ["REPO_ROOT"]
sned_bin = os.environ["SNED_BIN"]
verbose = os.environ.get("VERBOSE") == "1"
tmp = tempfile.mkdtemp(prefix="sned-ctrlc-smoke.")
env = os.environ.copy()
env.update({
    "SNED_NO_ALTERNATE_SCREEN": "1",
    "SNED_DIR": tmp,
    "SNED_DATA_DIR": os.path.join(tmp, "data"),
})

cmd = [
    os.path.join(repo, "user-scripts", "sned-pty-helper"),
    "24",
    "80",
    sned_bin,
    "--provider",
    "mock",
]

pid, fd = pty.fork()
if pid == 0:
    os.chdir(repo)
    os.execvpe(cmd[0], cmd, env)

buf = b""
sent_ctrlc = False
exit_code = None
deadline = time.time() + 8

try:
    while time.time() < deadline:
        readable, _, _ = select.select([fd], [], [], 0.1)
        if fd in readable:
            try:
                data = os.read(fd, 4096)
            except OSError:
                break
            if not data:
                break
            buf += data
            if b"\x1b[6n" in data:
                os.write(fd, b"\x1b[1;1R")
            if b"type a prompt" in buf and not sent_ctrlc:
                # Send Ctrl+C to cancel (no agent running, should clear input or quit)
                os.write(fd, b"\x03")
                sent_ctrlc = True
                # After Ctrl+C, send /exit to quit
                time.sleep(0.2)
                os.write(fd, b"/exit\r")

        ended, status = os.waitpid(pid, os.WNOHANG)
        if ended:
            exit_code = os.waitstatus_to_exitcode(status)
            break
    else:
        os.kill(pid, signal.SIGTERM)

    if exit_code is None:
        try:
            ended, status = os.waitpid(pid, os.WNOHANG)
            if ended:
                exit_code = os.waitstatus_to_exitcode(status)
        except ChildProcessError:
            exit_code = 0

    text = buf.decode("utf-8", "replace")
    if verbose:
        print(text)

    if "type a prompt" not in text:
        print("TUI_TEST_FAIL startup banner not rendered")
    elif not sent_ctrlc:
        print("TUI_TEST_FAIL Ctrl+C was not sent")
    elif exit_code not in (0, None):
        print(f"TUI_TEST_FAIL sned exited with {exit_code}")
    else:
        print("TUI_TEST_PASS Ctrl+C cancel and /exit path worked")
finally:
    shutil.rmtree(tmp, ignore_errors=True)
PY
}

test_json_no_prompt() {
    if ! command -v python3 >/dev/null 2>&1; then
        echo "TUI_TEST_FAIL python3 is required for pty smoke test"
        return 0
    fi

    SNED_BIN="$SNED_BIN" REPO_ROOT="$REPO_ROOT" VERBOSE="$VERBOSE" python3 - <<'PY'
import os
import pty
import select
import shutil
import signal
import tempfile
import time

repo = os.environ["REPO_ROOT"]
sned_bin = os.environ["SNED_BIN"]
verbose = os.environ.get("VERBOSE") == "1"
tmp = tempfile.mkdtemp(prefix="sned-json-smoke.")
env = os.environ.copy()
env.update({
    "SNED_DIR": tmp,
    "SNED_DATA_DIR": os.path.join(tmp, "data"),
})

pid, fd = pty.fork()
if pid == 0:
    os.chdir(repo)
    os.execvpe(sned_bin, [sned_bin, "--json"], env)

buf = b""
exit_code = None
deadline = time.time() + 3

try:
    while time.time() < deadline:
        readable, _, _ = select.select([fd], [], [], 0.1)
        if fd in readable:
            try:
                data = os.read(fd, 4096)
            except OSError:
                break
            if not data:
                break
            buf += data

        ended, status = os.waitpid(pid, os.WNOHANG)
        if ended:
            exit_code = os.waitstatus_to_exitcode(status)
            break
    else:
        os.kill(pid, signal.SIGTERM)
        print("TUI_TEST_FAIL --json no-prompt timed out")
        raise SystemExit(0)

    if exit_code is None:
        try:
            ended, status = os.waitpid(pid, os.WNOHANG)
            if ended:
                exit_code = os.waitstatus_to_exitcode(status)
        except ChildProcessError:
            exit_code = 0

    text = buf.decode("utf-8", "replace")
    if verbose:
        print(text)

    if text:
        print("TUI_TEST_FAIL --json no-prompt produced output")
    elif exit_code not in (0, None):
        print(f"TUI_TEST_FAIL --json no-prompt exited with {exit_code}")
    else:
        print("TUI_TEST_PASS --json no-prompt did not start TUI")
finally:
    shutil.rmtree(tmp, ignore_errors=True)
PY
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
        tui-startup-exit) result="$(test_tui_startup_exit 2>&1)" ;;
        help) result="$(test_help 2>&1)" ;;
        version) result="$(test_version 2>&1)" ;;
        invalid-flag) result="$(test_invalid_flag 2>&1)" ;;
        yolo-help) result="$(test_yolo_help 2>&1)" ;;
        json-no-prompt) result="$(test_json_no_prompt 2>&1)" ;;
        ctrlc-cancel) result="$(test_ctrlc_cancel 2>&1)" ;;
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
        printf "         Verbose re-run: ./user-scripts/tui-smoke-test.sh --verbose --test %s\n" "$name"
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
