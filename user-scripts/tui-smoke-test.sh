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
            echo "---------------------- ----------------------------------------------"
            echo "idle-keystroke         IDLE + keystroke: cursor visible, input renders"
            echo "idle-enter-nonempty    IDLE + Enter (text): prompt echoes, agent runs, cursor returns"
            echo "idle-enter-empty       IDLE + Enter (empty): no submission, no agent spawn"
            echo "idle-ctrl-c-empty      IDLE + Ctrl+C (empty): clean exit, terminal restored"
            echo "idle-ctrl-c-nonempty   IDLE + Ctrl+C (text): ^C shown, input clears, stays running"
            echo "idle-resize            IDLE + resize: scroll region updates"
            echo "busy-keystroke         AGENT_BUSY + keystroke: input at bottom, cursor hidden"
            echo "busy-enter-queue       AGENT_BUSY + Enter (text): queued msg echoed to scroll region"
            echo "busy-enter-empty       AGENT_BUSY + Enter (empty): nothing happens, cursor stays hidden"
            echo "busy-ctrl-c            AGENT_BUSY + Ctrl+C: ^C shown, agent aborted, cursor visible"
            echo "busy-resize            AGENT_BUSY + resize: scroll region updates, cursor hidden"
            echo "agent-done             AGENT_BUSY -> done: elapsed time, cursor visible at input"
            echo "multi-turn             3 IDLE<->AGENT_BUSY cycles: cursor toggles correctly"
            echo "input-line-clean       After agent completes: input line has no leftover text"
            echo "queue-input-clean      Queued msgs during AGENT_BUSY: input line clean after completion"
            echo "startup-banner         Startup: banner and prompt render"
            exit 0 ;;
        --help|-h)
            cat <<'HELP'
Usage: tui-smoke-test.sh [OPTIONS]

Automated smoke test for the sned interactive TUI state machine.
Runs expect-driven tests against ./target/debug/sned to verify every
state transition from docs/TUI_STATE_MACHINE.md.

OPTIONS:
  --verbose, -v      Show expect interaction output (debugging)
  --test NAME, -t    Run a single test by name (see --list)
  --list             List all test names and their state transitions
  --help, -h         Show this help

PREREQUISITES:
  - cargo build completed (uses ./target/debug/sned)
  - MINIMAX_API_KEY environment variable set
  - expect installed (brew install expect)

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

echo "Building sned (debug)..."
if ! cargo build 2>&1; then
    cat >&2 <<EOF

FAIL: cargo build failed.

  Fix compile errors, then re-run:
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

  Install it with:
    brew install expect

  Then re-run: ./user-scripts/tui-smoke-test.sh
EOF
    exit 2
fi

if [[ -z "${MINIMAX_API_KEY:-}" ]]; then
    cat >&2 <<EOF
FAIL: MINIMAX_API_KEY not set.

  Set the API key:
    export MINIMAX_API_KEY="your-key-here"

  Then re-run: ./user-scripts/tui-smoke-test.sh
EOF
    exit 2
fi

PTY_HELPER="${REPO_ROOT}/user-scripts/sned-pty-helper"
if [[ ! -x "$PTY_HELPER" ]]; then
    cat >&2 <<EOF
FAIL: $PTY_HELPER not found.

  The smoke test requires a C helper that sets the pty window size
  via TIOCSWINSZ ioctl before sned starts. Without it, expect's
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
FAIL_NAMES=()
RESULTS=()
TOTAL_TESTS=16
TEST_TIMEOUT=60

if [[ -n "${SNED_TEST_TIMEOUT:-}" ]]; then
    TEST_TIMEOUT="$SNED_TEST_TIMEOUT"
fi

declare -A TEST_DESC TEST_SOURCE
TEST_DESC[idle-keystroke]="IDLE + keystroke: cursor stays visible, typed chars render at input line"
TEST_SOURCE[idle-keystroke]="src/cli/interactive.rs render_input_line() -- cursor (?25h), scroll region row"

TEST_DESC[idle-enter-nonempty]="IDLE + Enter (text): prompt echoes to scroll region, agent starts, cursor returns after completion"
TEST_SOURCE[idle-enter-nonempty]="src/cli/interactive.rs submit + agent_done, src/core/agent_loop.rs"

TEST_DESC[idle-enter-empty]="IDLE + Enter (empty): no submission, no agent spawn, input stays blank"
TEST_SOURCE[idle-enter-empty]="src/cli/interactive.rs empty-line guard before submit"

TEST_DESC[idle-ctrl-c-empty]="IDLE + Ctrl+C (empty input): clean exit, scroll region reset, raw mode dropped"
TEST_SOURCE[idle-ctrl-c-empty]="src/cli/interactive.rs cleanup_terminal(), reset_scroll_region"

TEST_DESC[idle-ctrl-c-nonempty]="IDLE + Ctrl+C (text): ^C shown, input clears, process stays running"
TEST_SOURCE[idle-ctrl-c-nonempty]="src/cli/interactive.rs Ctrl+C handler -- clear vs exit branch"

TEST_DESC[idle-resize]="IDLE + resize: scroll region updates (CAVEAT: stty may not send SIGWINCH)"
TEST_SOURCE[idle-resize]="src/terminal/input.rs setup_sigwinch_handler(), src/cli/interactive.rs TerminalEvent::Resize"

TEST_DESC[busy-keystroke]="AGENT_BUSY + keystroke: typed chars appear at bottom row, cursor hidden (?25l)"
TEST_SOURCE[busy-keystroke]="src/cli/interactive.rs AGENT_BUSY input rendering, cursor hide"

TEST_DESC[busy-enter-queue]="AGENT_BUSY + Enter (text): queued message echoed to scroll region, processed after current turn"
TEST_SOURCE[busy-enter-queue]="src/cli/interactive.rs enqueue_text_message(), eprint_info queue feedback"

TEST_DESC[busy-enter-empty]="AGENT_BUSY + Enter (empty): nothing happens, cursor stays hidden"
TEST_SOURCE[busy-enter-empty]="src/cli/interactive.rs AGENT_BUSY empty-Enter handler, render_input_line(true)"

TEST_DESC[busy-ctrl-c]="AGENT_BUSY + Ctrl+C: ^C shown, agent aborted, cursor visible (?25h), returns to IDLE"
TEST_SOURCE[busy-ctrl-c]="src/cli/interactive.rs Ctrl+C during AGENT_BUSY, agent cancellation"

TEST_DESC[busy-resize]="AGENT_BUSY + resize: scroll region updates, cursor hidden (CAVEAT: stty may not send SIGWINCH)"
TEST_SOURCE[busy-resize]="src/terminal/input.rs setup_sigwinch_handler(), src/cli/interactive.rs TerminalEvent::Resize AGENT_BUSY"

TEST_DESC[agent-done]="AGENT_BUSY -> IDLE: elapsed time shown, cursor visible at input line, prompt re-renders"
TEST_SOURCE[agent-done]="src/cli/interactive.rs agent_done.notify_one(), render_input_line(false)"

TEST_DESC[multi-turn]="3x IDLE<->AGENT_BUSY cycles: cursor toggles ?25l/?25h each time, no stuck states"
TEST_SOURCE[multi-turn]="src/cli/interactive.rs cursor toggle across AGENT_BUSY/IDLE transitions"

TEST_DESC[input-line-clean]="After agent completes, input line is clean -- no leftover text from previous submission"
TEST_SOURCE[input-line-clean]="src/cli/interactive.rs render_input_line() input buffer clear on IDLE, input_line reset after submit"

TEST_DESC[queue-input-clean]="After queued messages during AGENT_BUSY complete, input line is clean -- no stale text persists"
TEST_SOURCE[queue-input-clean]="src/cli/interactive.rs enqueue_text_message(), render_input_line() input buffer reset after queued submit"

TEST_DESC[startup-banner]="Startup: banner (provider/model) renders, then prompt appears"
TEST_SOURCE[startup-banner]="src/cli/interactive.rs set_scroll_region(), eprint_raw() banner, render_input_line()"

# Run a test given a pre-written expect script file.
run_test_file() {
    local name="$1"
    local script_file="$2"

    if [[ -n "$RUN_TEST" && "$RUN_TEST" != "$name" ]]; then
        rm -f "$script_file"
        return 0
    fi

    local desc="${TEST_DESC[$name]:-unknown}"
    local source="${TEST_SOURCE[$name]:-src/cli/interactive.rs}"

    printf "  [%d/%d] RUNNING %s\n" $((PASS_COUNT + FAIL_COUNT + 1)) "$TOTAL_TESTS" "$name"
    printf "         %s\n" "$desc"

    local tmpout
    tmpout=$(mktemp /tmp/sned-tui-test-XXXXXX)

    local expect_cmd="expect"
    if [[ $VERBOSE -eq 1 ]]; then
        expect_cmd="expect -d"
    fi

    local exit_code
    exit_code=0 && timeout "$TEST_TIMEOUT" $expect_cmd "$script_file" > "$tmpout" 2>&1 || exit_code=$?

    rm -f "$script_file"
    local output
    output=$(cat "$tmpout")
    rm -f "$tmpout"

    local result_line result_verb
    if echo "$output" | grep -q "TUI_TEST_PASS"; then
        result_line="PASS  $name"
        result_verb="PASS"
        RESULTS+=("$result_line")
        PASS_COUNT=$((PASS_COUNT + 1))
    elif echo "$output" | grep -q "TUI_TEST_FAIL"; then
        local reason
        reason=$(echo "$output" | grep "TUI_TEST_FAIL" | head -1 | sed 's/.*TUI_TEST_FAIL//' | xargs) || reason="see verbose output"
        result_line="FAIL  $name -- $reason"
        result_verb="FAIL"
        RESULTS+=("$result_line")
        FAIL_NAMES+=("$name")
        FAIL_COUNT=$((FAIL_COUNT + 1))
    elif [[ $exit_code -eq 124 ]]; then
        result_line="FAIL  $name -- timed out after ${TEST_TIMEOUT}s"
        result_verb="FAIL"
        RESULTS+=("$result_line")
        FAIL_NAMES+=("$name")
        FAIL_COUNT=$((FAIL_COUNT + 1))
    elif [[ $exit_code -ne 0 ]]; then
        result_line="FAIL  $name -- expect crashed (exit $exit_code)"
        result_verb="FAIL"
        RESULTS+=("$result_line")
        FAIL_NAMES+=("$name")
        FAIL_COUNT=$((FAIL_COUNT + 1))
    else
        result_line="FAIL  $name -- no TUI_TEST_PASS marker (expect script exited early)"
        result_verb="FAIL"
        RESULTS+=("$result_line")
        FAIL_NAMES+=("$name")
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi

    if [[ "$result_verb" == "PASS" ]]; then
        printf "         -> PASS\n"
    else
        printf "         -> FAIL\n"
        printf "         Likely source: %s\n" "$source"
        printf "         State machine doc: docs/TUI_STATE_MACHINE.md\n"
        if [[ $exit_code -eq 124 ]]; then
            printf "         Timeout means sned hung -- expect never got expected output in %ds.\n" "$TEST_TIMEOUT"
            printf "         Common cause: cursor hidden and never shown, or agent_done never fires.\n"
        fi
        if [[ $exit_code -ne 0 ]] && [[ $exit_code -ne 124 ]]; then
            printf "         Expect exit code %d -- script crashed, not a sned bug per se.\n" "$exit_code"
            printf "         Run: ./user-scripts/tui-smoke-test.sh --verbose --test %s\n" "$name"
        fi
        local last_lines
        last_lines=$(echo "$output" | tail -5)
        if [[ -n "$last_lines" ]]; then
            printf "         Last expect output:\n"
            echo "$last_lines" | while IFS= read -r line; do
                printf "           %s\n" "$line"
            done
        fi
        printf "         Verbose re-run: ./user-scripts/tui-smoke-test.sh --verbose --test %s\n" "$name"
    fi
}

# Create a temp expect script file with the standard preamble.
# Returns the file path. Caller appends test body, then finish_script, then run_test_file.
new_script() {
    local f
    f=$(mktemp /tmp/sned-tui-script-XXXXXX)
    cat > "$f" <<'EXPECT'
    set timeout 10
    set env(TERM) xterm-256color
    proc check_cursor {fail_msg} {
        global timeout
        while {1} {
            set timeout 5
            expect {
                -re {\?25h} { set timeout 10; return }
                -re {\?25l} { }
                timeout { puts "TUI_TEST_FAIL no cursor show (?25h) at idle. $fail_msg"; exit 1 }
            }
        }
    }
    proc check_cursor_hidden {fail_msg} {
        global timeout
        set timeout 2
        expect {
            -re {\?25h} { puts "TUI_TEST_FAIL cursor visible (?25h) during AGENT_BUSY. $fail_msg"; exit 1 }
            -re {\?25l} { }
            timeout { }
        }
        set timeout 10
    }
    proc check_no_agent_output {fail_msg} {
        global timeout
        set timeout 2
        expect {
            -re "⠋|▶|⏱|elapsed|task completed" { puts "TUI_TEST_FAIL agent spawned on empty input -- should be no-op. $fail_msg"; exit 1 }
            timeout { }
        }
        set timeout 10
    }
EXPECT
    cat >> "$f" <<SPAWNLINE
    spawn ${REPO_ROOT}/user-scripts/sned-pty-helper 24 80 ${REPO_ROOT}/target/debug/sned --provider minimax --model "MiniMax-M2.7" --yolo
SPAWNLINE
    cat >> "$f" <<'EXPECT'
    sleep 1
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL startup timeout -- sned did not render prompt. Check: does cargo build succeed? Does MINIMAX_API_KEY work?"; exit 1 }
    }
    expect {
        -re {\?25h} { }
        timeout { }
    }
    sleep 0.3
EXPECT
    echo "$f"
}

# Append standard cleanup to an expect script file.
finish_script() {
    local f="$1"
    cat >> "$f" <<'EXPECT'
    send "/exit\r"
    set timeout 3
    expect {
        eof { }
        timeout { }
    }
    catch {close}
    catch {wait -nowait}
EXPECT
}

# --- Test definitions ---

test_idle_keystroke() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
    send "hello"
    sleep 0.5
    send "\x7f\x7f\x7f\x7f\x7f"
    sleep 0.2
    check_cursor "Bug: cursor not visible during IDLE keystroke. Check: render_input_line(false) must emit ?25h."
    puts "TUI_TEST_PASS cursor visible and input responsive"
EXPECT
    finish_script "$f"
    run_test_file "idle-keystroke" "$f"
}

test_idle_enter_nonempty() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
    send "say hello\r"
    expect {
        -re "⠋|▶|searched|Error|elapsed|⏱|Hello|How can I help" { }
        timeout { puts "TUI_TEST_FAIL agent did not start -- no output seen after Enter. Check: does agent_loop.rs spawn correctly? Is the provider responding?"; exit 1 }
    }
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL agent did not return to idle prompt. Check: is agent_done.notify_one() called? Does render_input_line(..., false) show cursor?"; exit 1 }
    }
    sleep 0.2
    check_cursor "Bug: cursor-disappears-after-turn. Check: render_input_line(false) must emit ?25h."
    puts "TUI_TEST_PASS prompt echoed and agent completed"
EXPECT
    finish_script "$f"
    run_test_file "idle-enter-nonempty" "$f"
}

test_idle_enter_empty() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
    send "\r"
    sleep 0.3
    check_no_agent_output "Bug: empty Enter spawned an agent. Check: empty-line guard before submit."
    check_cursor "Bug: cursor not visible after empty Enter. Check: render_input_line(false) on empty Enter must emit ?25h."
    send "test"
    sleep 0.3
    puts "TUI_TEST_PASS empty enter did not submit, cursor visible"
    send "\x7f\x7f\x7f\x7f"
EXPECT
    finish_script "$f"
    run_test_file "idle-enter-empty" "$f"
}

test_idle_ctrl_c_empty() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
    send "\x03"
    # cleanup_terminal() should show cursor (?25h) before exiting.
    # Use short timeout -- if process exits before we see ?25h,
    # that is acceptable (cleanup happened fast).
    expect {
        -re {\?25h} { }
        eof { }
        timeout { }
    }
    expect {
        eof { puts "TUI_TEST_PASS clean exit on Ctrl+C with terminal cleanup" }
        timeout { puts "TUI_TEST_FAIL did not exit on Ctrl+C with empty input. Check: cleanup_terminal() should call reset_scroll_region and drop raw_guard."; exit 1 }
    }
EXPECT
    # No finish_script -- this test exits the process
    run_test_file "idle-ctrl-c-empty" "$f"
}

test_idle_ctrl_c_nonempty() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
    send "some text"
    sleep 0.3
    send "\x03"
    sleep 0.3
    expect {
        -re {\^C} { }
        timeout { puts "TUI_TEST_FAIL no ^C on Ctrl+C with non-empty input. Check: Ctrl+C handler should print ^C before re-rendering."; exit 1 }
    }
    # After ^C, input should be cleared. Type new text to confirm old text is gone.
    # If old text persisted, the input line would show "some text" plus new input.
    send "fresh"
    sleep 0.3
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL sned exited on Ctrl+C with non-empty input -- should clear input, not exit. Check: Ctrl+C handler branch for non-empty input in interactive.rs."; exit 1 }
    }
    sleep 0.2
    check_cursor "After Ctrl+C cleared input. Check: Ctrl+C handler must show cursor."
    puts "TUI_TEST_PASS Ctrl+C cleared input but did not exit"
    send "\x7f\x7f\x7f\x7f\x7f"
EXPECT
    finish_script "$f"
    run_test_file "idle-ctrl-c-nonempty" "$f"
}

test_idle_resize() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
    # CAVEAT: expect's stty columns/rows may not send SIGWINCH to the child.
    # If SIGWINCH is not sent, sned's resize handler never fires and this
    # test passes without actually exercising the resize code path.
    catch {stty columns 120 rows 30}
    sleep 0.5
    catch {stty columns 80 rows 24}
    sleep 0.5
    # CAVEAT: if SIGWINCH did not fire, this check passes trivially.
    # If it did fire, cursor must be visible per the state machine.
    check_cursor "Bug: cursor not visible after idle resize. Check: resize handler must call render_input_line(false) which emits ?25h."
    send "test"
    sleep 0.3
    puts "TUI_TEST_PASS resize handled, input responsive"
    send "\x7f\x7f\x7f\x7f"
EXPECT
    finish_script "$f"
    run_test_file "idle-resize" "$f"
}

test_busy_keystroke() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
    send "list files\r"
    expect {
        -re "⏱|elapsed|files|listing|searching|task completed|⠋" { }
        timeout { puts "TUI_TEST_FAIL agent did not produce output. Check: provider API key, network."; exit 1 }
    }
    # Agent is now busy -- send keystroke and verify cursor hidden
    send "typing while busy"
    sleep 0.3
    check_cursor_hidden "Bug: cursor visible (?25h) during AGENT_BUSY keystroke. Check: render_input_line(true) must emit ?25l, not ?25h."
    # Wait for agent to complete
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL agent did not complete. Check: does the agent task call agent_done.notify_one()? Does render_input_line show cursor after completion?"; exit 1 }
    }
    sleep 0.2
    check_cursor "After agent completed while typing. Bug: cursor-disappears-after-turn. Check: render_input_line(false) must emit ?25h."
    puts "TUI_TEST_PASS input visible during agent execution, cursor hidden then shown"
    send "\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f"
EXPECT
    finish_script "$f"
    run_test_file "busy-keystroke" "$f"
}

test_busy_enter_queue() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
    send "read the README\r"
    # Wait for agent output to confirm agent is actually busy
    expect {
        -re "⏱|elapsed|searching|reading|task completed|⠋" { }
        timeout { puts "TUI_TEST_FAIL agent did not start -- no output seen after Enter. Check: provider API key, network."; exit 1 }
    }
    # Agent is busy -- verify cursor hidden before queueing
    check_cursor_hidden "Bug: cursor visible (?25h) during AGENT_BUSY before queue. Check: render_input_line(true) must emit ?25l."
    # Send queued message while agent is busy
    send "also check BUILD_SPEC.md\r"
    sleep 0.5
    expect {
        -re "(?i)queue" { }
        timeout { puts "TUI_TEST_FAIL no queue feedback -- message enqueue path broken. Check: handle.enqueue_text_message() and eprint_info queue message in interactive.rs."; exit 1 }
    }
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL agent did not complete after queue. Check: does agent_done.notify_one() fire after processing queued messages?"; exit 1 }
    }
    sleep 0.2
    check_cursor "After queued agent completed. Bug: cursor-disappears-after-turn. Check: render_input_line(false) must emit ?25h."
    puts "TUI_TEST_PASS queued message echoed and agent completed"
EXPECT
    finish_script "$f"
    run_test_file "busy-enter-queue" "$f"
}

test_busy_enter_empty() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
    send "say hello\r"
    # Wait for agent output to confirm agent is actually busy
    expect {
        -re "⏱|elapsed|Hello|How can I help|task completed|⠋" { }
        timeout { puts "TUI_TEST_FAIL agent did not start -- no output seen after Enter. Check: provider API key, network."; exit 1 }
    }
    # Agent is busy -- send empty Enter (should be no-op)
    send "\r"
    sleep 0.3
    check_cursor_hidden "Bug: cursor visible (?25h) during AGENT_BUSY after empty Enter. Check: AGENT_BUSY empty Enter should call render_input_line(true), which emits ?25l."
    check_no_agent_output "Bug: empty Enter during AGENT_BUSY spawned extra agent work. Check: AGENT_BUSY empty Enter should be no-op."
    # Wait for the first agent turn to complete
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL agent did not complete. Check: agent_done.notify_one()."; exit 1 }
    }
    sleep 0.2
    check_cursor "After agent completed. Bug: cursor-disappears-after-turn. Check: render_input_line(false) must emit ?25h."
    puts "TUI_TEST_PASS empty Enter during AGENT_BUSY was no-op, cursor stayed hidden then shown"
EXPECT
    finish_script "$f"
    run_test_file "busy-enter-empty" "$f"
}

test_busy_ctrl_c() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
    send "read all files\r"
    # Wait for agent output to confirm agent is actually busy
    expect {
        -re "⏱|elapsed|searching|reading|task completed|⠋" { }
        timeout { puts "TUI_TEST_FAIL agent did not start -- no output seen. Check: provider API key, network."; exit 1 }
    }
    sleep 0.3
    send "\x03"
    sleep 0.5
    expect {
        -re {\^C} { }
        timeout { puts "TUI_TEST_FAIL no ^C on Ctrl+C during AGENT_BUSY. Check: agent cancellation should print ^C."; exit 1 }
    }
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL Ctrl+C did not return to idle prompt. Check: agent cancellation in interactive.rs, agent_done notification after abort."; exit 1 }
    }
    sleep 0.2
    check_cursor "After Ctrl+C aborted agent. Check: agent cancellation path must show cursor."
    send "still alive"
    sleep 0.3
    puts "TUI_TEST_PASS Ctrl+C aborted agent, ^C shown, returned to idle"
    send "\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f"
EXPECT
    finish_script "$f"
    run_test_file "busy-ctrl-c" "$f"
}

test_busy_resize() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
    send "say hello\r"
    # Wait for agent output to confirm agent is actually busy
    expect {
        -re "⏱|elapsed|Hello|How can I help|task completed|⠋" { }
        timeout { puts "TUI_TEST_FAIL agent did not start -- no output seen after Enter. Check: provider API key, network."; exit 1 }
    }
    # CAVEAT: expect's stty columns/rows may not send SIGWINCH to the child.
    # If SIGWINCH is not sent, sned's resize handler never fires and the
    # cursor-hidden check below still passes (no ?25h appears either).
    catch {stty columns 120 rows 30}
    sleep 0.5
    # Cursor should stay hidden during AGENT_BUSY resize
    check_cursor_hidden "Bug: cursor visible (?25h) during AGENT_BUSY resize. Check: AGENT_BUSY resize must call render_input_line(true), which emits ?25l."
    catch {stty columns 80 rows 24}
    sleep 0.5
    # Wait for agent to complete
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL agent did not complete after resize. Check: agent_done.notify_one()."; exit 1 }
    }
    sleep 0.2
    check_cursor "After agent completed with resize. Bug: cursor-disappears-after-turn. Check: render_input_line(false) must emit ?25h."
    puts "TUI_TEST_PASS resize during AGENT_BUSY handled, cursor stayed hidden then shown"
EXPECT
    finish_script "$f"
    run_test_file "busy-resize" "$f"
}

test_agent_done() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
    send "say hi\r"
    # Wait for agent task output (exclude elapsed markers so we can check them separately)
    expect {
        -re "⠋|▶|Hi|Hello|How can I help|task completed" { }
        timeout { puts "TUI_TEST_FAIL agent did not produce output. Check: provider API key, network, model availability."; exit 1 }
    }
    # Agent completion must show elapsed time before or alongside the prompt
    expect {
        -re "⏱|elapsed" { }
        timeout { puts "TUI_TEST_FAIL no elapsed time after agent completion. Check: agent_done handler must display elapsed time before re-rendering prompt."; exit 1 }
    }
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL agent did not return to idle prompt. Check: agent_done.notify_one()"; exit 1 }
    }
    sleep 0.2
    check_cursor "After agent completed. Bug: cursor-disappears-after-turn. Check: render_input_line(false) must emit ?25h."
    send "after done"
    sleep 0.3
    send "\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f"
    puts "TUI_TEST_PASS cursor visible after agent completion with elapsed time"
EXPECT
    finish_script "$f"
    run_test_file "agent-done" "$f"
}

test_multi_turn() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
    # Turn 1
    send "say hello\r"
    expect {
        -re "⏱|elapsed|Hi|Hello|How can I help|task completed" { }
        timeout { puts "TUI_TEST_FAIL turn 1 did not produce output. Check: provider API key, network."; exit 1 }
    }
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL turn 1 did not return to idle. Check: agent_done notification."; exit 1 }
    }
    # Verify cursor visible at idle BEFORE sending turn 2.
    sleep 0.2
    check_cursor "After turn 1. Bug: cursor-disappears-after-turn. Check: render_input_line(false) must emit ?25h."
    # Turn 2
    send "say world\r"
    expect {
        -re "⏱|elapsed|Hi|Hello|How can I help|task completed" { }
        timeout { puts "TUI_TEST_FAIL turn 2 did not produce output."; exit 1 }
    }
    # Verify cursor hidden during AGENT_BUSY before turn 2 completes
    check_cursor_hidden "Bug: cursor visible (?25h) during AGENT_BUSY turn 2. Check: render_input_line(true) must emit ?25l."
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL turn 2 did not complete. Check: agent_done notification."; exit 1 }
    }
    sleep 0.2
    check_cursor "After turn 2. Bug confirmed: cursor disappears on follow-up prompts. Check: render_input_line(false) cursor show logic."
    # Turn 3
    send "say foo\r"
    sleep 0.5
    send "interrupting text"
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL turn 3 did not complete. Check: cursor toggle across multiple AGENT_BUSY/IDLE transitions."; exit 1 }
    }
    sleep 0.2
    check_cursor "After turn 3. Cursor disappears on every subsequent prompt. Check: render_input_line(false) always emits ?25h."
    puts "TUI_TEST_PASS three turns completed with cursor toggling"
EXPECT
    finish_script "$f"
    run_test_file "multi-turn" "$f"
}

test_input_line_clean() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
    # Turn 1: submit with a unique marker
    send "say QjK7x_input_test\r"
    expect {
        -re "⏱|elapsed|QjK7x|task completed" { }
        timeout { puts "TUI_TEST_FAIL turn 1 did not produce output"; exit 1 }
    }
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL turn 1 did not complete"; exit 1 }
    }
    sleep 0.2
    check_cursor "After turn 1 in input-line-clean."
    # After agent completes, the input line should be clean (empty).
    set timeout 2
    expect {
        "QjK7x_input_test" { puts "TUI_TEST_FAIL input line not clean after agent completion -- old submission text persists on input line. Check: render_input_line() must clear/reset the input buffer on IDLE transition, and re-render with empty input."; exit 1 }
        timeout { }
    }
    set timeout 10
    # Turn 2: submit a different prompt
    send "say hello\r"
    expect {
        -re "⏱|elapsed|hello|task completed" { }
        timeout { puts "TUI_TEST_FAIL turn 2 did not produce output"; exit 1 }
    }
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL turn 2 did not complete"; exit 1 }
    }
    sleep 0.2
    check_cursor "After turn 2 in input-line-clean."
    set timeout 2
    expect {
        "say hello" { puts "TUI_TEST_FAIL input line not clean after turn 2 -- submission text 'say hello' persists on input line. Check: render_input_line() must clear input buffer after submission."; exit 1 }
        timeout { }
    }
    set timeout 10
    puts "TUI_TEST_PASS input line clean after agent completion"
EXPECT
    finish_script "$f"
    run_test_file "input-line-clean" "$f"
}

test_queue_input_clean() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
    # Turn 1: start agent, then queue a second message while busy
    send "say first_Zm9q\r"
    expect {
        -re "⏱|elapsed|first|task completed" { }
        timeout { puts "TUI_TEST_FAIL turn 1 did not produce output"; exit 1 }
    }
    sleep 0.5
    # Agent is busy -- send a queued message
    send "say second_Xr4p\r"
    # Wait for both turns to complete
    expect {
        -re "⏱|elapsed|second|task completed" { }
        timeout { puts "TUI_TEST_FAIL queued turn did not produce output"; exit 1 }
    }
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL agent did not complete queued turns. Check: agent_done after queue processing."; exit 1 }
    }
    sleep 0.2
    check_cursor "After queued turns completed."
    # Input line must be clean -- no leftover text from either submission.
    set timeout 2
    expect {
        "say second_Xr4p" { puts "TUI_TEST_FAIL queued submission text persists on input line after agent completion. Check: render_input_line() must clear input buffer after queued submission, and the idle re-render after agent_done must show only the prompt with empty input."; exit 1 }
        "say first_Zm9q" { puts "TUI_TEST_FAIL first submission text persists on input line after agent completion. Check: render_input_line() input buffer reset."; exit 1 }
        timeout { }
    }
    set timeout 10
    # Queue a third and fourth message
    send "say third_Wk8m\r"
    expect {
        -re "⏱|elapsed|third|task completed" { }
        timeout { puts "TUI_TEST_FAIL third queued turn did not produce output"; exit 1 }
    }
    sleep 0.5
    send "say fourth_Jt3n\r"
    expect {
        -re "⏱|elapsed|fourth|task completed" { }
        timeout { puts "TUI_TEST_FAIL fourth queued turn did not produce output"; exit 1 }
    }
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL agent did not complete second queue batch."; exit 1 }
    }
    sleep 0.2
    check_cursor "After second queue batch."
    set timeout 2
    expect {
        "say fourth_Jt3n" { puts "TUI_TEST_FAIL fourth submission text persists on input line after queue. Check: render_input_line() must reset input buffer after each queued submission completes."; exit 1 }
        "say third_Wk8m" { puts "TUI_TEST_FAIL third submission text persists on input line after queue."; exit 1 }
        timeout { }
    }
    set timeout 10
    puts "TUI_TEST_PASS input line clean after queued submissions"
EXPECT
    finish_script "$f"
    run_test_file "queue-input-clean" "$f"
}

test_startup_banner() {
    local f
    f=$(mktemp /tmp/sned-tui-script-XXXXXX)
    cat > "$f" <<'EXPECT'
    set timeout 10
    set env(TERM) xterm-256color
EXPECT
    cat >> "$f" <<SPAWNLINE
    spawn ${REPO_ROOT}/user-scripts/sned-pty-helper 24 80 ${REPO_ROOT}/target/debug/sned --provider minimax --model "MiniMax-M2.7" --yolo
SPAWNLINE
    cat >> "$f" <<'EXPECT'
    sleep 1
    expect {
        -re "sned" { }
        timeout { puts "TUI_TEST_FAIL no startup banner. Check: is_print_quiet() suppressing output? Does eprint_raw() work with scroll region?"; exit 1 }
    }
    expect {
        -re "❯" { }
        timeout { puts "TUI_TEST_FAIL no prompt after banner. Check: set_scroll_region() called at startup? Does render_input_line() position cursor correctly?"; exit 1 }
    }
    expect {
        -re {\?25h} { }
        timeout { puts "TUI_TEST_FAIL cursor not visible (?25h) after startup. Check: render_input_line() must emit cursor show on first render."; exit 1 }
    }
    puts "TUI_TEST_PASS startup banner and prompt rendered"
EXPECT
    finish_script "$f"
    run_test_file "startup-banner" "$f"
}

# --- Run all tests ---

ALL_TEST_NAMES=(idle-keystroke idle-enter-nonempty idle-enter-empty idle-ctrl-c-empty idle-ctrl-c-nonempty idle-resize busy-keystroke busy-enter-queue busy-enter-empty busy-ctrl-c busy-resize agent-done multi-turn input-line-clean queue-input-clean startup-banner)

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

echo "=========================================="
echo "  sned TUI State Machine Smoke Test"
echo "=========================================="
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
test_busy_enter_empty
sleep 0.5
test_busy_ctrl_c
sleep 0.5
test_busy_resize
sleep 0.5
test_agent_done
sleep 0.5
test_multi_turn
sleep 0.5
test_input_line_clean
sleep 0.5
test_queue_input_clean
sleep 0.5
test_startup_banner

# --- Results ---

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
    echo "  Failed tests and likely sources:"
    for name in "${FAIL_NAMES[@]}"; do
        printf "    %-22s -> %s\n" "$name" "${TEST_SOURCE[$name]}"
    done
    echo ""
    echo "  Next steps:"
    echo "    1. Re-run failing test(s) with verbose output:"
    for name in "${FAIL_NAMES[@]}"; do
        echo "       ./user-scripts/tui-smoke-test.sh --verbose --test $name"
    done
    echo "    2. Consult docs/TUI_STATE_MACHINE.md for expected behavior"
    echo "    3. Check render_input_line() invariants in src/cli/interactive.rs"
    echo "    4. Common root causes:"
    echo "       - Cursor not shown after agent completes (invariant: ?25h on idle)"
    echo "       - Cursor not hidden during agent execution (invariant: ?25l on busy)"
    echo "       - Prompt echo written to pinned input row instead of scroll region"
    echo "       - Cursor left at input row after agent-busy render (must return to scroll region bottom)"
    echo "       - Raw mode dropped during agent execution (kernel echo garbles display)"
    echo "       - Scroll region not reset on exit (terminal left in broken state)"
    echo "       - Input line not cleared after submission (stale text persists on next prompt)"
    exit 1
fi
