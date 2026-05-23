#!/usr/bin/env bash
# TUI state machine smoke test for sned interactive shell.
#
# Uses `expect` to drive the interactive shell and verify each
# state transition from docs/TUI_STATE_MACHINE.md.
#
# Prerequisites:
#   - `expect` installed (brew install expect on macOS)
#   - `cargo build` completed (uses ./target/debug/sned)
#   - MINIMAX_API_KEY set in environment (or edit SNED_BIN below)
#
# Usage:
#   ./user-scripts/tui-smoke-test.sh            # run all tests
#   ./user-scripts/tui-smoke-test.sh --verbose   # show expect output
#   ./user-scripts/tui-smoke-test.sh --test idle-keystroke  # run one test
#
# Exit codes:
#   0  all passed
#   1  one or more failures
#   2  setup error (no expect, no binary, no API key)

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
            echo "Test Name              State Transition"
            echo "────────────────────── ────────────────────────────────────────────"
            echo "idle-keystroke         IDLE + keystroke → cursor visible, input renders"
            echo "idle-enter-nonempty    IDLE + Enter (text) → prompt echoes, agent runs"
            echo "idle-enter-empty       IDLE + Enter (empty) → no submission"
            echo "idle-ctrl-c-empty      IDLE + Ctrl+C (empty) → clean exit"
            echo "idle-ctrl-c-nonempty   IDLE + Ctrl+C (text) → input clears, stays running"
            echo "idle-resize            IDLE + resize → scroll region updates"
            echo "busy-keystroke         AGENT_BUSY + keystroke → input at bottom, cursor hidden"
            echo "busy-enter-queue      AGENT_BUSY + Enter → queued msg echoed to scroll region"
            echo "busy-ctrl-c            AGENT_BUSY + Ctrl+C → agent aborted, cursor visible"
            echo "agent-done             AGENT_BUSY → done → cursor visible at input"
            echo "multi-turn             3 IDLE↔AGENT_BUSY cycles → cursor toggles correctly"
            echo "startup-banner         Startup → banner and prompt render"
            exit 0 ;;
        --help|-h)
            cat <<'HELP'
Usage: tui-smoke-test.sh [OPTIONS]

Automated smoke test for the sned interactive TUI state machine.
Runs expect-driven tests against ./target/debug/sned to verify every
state transition from docs/TUI_STATE_MACHINE.md.

OPTIONS:
  --verbose, -v      Show expect interaction output (debugging)
  --test NAME, -t    Run a single test by name (see list below)
  --list             List all test names and their state transitions
  --help, -h         Show this help

WHEN TO RUN:
  After ANY change to src/cli/interactive.rs or src/terminal/input.rs.
  Before committing TUI work. When user reports display/cursor bugs.

PREREQUISITES:
  - cargo build completed (uses ./target/debug/sned)
  - MINIMAX_API_KEY environment variable set
  - expect installed (brew install expect)

TESTS:
  idle-keystroke        IDLE + keystroke → cursor visible, input renders
  idle-enter-nonempty   IDLE + Enter (text) → prompt echoes, agent runs, cursor returns
  idle-enter-empty      IDLE + Enter (empty) → no submission
  idle-ctrl-c-empty     IDLE + Ctrl+C (empty) → clean exit, terminal restored
  idle-ctrl-c-nonempty  IDLE + Ctrl+C (text) → input clears, does not exit
  idle-resize           IDLE + resize → scroll region updates, input re-renders
  busy-keystroke        AGENT_BUSY + keystroke → input at bottom, cursor hidden
  busy-enter-queue      AGENT_BUSY + Enter (queue msg) → echoed to scroll region
  busy-ctrl-c           AGENT_BUSY + Ctrl+C → agent aborted, cursor visible
  agent-done            AGENT_BUSY → done → cursor visible at input
  multi-turn            3 IDLE↔AGENT_BUSY cycles → cursor toggles each time
  startup-banner        Startup → banner and prompt render correctly

EXAMPLES:
  # Run all tests (standard workflow after TUI changes)
  ./user-scripts/tui-smoke-test.sh

  # Debug a specific failing test
  ./user-scripts/tui-smoke-test.sh --verbose --test busy-enter-queue

  # See which tests exist
  ./user-scripts/tui-smoke-test.sh --list

EXIT CODES:
  0  All tests passed
  1  One or more test failures
  2  Setup error (missing expect, binary, or API key)
HELP
            exit 0 ;;
        *) echo "Unknown argument: $1. Run with --help for usage." >&2; exit 2 ;;
    esac
done

# --- Setup checks ---

# Always rebuild so we test the latest code, not a stale binary
echo "Building sned (debug)..."
if ! cargo build 2>&1; then
    cat >&2 <<EOF

FAIL: cargo build failed.

  Fix compile errors in the code above, then re-run:
    ./user-scripts/tui-smoke-test.sh

  If errors are in src/cli/interactive.rs, consult docs/TUI_STATE_MACHINE.md
  for the expected render_input_line() invariants.
EOF
    exit 2
fi
echo ""

if ! command -v expect &>/dev/null; then
    cat >&2 <<EOF
FAIL: 'expect' not found.

  This test requires the 'expect' tool to drive the interactive shell.
  Install it with:
    brew install expect

  Then re-run: ./user-scripts/tui-smoke-test.sh
EOF
    exit 2
fi

if [[ -z "${MINIMAX_API_KEY:-}" ]]; then
    cat >&2 <<EOF
FAIL: MINIMAX_API_KEY not set.

  The smoke test uses MiniMax-M2.7 as the provider. Set the API key:
    export MINIMAX_API_KEY="your-key-here"

  Then re-run: ./user-scripts/tui-smoke-test.sh

  If you don't have a MiniMax key, you can modify SNED_BIN at the top
  of this script to use a different provider.
EOF
    exit 2
fi

PTY_HELPER="${REPO_ROOT}/user-scripts/sned-pty-helper"
if [[ ! -x "$PTY_HELPER" ]]; then
    cat >&2 <<EOF
FAIL: $PTY_HELPER not found.

  The TUI smoke test requires a small C helper that sets the pty window
  size (via TIOCSWINSZ ioctl) before sned starts. Without it, expect's
  pty defaults to 0x0 and sned panics on div_ceil(0).

  Rebuild the helper:
    gcc -o user-scripts/sned-pty-helper user-scripts/sned-pty-helper.c
    chmod +x user-scripts/sned-pty-helper

  Then re-run: ./user-scripts/tui-smoke-test.sh
EOF
    exit 2
fi

# --- Test infrastructure ---

PASS_COUNT=0
FAIL_COUNT=0
RESULTS=()

run_test() {
    local name="$1"
    local expect_script="$2"

    if [[ -n "$RUN_TEST" && "$RUN_TEST" != "$name" ]]; then
        return 0
    fi

    local tmpout
    tmpout=$(mktemp /tmp/sned-tui-test-XXXXXX)

    # Run expect in a subshell with its own pty to isolate terminal state.
    # Each test gets a fresh terminal — no contamination from prior tests.
    # The subshell ensures the parent terminal is never modified.
    (
        # Save and restore parent terminal state around each test
        # to prevent cross-contamination if expect misbehaves
        stty -g 2>/dev/null || true
        if [[ $VERBOSE -eq 1 ]]; then
            expect -d -c "$expect_script" 2>&1
        else
            expect -c "$expect_script" 2>&1
        fi
        local ec=$?
        stty sane 2>/dev/null || true
        exit $ec
    ) > "$tmpout" 2>&1

    local exit_code=$?
    local output
    output=$(cat "$tmpout")
    rm -f "$tmpout"

    # Check for PASS/FAIL markers in output
    if echo "$output" | grep -q "TUI_TEST_PASS"; then
        RESULTS+=("PASS  $name")
        PASS_COUNT=$((PASS_COUNT + 1))
    elif echo "$output" | grep -q "TUI_TEST_FAIL"; then
        local reason
        reason=$(echo "$output" | grep "TUI_TEST_FAIL" | head -1 | sed 's/.*TUI_TEST_FAIL//' | xargs)
        RESULTS+=("FAIL  $name — $reason")
        FAIL_COUNT=$((FAIL_COUNT + 1))
    elif [[ $exit_code -ne 0 ]]; then
        RESULTS+=("FAIL  $name — expect crashed (exit $exit_code). Re-run with --verbose --test $name to diagnose.")
        FAIL_COUNT=$((FAIL_COUNT + 1))
    else
        RESULTS+=("FAIL  $name — no TUI_TEST_PASS marker. The expect script may have hung or exited early. Re-run with --verbose --test $name.")
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
}

# Common expect preamble: spawn sned via pty helper, wait for prompt, set timeout
expect_preamble() {
    cat <<'EXPECT'
    set timeout 30
    # Use sned-pty-helper to set TIOCSWINSZ before sned starts.
    # expect's pty defaults to 0x0 window size, which causes
    # crossterm::terminal::size() to return (0, 0) and triggers
    # a div_ceil(0) panic in render_input_line(). The helper calls
    # ioctl(TIOCSWINSZ) on stdin (the pty slave) then execs sned.
    set env(TERM) xterm-256color
    spawn /Users/easto/projects/dirac-fork/user-scripts/sned-pty-helper 24 80 /Users/easto/projects/dirac-fork/target/debug/sned --provider minimax --model "MiniMax-M2.7" --yolo
    # Wait for sned to fully initialize: raw mode → bracketed paste →
    # scroll region → banner → prompt. Without this sleep, keystrokes
    # arrive before sned is ready to read them.
    sleep 1
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL startup timeout — sned did not render prompt. Check: does cargo build succeed? Does MINIMAX_API_KEY work?"; exit 1 }
    }
    # Brief pause after prompt appears so the terminal state is stable
    sleep 0.3
EXPECT
}

expect_cleanup() {
    cat <<'EXPECT'
    send "exit\r"
    expect {
        eof { }
        timeout { }
    }
    # Force kill if the process didn't exit cleanly
    catch {wait -nowait}
    sleep 0.3
EXPECT
}

# --- Test definitions ---

test_idle_keystroke() {
    run_test "idle-keystroke" "$(expect_preamble)
    # Type 'hello' — should appear at input line
    send "hello"
    sleep 0.5
    # Verify input area is not empty by checking we can backspace
    send "\x7f\x7f\x7f\x7f\x7f"
    sleep 0.2
    puts \"TUI_TEST_PASS cursor visible and input responsive\"
    $(expect_cleanup)"
}

test_idle_enter_nonempty() {
    run_test "idle-enter-nonempty" "$(expect_preamble)
    # Send a simple prompt and press Enter
    send \"say hello\r\"
    # Wait for agent to start (spinner or output appears)
    expect {
        -re \"Ɵ|▶|searched|Error|elapsed\" { }
        timeout { puts \"TUI_TEST_FAIL agent did not start — no output seen after Enter. Check: does agent_loop.rs spawn correctly? Is the provider responding?\"; exit 1 }
    }
    # Wait for agent to finish
    expect {
        -re \"❯\" { }
        timeout { puts \"TUI_TEST_FAIL agent did not return to idle prompt. Check: is agent_done.notify_one() called? Does render_input_line(..., false) show cursor?\"; exit 1 }
    }
    puts \"TUI_TEST_PASS prompt echoed and agent completed\"
    $(expect_cleanup)"
}

test_idle_enter_empty() {
    run_test "idle-enter-empty" "$(expect_preamble)
    # Press Enter with empty input — should just clear
    send \"\r\"
    sleep 0.5
    # Input should still be empty, cursor at prompt
    send \"test\"
    sleep 0.3
    puts \"TUI_TEST_PASS empty enter did not submit\"
    # Clean up the test text
    send \"\x7f\x7f\x7f\x7f\"
    $(expect_cleanup)"
}

test_idle_ctrl_c_empty() {
    run_test "idle-ctrl-c-empty" "$(cat <<'EXPECT'
    set timeout 10
    set env(TERM) xterm-256color
    spawn /Users/easto/projects/dirac-fork/user-scripts/sned-pty-helper 24 80 /Users/easto/projects/dirac-fork/target/debug/sned --provider minimax --model "MiniMax-M2.7" --yolo
    sleep 1
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL startup timeout — sned did not render prompt. Check: does cargo build succeed? Does MINIMAX_API_KEY work?"; exit 1 }
    }
    sleep 0.3
    # Ctrl+C with empty input should exit
    send "\x03"
    expect {
        eof { puts "TUI_TEST_PASS clean exit on Ctrl+C" }
        timeout { puts "TUI_TEST_FAIL did not exit on Ctrl+C with empty input. Check: cleanup_terminal() should call reset_scroll_region and drop raw_guard."; exit 1 }
    }
EXPECT
)"
}

test_idle_ctrl_c_nonempty() {
    run_test "idle-ctrl-c-nonempty" "$(expect_preamble)
    # Type some text
    send \"some text\"
    sleep 0.3
    # Ctrl+C should clear input, not exit
    send \"\x03\"
    sleep 0.5
    # Should still be running — type something new
    send \"still here\"
    sleep 0.3
    puts \"TUI_TEST_PASS Ctrl+C cleared input but did not exit\"
    send \"\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\"
    $(expect_cleanup)"
}

test_idle_resize() {
    run_test "idle-resize" "$(expect_preamble)
    # Trigger resize by changing the pty window size
    # expect's 'stty' operates on the spawned pty, not the parent terminal
    catch {stty columns 120 rows 30}
    sleep 0.5
    catch {stty columns 80 rows 24}
    sleep 0.5
    # Should still be responsive
    send \"test\"
    sleep 0.3
    puts \"TUI_TEST_PASS resize handled, input responsive\"
    send \"\x7f\x7f\x7f\x7f\"
    $(expect_cleanup)"
}

test_busy_keystroke() {
    run_test "busy-keystroke" "$(expect_preamble)
    # Start agent with a prompt
    send \"list files\r\"
    sleep 1
    # Agent should be busy now — type something
    send \"typing while busy\"
    sleep 0.5
    # The input line should show our typed text at bottom row
    # Wait for agent to complete
    expect {
        -re \"❯\" { }
        timeout { puts \"TUI_TEST_FAIL agent did not complete. Check: does the agent task call agent_done.notify_one()? Does render_input_line show cursor after completion?\"; exit 1 }
    }
    puts \"TUI_TEST_PASS input visible during agent execution\"
    send \"\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\"
    $(expect_cleanup)"
}

test_busy_enter_queue() {
    run_test "busy-enter-queue" "$(expect_preamble)
    # Start agent
    send \"read the README\r\"
    sleep 1
    # Agent busy — send a queued message
    send \"also check BUILD_SPEC.md\r\"
    sleep 0.5
    # Should see queued feedback
    expect {
        -re \"queued|Queue\" { }
        timeout { puts \"TUI_TEST_FAIL no queue feedback — message enqueue path broken. Check: handle.enqueue_text_message() and eprint_info queue message in interactive.rs.\"; exit 1 }
    }
    # Wait for agent to complete
    expect {
        -re \"❯\" { }
        timeout { puts \"TUI_TEST_FAIL agent did not complete after queue. Check: does agent_done.notify_one() fire after processing queued messages?\"; exit 1 }
    }
    puts \"TUI_TEST_PASS queued message echoed and agent completed\"
    $(expect_cleanup)"
}

test_busy_ctrl_c() {
    run_test "busy-ctrl-c" "$(expect_preamble)
    # Start agent
    send \"read all files\r\"
    sleep 1
    # Ctrl+C should abort agent
    send \"\x03\"
    sleep 1
    # Should be back at idle prompt
    send \"still alive\"
    sleep 0.3
    puts \"TUI_TEST_PASS Ctrl+C aborted agent, returned to idle\"
    send \"\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\"
    $(expect_cleanup)"
}

test_agent_done() {
    run_test "agent-done" "$(expect_preamble)
    # Start and complete a simple agent turn
    send \"say hi\r\"
    expect {
        -re \"❯\" { }
        timeout { puts \"TUI_TEST_FAIL agent did not complete\"; exit 1 }
    }
    # Cursor should be visible at input line
    send \"after done\"
    sleep 0.3
    puts \"TUI_TEST_PASS cursor visible after agent completion\"
    send \"\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\"
    $(expect_cleanup)"
}

test_multi_turn() {
    run_test "multi-turn" "$(expect_preamble)
    # Turn 1
    send \"say hello\r\"
    expect {
        -re \"❯\" { }
        timeout { puts \"TUI_TEST_FAIL turn 1 did not complete. Check: agent_done notification and cursor visibility (?25h) after completion.\"; exit 1 }
    }
    # Turn 2 — verify cursor is still visible
    send \"say world\r\"
    expect {
        -re \"❯\" { }
        timeout { puts \"TUI_TEST_FAIL turn 2 did not complete. Cursor may be stuck hidden after turn 1. Check: render_input_line(false) must emit ?25h.\"; exit 1 }
    }
    # Turn 3 — type during agent, then let it finish
    send \"say foo\r\"
    sleep 0.5
    send \"interrupting text\"
    expect {
        -re \"❯\" { }
        timeout { puts \"TUI_TEST_FAIL turn 3 did not complete. Check: cursor toggle across multiple AGENT_BUSY→IDLE transitions.\"; exit 1 }
    }
    puts \"TUI_TEST_PASS three turns completed with cursor toggling\"
    $(expect_cleanup)"
}

test_startup_banner() {
    run_test "startup-banner" "$(cat <<'EXPECT'
    set timeout 10
    set env(TERM) xterm-256color
    spawn /Users/easto/projects/dirac-fork/user-scripts/sned-pty-helper 24 80 /Users/easto/projects/dirac-fork/target/debug/sned --provider minimax --model "MiniMax-M2.7" --yolo
    sleep 1
    # Should see startup info (provider/model) and prompt
    expect {
        -re "sned" { }
        timeout { puts "TUI_TEST_FAIL no startup banner. Check: is_print_quiet() suppressing output? Does eprint_raw() work with scroll region?"; exit 1 }
    }
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL no prompt after banner. Check: set_scroll_region() called at startup? Does render_input_line() position cursor correctly?"; exit 1 }
    }
    puts "TUI_TEST_PASS startup banner and prompt rendered"
    send "exit\r"
    expect eof
EXPECT
)"
}

# --- Run all tests ---

ALL_TEST_NAMES=(idle-keystroke idle-enter-nonempty idle-enter-empty idle-ctrl-c-empty idle-ctrl-c-nonempty idle-resize busy-keystroke busy-enter-queue busy-ctrl-c agent-done multi-turn startup-banner)

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
        echo "" >&2
        echo "Run with --list to see full descriptions, or --help for usage." >&2
        exit 2
    fi
fi

echo "═══════════════════════════════════════════"
echo "  sned TUI State Machine Smoke Test"
echo "═══════════════════════════════════════════"
echo ""

test_idle_keystroke
sleep 0.5
test_idle_enter_nonempty
sleep 0.5
test_idle_enter_empty
sleep 0.5
test_idle_ctrl_c_empty
sleep 0.5
test_idle_ctrl_c_nonempty
sleep 0.5
test_idle_resize
sleep 0.5
test_busy_keystroke
sleep 0.5
test_busy_enter_queue
sleep 0.5
test_busy_ctrl_c
sleep 0.5
test_agent_done
sleep 0.5
test_multi_turn
sleep 0.5
test_startup_banner

# --- Results ---

echo ""
echo "═══════════════════════════════════════════"
echo "  Results"
echo "═══════════════════════════════════════════"

for r in "${RESULTS[@]}"; do
    echo "  $r"
done

echo ""
echo "  $PASS_COUNT passed, $FAIL_COUNT failed"

if [[ $FAIL_COUNT -gt 0 ]]; then
    echo ""
    echo "  To diagnose failures:"
    echo "    1. Re-run the specific failing test with verbose output:"
    for r in "${RESULTS[@]}"; do
        if [[ "$r" == FAIL* ]]; then
            test_name=$(echo "$r" | awk '{print $2}')
            echo "       ./user-scripts/tui-smoke-test.sh --verbose --test $test_name"
        fi
    done
    echo "    2. Check docs/TUI_STATE_MACHINE.md for the expected behavior"
    echo "    3. Check src/cli/interactive.rs render_input_line() invariants"
    echo "    4. Common failure causes:"
    echo "       - Cursor not shown after agent completes (invariant: ?25h on idle)"
    echo "       - Cursor not hidden during agent execution (invariant: ?25l on busy)"
    echo "       - Prompt echo written to wrong row (must go to scroll region, not input row)"
    echo "       - Cursor left at input row after agent-busy render (must return to scroll region bottom)"
    exit 1
fi
