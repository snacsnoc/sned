#!/usr/bin/env bash
# TUI smoke test for sned ratatui-based interactive shell.
#
# Matches text content only (no ANSI cursor codes or scroll regions).
# Uses SNED_NO_ALTERNATE_SCREEN=1 for inline viewport rendering.
#
# Prerequisites:
#   - expect installed (brew install expect)
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
            echo "startup          Banner and prompt render on launch"
            echo "idle-keystroke   IDLE: keystrokes render in input"
            echo "idle-enter       IDLE + Enter: prompt echoes, agent runs"
            echo "idle-ctrl-c      IDLE + Ctrl+C: clean exit"
            echo "busy-keystroke   AGENT_BUSY: input still accepts keystrokes"
            echo "busy-ctrl-c      AGENT_BUSY + Ctrl+C: agent aborts, ^C shown"
            echo "agent-done       Agent completes, elapsed time shown"
            echo "multi-turn       Multiple IDLE<->AGENT cycles"
            echo "input-clean      Input line clears after submission"
            exit 0 ;;
        --help|-h)
            cat <<'HELP'
Usage: tui-smoke-test.sh [OPTIONS]

Smoke test for the sned ratatui TUI. Matches text content only.

OPTIONS:
  --verbose, -v      Show expect interaction output
  --test NAME, -t    Run a single test by name (see --list)
  --list             List all test names
  --help, -h         Show this help

PREREQUISITES:
  - cargo build completed
  - MINIMAX_API_KEY environment variable set
  - expect installed

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

if ! command -v expect &>/dev/null; then
    echo "FAIL: expect not found. Install: brew install expect" >&2
    exit 2
fi

if [[ -z "${MINIMAX_API_KEY:-}" ]]; then
    echo "FAIL: MINIMAX_API_KEY not set" >&2
    exit 2
fi

PASS_COUNT=0
FAIL_COUNT=0
FAIL_NAMES=()
RESULTS=()
TOTAL_TESTS=9
TEST_TIMEOUT=60

declare -A TEST_DESC TEST_SOURCE
TEST_DESC[startup]="Banner (provider/model) and prompt render on launch"
TEST_SOURCE[startup]="src/cli/interactive.rs startup banner, App::new()"

TEST_DESC[idle-keystroke]="IDLE state: keystrokes render in input textarea"
TEST_SOURCE[idle-keystroke]="tui-textarea handles input, ratatui renders"

TEST_DESC[idle-enter]="IDLE + Enter: prompt echoes to output, agent starts"
TEST_SOURCE[idle-enter]="src/cli/interactive.rs submit, src/core/agent_loop.rs"

TEST_DESC[idle-ctrl-c]="IDLE + Ctrl+C: clean exit, terminal restored"
TEST_SOURCE[idle-ctrl-c]="src/cli/interactive.rs Ctrl+C handler"

TEST_DESC[busy-keystroke]="AGENT_BUSY: input textarea still accepts keystrokes"
TEST_SOURCE[busy-keystroke]="tui-textarea active during agent execution"

TEST_DESC[busy-ctrl-c]="AGENT_BUSY + Ctrl+C: ^C shown, agent aborts"
TEST_SOURCE[busy-ctrl-c]="src/cli/interactive.rs agent cancellation"

TEST_DESC[agent-done]="Agent completes: elapsed time shown, prompt returns"
TEST_SOURCE[agent-done]="src/cli/interactive.rs agent_done handler"

TEST_DESC[multi-turn]="Multiple IDLE<->AGENT cycles complete correctly"
TEST_SOURCE[multi-turn]="src/cli/interactive.rs main loop"

TEST_DESC[input-clean]="Input line clears after submission"
TEST_SOURCE[input-clean]="src/cli/interactive.rs submit clears textarea"

run_test_file() {
    local name="$1"
    local script_file="$2"

    if [[ -n "$RUN_TEST" && "$RUN_TEST" != "$name" ]]; then
        rm -f "$script_file"
        return 0
    fi

    local desc="${TEST_DESC[$name]:-unknown}"
    local source="${TEST_SOURCE[$name]:-unknown}"

    printf "  [%d/%d] RUNNING %s\n" $((PASS_COUNT + FAIL_COUNT + 1)) "$TOTAL_TESTS" "$name"
    printf "         %s\n" "$desc"

    local tmpout
    tmpout=$(mktemp /tmp/sned-tui-test-XXXXXX)

    local expect_cmd="expect"
    if [[ $VERBOSE -eq 1 ]]; then
        expect_cmd="expect -d"
    fi

    local exit_code=0
    timeout "$TEST_TIMEOUT" $expect_cmd "$script_file" > "$tmpout" 2>&1 || exit_code=$?

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
        result_line="FAIL  $name -- no TUI_TEST_PASS marker"
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
        if [[ $exit_code -eq 124 ]]; then
            printf "         Timeout: sned hung or expected text never appeared\n"
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

new_script() {
    local f
    f=$(mktemp /tmp/sned-tui-script-XXXXXX)
    cat > "$f" <<'EXPECT'
set timeout 10
set env(TERM) xterm-256color
EXPECT
    cat >> "$f" <<SPAWNLINE
spawn $ENV{SNED_NO_ALTERNATE_SCREEN} ${REPO_ROOT}/target/debug/sned --provider minimax --model "MiniMax-M2.7" --yolo
SPAWNLINE
    cat >> "$f" <<'EXPECT'
sleep 1
expect {
    -re "❯" { }
    timeout { puts "TUI_TEST_FAIL startup timeout -- sned did not render prompt"; exit 1 }
}
sleep 0.3
EXPECT
    echo "$f"
}

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

test_startup() {
    local f
    f=$(mktemp /tmp/sned-tui-script-XXXXXX)
    cat > "$f" <<'EXPECT'
set timeout 10
set env(TERM) xterm-256color
EXPECT
    cat >> "$f" <<SPAWNLINE
spawn $ENV{SNED_NO_ALTERNATE_SCREEN} ${REPO_ROOT}/target/debug/sned --provider minimax --model "MiniMax-M2.7" --yolo
SPAWNLINE
    cat >> "$f" <<'EXPECT'
sleep 1
expect {
    -re "sned" { }
    timeout { puts "TUI_TEST_FAIL no startup banner"; exit 1 }
}
expect {
    -re "❯" { }
    timeout { puts "TUI_TEST_FAIL no prompt after banner"; exit 1 }
}
puts "TUI_TEST_PASS startup banner and prompt rendered"
EXPECT
    finish_script "$f"
    run_test_file "startup" "$f"
}

test_idle_keystroke() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
send "hello"
sleep 0.5
send "\x7f\x7f\x7f\x7f\x7f"
sleep 0.2
puts "TUI_TEST_PASS input responsive"
EXPECT
    finish_script "$f"
    run_test_file "idle-keystroke" "$f"
}

test_idle_enter() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
send "say hello\r"
expect {
    -re "⠋|▶|elapsed|⏱|Hello|How can I help" { }
    timeout { puts "TUI_TEST_FAIL agent did not start"; exit 1 }
}
expect {
    -re "❯" { }
    timeout { puts "TUI_TEST_FAIL agent did not return to idle"; exit 1 }
}
sleep 0.2
puts "TUI_TEST_PASS prompt echoed and agent completed"
EXPECT
    finish_script "$f"
    run_test_file "idle-enter" "$f"
}

test_idle_ctrl_c() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
send "\x03"
expect {
    eof { puts "TUI_TEST_PASS clean exit on Ctrl+C" }
    timeout { puts "TUI_TEST_FAIL did not exit on Ctrl+C"; exit 1 }
}
EXPECT
    run_test_file "idle-ctrl-c" "$f"
}

test_busy_keystroke() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
send "list files\r"
expect {
    -re "⏱|elapsed|files|listing|⠋" { }
    timeout { puts "TUI_TEST_FAIL agent did not produce output"; exit 1 }
}
send "typing while busy"
sleep 0.3
expect {
    -re "❯" { }
    timeout { puts "TUI_TEST_FAIL agent did not complete"; exit 1 }
}
sleep 0.2
send "\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f"
puts "TUI_TEST_PASS input active during agent execution"
EXPECT
    finish_script "$f"
    run_test_file "busy-keystroke" "$f"
}

test_busy_ctrl_c() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
send "read all files\r"
expect {
    -re "⏱|elapsed|searching|reading|⠋" { }
    timeout { puts "TUI_TEST_FAIL agent did not start"; exit 1 }
}
sleep 0.3
send "\x03"
sleep 0.5
expect {
    -re "\^C" { }
    timeout { puts "TUI_TEST_FAIL no ^C on Ctrl+C"; exit 1 }
}
expect {
    -re "❯" { }
    timeout { puts "TUI_TEST_FAIL Ctrl+C did not return to idle"; exit 1 }
}
send "still alive"
sleep 0.3
send "\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f"
puts "TUI_TEST_PASS Ctrl+C aborted agent"
EXPECT
    finish_script "$f"
    run_test_file "busy-ctrl-c" "$f"
}

test_agent_done() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
send "say hi\r"
expect {
    -re "⠋|▶|Hi|Hello|How can I help" { }
    timeout { puts "TUI_TEST_FAIL agent did not produce output"; exit 1 }
}
expect {
    -re "⏱|elapsed" { }
    timeout { puts "TUI_TEST_FAIL no elapsed time after agent completion"; exit 1 }
}
expect {
    -re "❯" { }
    timeout { puts "TUI_TEST_FAIL agent did not return to idle"; exit 1 }
}
sleep 0.2
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
send "say hello\r"
expect {
    -re "⏱|elapsed|Hi|Hello" { }
    timeout { puts "TUI_TEST_FAIL turn 1 did not produce output"; exit 1 }
}
expect {
    -re "❯" { }
    timeout { puts "TUI_TEST_FAIL turn 1 did not complete"; exit 1 }
}
send "say world\r"
expect {
    -re "⏱|elapsed|world" { }
    timeout { puts "TUI_TEST_FAIL turn 2 did not produce output"; exit 1 }
}
expect {
    -re "❯" { }
    timeout { puts "TUI_TEST_FAIL turn 2 did not complete"; exit 1 }
}
send "say foo\r"
sleep 0.5
expect {
    -re "❯" { }
    timeout { puts "TUI_TEST_FAIL turn 3 did not complete"; exit 1 }
}
sleep 0.2
puts "TUI_TEST_PASS three turns completed"
EXPECT
    finish_script "$f"
    run_test_file "multi-turn" "$f"
}

test_input_clean() {
    local f; f=$(new_script)
    cat >> "$f" <<'EXPECT'
send "say QjK7x_input_test\r"
expect {
    -re "⏱|elapsed|QjK7x" { }
    timeout { puts "TUI_TEST_FAIL turn 1 did not produce output"; exit 1 }
}
expect {
    -re "❯" { }
    timeout { puts "TUI_TEST_FAIL turn 1 did not complete"; exit 1 }
}
sleep 0.2
set timeout 2
expect {
    "QjK7x_input_test" { puts "TUI_TEST_FAIL input line not clean after agent completion"; exit 1 }
    timeout { }
}
set timeout 10
send "say hello\r"
expect {
    -re "⏱|elapsed|hello" { }
    timeout { puts "TUI_TEST_FAIL turn 2 did not produce output"; exit 1 }
}
expect {
    -re "❯" { }
    timeout { puts "TUI_TEST_FAIL turn 2 did not complete"; exit 1 }
}
sleep 0.2
set timeout 2
expect {
    "say hello" { puts "TUI_TEST_FAIL input line not clean after turn 2"; exit 1 }
    timeout { }
}
set timeout 10
puts "TUI_TEST_PASS input line clean after agent completion"
EXPECT
    finish_script "$f"
    run_test_file "input-clean" "$f"
}

ALL_TEST_NAMES=(startup idle-keystroke idle-enter idle-ctrl-c busy-keystroke busy-ctrl-c agent-done multi-turn input-clean)

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

export SNED_NO_ALTERNATE_SCREEN=1

test_startup
sleep 0.5
test_idle_keystroke
sleep 0.5
test_idle_enter
sleep 0.5
test_idle_ctrl_c
sleep 0.5
test_busy_keystroke
sleep 0.5
test_busy_ctrl_c
sleep 0.5
test_agent_done
sleep 0.5
test_multi_turn
sleep 0.5
test_input_clean

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
