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

ALL_TEST_NAMES="tui-startup-exit tui-user-echo tui-turn-indicators tui-approval-scroll tui-history-navigation tui-slash-commands tui-auto-scroll tui-model-switch help version invalid-flag yolo-help json-no-prompt ctrlc-quit-empty"
TOTAL_TESTS=14
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
tui-history-navigation Type prompts, press Up arrow, verify previous prompt appears
tui-slash-commands    Type /help, verify help text renders in output
tui-auto-scroll       Type multiple prompts, verify output scrolls to show latest
tui-model-switch      Type /model anthropic/claude-sonnet-4, verify switch message renders
help                  --help shows usage
version               --version shows version
invalid-flag           Invalid flag returns an error
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
        tui-history-navigation) echo "Type prompts, press Up arrow, verify previous prompt appears in input" ;;
        tui-slash-commands) echo "Type /help, verify help text renders in output" ;;
        tui-auto-scroll) echo "Type multiple prompts, verify output scrolls to show latest" ;;
        tui-model-switch) echo "Type /model anthropic/claude-sonnet-4, verify switch message renders" ;;
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
        tui-history-navigation) echo "src/cli/interactive.rs handle_key_event Up/Down arrow history / src/cli/tui/history.rs FileHistory" ;;
        tui-slash-commands) echo "src/cli/interactive.rs handle_cli_only_command / src/cli/slash_commands.rs format_help_text" ;;
        tui-auto-scroll) echo "src/cli/tui/app.rs scroll_mode / src/cli/interactive.rs drain_output auto-scroll" ;;
        tui-model-switch) echo "src/cli/interactive.rs handle_cli_only_command ModelSwitch / src/core/agent_loop.rs set_provider" ;;
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

test_tui_startup_exit() {
    if ! command -v python3 >/dev/null 2>&1; then
        echo "TUI_TEST_FAIL python3 is required for pty smoke test"
        return 0
    fi

    SNED_BIN="$SNED_BIN" REPO_ROOT="$REPO_ROOT" VERBOSE="$VERBOSE" python3 - <<'PY'
import os
import pty
import re
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

test_tui_user_echo() {
    if ! command -v python3 >/dev/null 2>&1; then
        echo "TUI_TEST_FAIL python3 is required for pty smoke test"
        return 0
    fi

    SNED_BIN="$SNED_BIN" REPO_ROOT="$REPO_ROOT" VERBOSE="$VERBOSE" python3 - <<'PY'
import os
import pty
import re
import select
import shutil
import signal
import tempfile
import time

repo = os.environ["REPO_ROOT"]
sned_bin = os.environ["SNED_BIN"]
verbose = os.environ.get("VERBOSE") == "1"
tmp = tempfile.mkdtemp(prefix="sned-user-echo.")
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
sent_prompt = False
sent_exit = False
exit_code = None
deadline = time.time() + 10

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
            if b"type a prompt" in buf and not sent_prompt:
                os.write(fd, b"hello world\r")
                sent_prompt = True
            if sent_prompt and b"hello world" in buf and not sent_exit:
                time.sleep(0.5)
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

    # Check that the user message echo prefix appears in the transcript.
    # The ❯ character is UTF-8 encoded as \xe2\x9d\xaf in the pty output.
    has_prompt_prefix = b"\xe2\x9d\xaf" in buf or "\u276f" in text
    if not sent_prompt:
        print("TUI_TEST_FAIL prompt was not sent")
    elif not has_prompt_prefix:
        print("TUI_TEST_FAIL user message echo missing \u276f prefix in transcript")
    elif not sent_exit:
        print("TUI_TEST_FAIL /exit was not sent")
    elif exit_code not in (0, None):
        print(f"TUI_TEST_FAIL sned exited with {exit_code}")
    else:
        print("TUI_TEST_PASS user message \u276f prefix appeared in transcript")
finally:
    shutil.rmtree(tmp, ignore_errors=True)
PY
}

test_tui_turn_indicators() {
    if ! command -v python3 >/dev/null 2>&1; then
        echo "TUI_TEST_FAIL python3 is required for pty smoke test"
        return 0
    fi

    SNED_BIN="$SNED_BIN" REPO_ROOT="$REPO_ROOT" VERBOSE="$VERBOSE" python3 - <<'PY'
import os
import pty
import re
import select
import shutil
import signal
import tempfile
import time

repo = os.environ["REPO_ROOT"]
sned_bin = os.environ["SNED_BIN"]
verbose = os.environ.get("VERBOSE") == "1"
tmp = tempfile.mkdtemp(prefix="sned-turn-ind.")
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
sent_prompt = False
sent_exit = False
exit_code = None
deadline = time.time() + 12

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
            if b"type a prompt" in buf and not sent_prompt:
                os.write(fd, b"hello\r")
                sent_prompt = True
            # Wait for mock response + turn separator, then exit.
            # The mock provider responds with "Mock provider response - task completed successfully".
            # After the turn completes, a ─ separator and elapsed time appear.
            if sent_prompt and b"Mock provider" in buf and not sent_exit:
                time.sleep(1.0)
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

    # The ✦ assistant turn indicator is UTF-8 \xe2\x9c\xa6.
    has_assistant_indicator = b"\xe2\x9c\xa6" in buf or "\u2666" in text
    # The ─ turn separator is UTF-8 \xe2\x94\x80.
    has_turn_separator = b"\xe2\x94\x80" in buf or "\u2500" in text

    if not sent_prompt:
        print("TUI_TEST_FAIL prompt was not sent")
    elif not has_assistant_indicator:
        print("TUI_TEST_FAIL assistant turn indicator \u2666 missing from transcript")
    elif not has_turn_separator:
        print("TUI_TEST_FAIL turn separator \u2500 missing from transcript")
    elif exit_code not in (0, None):
        print(f"TUI_TEST_FAIL sned exited with {exit_code}")
    else:
        print("TUI_TEST_PASS turn indicators (\u2666, \u2500) appeared in transcript")
finally:
    shutil.rmtree(tmp, ignore_errors=True)
PY
}

test_tui_approval_scroll() {
    if ! command -v python3 >/dev/null 2>&1; then
        echo "TUI_TEST_FAIL python3 is required for pty smoke test"
        return 0
    fi

    SNED_BIN="$SNED_BIN" REPO_ROOT="$REPO_ROOT" VERBOSE="$VERBOSE" python3 - <<'PY'
import os
import pty
import re
import select
import shutil
import signal
import tempfile
import time

repo = os.environ["REPO_ROOT"]
sned_bin = os.environ["SNED_BIN"]
verbose = os.environ.get("VERBOSE") == "1"
tmp = tempfile.mkdtemp(prefix="sned-approval-scroll.")
env = os.environ.copy()
env.update({
    "SNED_NO_ALTERNATE_SCREEN": "1",
    "SNED_DIR": tmp,
    "SNED_DATA_DIR": os.path.join(tmp, "data"),
    "SNED_MOCK_APPROVAL_SCROLL": "1",
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
sent_user_prompt = False
sent_scroll = False
sent_approve = False
sent_exit = False
prompt_visible = False
exit_code = None
deadline = time.time() + 18

ansi_re = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")

def visible_tail(text: str, rows: int = 24) -> str:
    clean = ansi_re.sub("", text).replace("\r", "\n")
    lines = [line for line in clean.split("\n") if line.strip()]
    return "\n".join(lines[-rows:])

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
            text = buf.decode("utf-8", "replace")
            if "type a prompt" in text and not sent_user_prompt:
                os.write(fd, b"trigger approval scroll\r")
                sent_user_prompt = True
            if "approval scroll line 15" in text and not sent_scroll:
                os.write(fd, b"\x1b[5~\x1b[5~\x1b[5~")
                sent_scroll = True
            tail = visible_tail(text)
            if "Execute this tool?" in tail:
                prompt_visible = True
            if "Execute this tool?" in text and not sent_approve:
                os.write(fd, b"y\r")
                sent_approve = True
            if sent_approve and "Task Completed" in text and not sent_exit:
                time.sleep(0.25)
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

    if not sent_user_prompt:
        print("TUI_TEST_FAIL initial user prompt was not sent")
    elif not sent_scroll:
        print("TUI_TEST_FAIL PageUp was not sent")
    elif "Execute this tool?" not in text:
        print("TUI_TEST_FAIL approval prompt did not appear after scrolling")
    elif not prompt_visible:
        print("TUI_TEST_FAIL approval prompt appeared in transcript but not in the visible viewport")
    elif not sent_approve:
        print("TUI_TEST_FAIL approval prompt was not acknowledged")
    elif "attempt_completion" not in visible_tail(text):
        print("TUI_TEST_FAIL completion box did not appear in visible viewport after approval")
    elif exit_code not in (0, None):
        print(f"TUI_TEST_FAIL sned exited with {exit_code}")
    else:
        print("TUI_TEST_PASS approval prompt stayed visible after scrolling")
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

test_ctrlc_quit_empty() {
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
sent_first = False
sent_second = False
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
            # Wait for banner before sending first Ctrl+C
            if b"type a prompt" in buf and not sent_first:
                time.sleep(0.2)
                os.write(fd, b"\x03")  # First Ctrl+C
                sent_first = True
                print("First Ctrl+C sent")
            elif sent_first and not sent_second:
                # After first Ctrl+C, wait for ~0.3s and send second
                time.sleep(0.3)
                os.write(fd, b"\x03")  # Second Ctrl+C (within 2s window)
                sent_second = True
                print("Second Ctrl+C sent")

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
    elif not sent_first:
        print("TUI_TEST_FAIL first Ctrl+C was not sent")
    elif not sent_second:
        print("TUI_TEST_FAIL second Ctrl+C was not sent")
    elif exit_code not in (0, None):
        print(f"TUI_TEST_FAIL sned exited with {exit_code}")
    else:
        print("TUI_TEST_PASS double Ctrl+C quits from idle")
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

    # Allow tracing noise on stderr — the pty merges both streams.
    # The real check: no TUI-specific markers (banner, ratatui frame) should appear.
    has_tui_markers = any(m in text for m in ["type a prompt", "sned ", "Input"])
    if has_tui_markers:
        print("TUI_TEST_FAIL --json no-prompt started the TUI")
    elif exit_code not in (0, None):
        print(f"TUI_TEST_FAIL --json no-prompt exited with {exit_code}")
    else:
        print("TUI_TEST_PASS --json no-prompt did not start TUI")
finally:
    shutil.rmtree(tmp, ignore_errors=True)
PY
}

test_tui_history_navigation() {
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
tmp = tempfile.mkdtemp(prefix="sned-history-nav.")
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
sent_first = False
sent_second = False
sent_up = False
sent_exit = False
first_responded = False
second_responded = False
exit_code = None
deadline = time.time() + 15

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
            text = buf.decode("utf-8", "replace")
            if "type a prompt" in text and not sent_first:
                os.write(fd, b"first command\r")
                sent_first = True
            # Wait for the first command to get a mock response before sending the second
            if sent_first and "Mock provider" in text and not sent_second:
                time.sleep(0.3)
                os.write(fd, b"second command\r")
                sent_second = True
            # Wait for the second command to get a mock response before pressing Up
            if sent_second and "Mock provider" in text and not first_responded:
                first_responded = True
            if sent_second and first_responded and not sent_up:
                # Count mock responses to ensure second command was processed
                response_count = text.count("Mock provider")
                if response_count >= 2 and not sent_up:
                    time.sleep(0.3)
                    # Up arrow: \x1b[A
                    os.write(fd, b"\x1b[A")
                    sent_up = True
            if sent_up and not sent_exit:
                time.sleep(0.5)
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

    # After pressing Up, the input field should show "first command" (previous history entry)
    # The textarea renders input text, so it will appear in the pty output
    if not sent_first:
        print("TUI_TEST_FAIL first prompt was not sent")
    elif not sent_second:
        print("TUI_TEST_FAIL second prompt was not sent")
    elif not sent_up:
        print("TUI_TEST_FAIL Up arrow was not sent")
    elif "first command" not in text:
        print("TUI_TEST_FAIL previous prompt 'first command' not found after Up arrow")
    elif exit_code not in (0, None):
        print(f"TUI_TEST_FAIL sned exited with {exit_code}")
    else:
        print("TUI_TEST_PASS history navigation via Up arrow worked")
finally:
    shutil.rmtree(tmp, ignore_errors=True)
PY
}

test_tui_slash_commands() {
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
tmp = tempfile.mkdtemp(prefix="sned-slash-cmd.")
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
sent_help = False
sent_exit = False
exit_code = None
deadline = time.time() + 10

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
            text = buf.decode("utf-8", "replace")
            if "type a prompt" in text and not sent_help:
                os.write(fd, b"/help\r")
                sent_help = True
            if sent_help and "Sned Commands" in text and not sent_exit:
                time.sleep(0.25)
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

    # The /help command renders help text containing keyboard shortcuts.
    # Check for "Ctrl+C" which appears in the keyboard shortcuts section of the help text.
    if not sent_help:
        print("TUI_TEST_FAIL /help was not sent")
    elif "Ctrl+C" not in text:
        print("TUI_TEST_FAIL help text with keyboard shortcuts not found in output")
    elif exit_code not in (0, None):
        print(f"TUI_TEST_FAIL sned exited with {exit_code}")
    else:
        print("TUI_TEST_PASS /help rendered help text in output")
finally:
    shutil.rmtree(tmp, ignore_errors=True)
PY
}

test_tui_auto_scroll() {
    if ! command -v python3 >/dev/null 2>&1; then
        echo "TUI_TEST_FAIL python3 is required for pty smoke test"
        return 0
    fi

    SNED_BIN="$SNED_BIN" REPO_ROOT="$REPO_ROOT" VERBOSE="$VERBOSE" python3 - <<'PY'
import os
import pty
import re
import select
import shutil
import signal
import tempfile
import time

repo = os.environ["REPO_ROOT"]
sned_bin = os.environ["SNED_BIN"]
verbose = os.environ.get("VERBOSE") == "1"
tmp = tempfile.mkdtemp(prefix="sned-auto-scroll.")
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
prompt_count = 0
banner_seen = False
pending_prompt = None
next_prompt_ready_at = 0.0
next_prompt = 1
sent_scroll = False
sent_exit = False
exit_code = None
deadline = time.time() + 30

ansi_re = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")

def visible_tail(text, rows=24):
    """Get the last N non-empty lines (the visible viewport)."""
    clean = ansi_re.sub("", text).replace("\r", "\n")
    lines = [line for line in clean.split("\n") if line.strip()]
    return "\n".join(lines[-rows:])

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
            text = buf.decode("utf-8", "replace")
        tail = text.replace("\r", "\n")
        if "type a prompt" in text and not banner_seen:
            banner_seen = True

        # Drive the prompt sequence from a small state machine outside the
        # read branch so the next prompt can be sent even when the TUI is
        # momentarily quiet.
        if pending_prompt is None and banner_seen and next_prompt <= 5:
            if time.time() >= next_prompt_ready_at:
                os.write(fd, f"prompt {next_prompt}\r".encode())
                prompt_count += 1
                pending_prompt = next_prompt

        if pending_prompt is not None and f"prompt {pending_prompt}" in tail:
            pending_prompt = None
            next_prompt += 1
            next_prompt_ready_at = time.time() + 0.5

        # After all prompts are processed, scroll up to enter Manual mode,
        # then send a 6th prompt to verify auto-scroll resets the viewport.
        if prompt_count >= 5 and "Mock provider" in text and not sent_scroll:
            os.write(fd, b"\x1b[5~\x1b[5~\x1b[5~")
            sent_scroll = True

        if sent_scroll and not sent_exit:
            # Send a final prompt after scrolling — this triggers new output
            # which should force the viewport back to bottom.
            os.write(fd, b"final prompt\r")
            time.sleep(1.0)
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

    # Check the VISIBLE VIEWPORT (last 24 lines), not the full buffer.
    # If auto-scroll works, the latest output must be in the visible tail.
    viewport = visible_tail(text)
    has_latest_in_viewport = "final prompt" in viewport
    has_mock_in_viewport = "Mock provider" in viewport

    if prompt_count < 5:
        print(f"TUI_TEST_FAIL only sent {prompt_count} prompts, expected 5")
    elif not sent_scroll:
        print("TUI_TEST_FAIL PageUp scroll not sent")
    elif not has_latest_in_viewport:
        print("TUI_TEST_FAIL latest prompt not in visible viewport — auto-scroll failed")
    elif not has_mock_in_viewport:
        print("TUI_TEST_FAIL mock response not in visible viewport — auto-scroll failed")
    elif exit_code not in (0, None):
        print(f"TUI_TEST_FAIL sned exited with {exit_code}")
    else:
        print("TUI_TEST_PASS auto-scroll kept viewport at bottom for new output")
finally:
    shutil.rmtree(tmp, ignore_errors=True)
PY
}

test_tui_model_switch() {
    if ! command -v python3 >/dev/null 2>&1; then
        echo "TUI_TEST_FAIL python3 is required for pty smoke test"
        return 0
    fi

    SNED_BIN="$SNED_BIN" REPO_ROOT="$REPO_ROOT" VERBOSE="$VERBOSE" python3 - <<'PY'
import os
import pty
import re
import select
import shutil
import signal
import tempfile
import time

repo = os.environ["REPO_ROOT"]
sned_bin = os.environ["SNED_BIN"]
verbose = os.environ.get("VERBOSE") == "1"
tmp = tempfile.mkdtemp(prefix="sned-model-switch.")
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
sent_model = False
sent_exit = False
exit_code = None
deadline = time.time() + 10

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
            text = buf.decode("utf-8", "replace")
            if "type a prompt" in text and not sent_model:
                os.write(fd, b"/model anthropic/claude-sonnet-4\r")
                sent_model = True
            if sent_model and "Model switched to" in text and not sent_exit:
                time.sleep(0.25)
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
    clean = re.sub(r"\x1b\[[0-9;?]*[ -/]*[@-~]", " ", text)
    if verbose:
        print(text)

    if not sent_model:
        print("TUI_TEST_FAIL /model command was not sent")
    elif "Model switched to anthropic/claude-sonnet-4" not in clean:
        print("TUI_TEST_FAIL 'Model switched to' message not found in output")
    elif exit_code not in (0, None):
        print(f"TUI_TEST_FAIL sned exited with {exit_code}")
    else:
        print("TUI_TEST_PASS /model command rendered switch confirmation")
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
        tui-user-echo) result="$(test_tui_user_echo 2>&1)" ;;
        tui-turn-indicators) result="$(test_tui_turn_indicators 2>&1)" ;;
        tui-approval-scroll) result="$(test_tui_approval_scroll 2>&1)" ;;
        tui-history-navigation) result="$(test_tui_history_navigation 2>&1)" ;;
        tui-slash-commands) result="$(test_tui_slash_commands 2>&1)" ;;
        tui-auto-scroll) result="$(test_tui_auto_scroll 2>&1)" ;;
        tui-model-switch) result="$(test_tui_model_switch 2>&1)" ;;
        help) result="$(test_help 2>&1)" ;;
        version) result="$(test_version 2>&1)" ;;
        invalid-flag) result="$(test_invalid_flag 2>&1)" ;;
        yolo-help) result="$(test_yolo_help 2>&1)" ;;
        json-no-prompt) result="$(test_json_no_prompt 2>&1)" ;;
        ctrlc-quit-empty) result="$(test_ctrlc_quit_empty 2>&1)" ;;
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
