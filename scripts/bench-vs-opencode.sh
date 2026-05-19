#!/usr/bin/env bash
#
# bench-vs-opencode.sh — Compare sned vs opencode performance
#
# Usage:
#   ./scripts/bench-vs-opencode.sh [OPTIONS]
#
# Options:
#   --fixture <name>       Run specific fixture: trivial, medium, long (default: all)
#   --provider <name>      Provider to use for sned (default: minimax)
#   --model <name>         Model to use for sned (default: MiniMax-M2.7)
#   --opencode-model <str> Model string for opencode (default: minimax-coding-plan/MiniMax-M2.7)
#   --iterations <n>       Number of iterations per fixture (default: 3)
#   --output <file>        Output JSON file (default: stdout)
#   --help                 Show this help message
#
# Requirements:
#   - sned binary (built via cargo build --release)
#   - opencode binary (npm install -g opencode-ai)
#   - Provider API key (MINIMAX_API_KEY for minimax provider)
#
# Output:
#   JSON object with timing and memory comparison for each fixture
#

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURES_DIR="${SCRIPT_DIR}/bench-fixtures"
SNED_DIR="${SCRIPT_DIR}/.."

FIXTURE="all"
PROVIDER="minimax"
MODEL="MiniMax-M2.7"
OPENCODE_MODEL="minimax-coding-plan/MiniMax-M2.7"
ITERATIONS=3
OUTPUT_FILE=""
while [[ $# -gt 0 ]]; do
    case $1 in
        --fixture)
            FIXTURE="$2"
            shift 2
            ;;
        --provider)
            PROVIDER="$2"
            shift 2
            ;;
        --model)
            MODEL="$2"
            shift 2
            ;;
        --opencode-model)
            OPENCODE_MODEL="$2"
            shift 2
            ;;
        --iterations)
            ITERATIONS="$2"
            shift 2
            ;;
        --output)
            OUTPUT_FILE="$2"
            shift 2
            ;;
        --help)
            head -24 "$0" | tail -20 | sed 's/^# \?//'
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            echo "Use --help for usage information" >&2
            exit 1
            ;;
    esac
done

if [[ "$FIXTURE" != "all" && "$FIXTURE" != "trivial" && "$FIXTURE" != "medium" && "$FIXTURE" != "long" ]]; then
    echo "Error: Invalid fixture '$FIXTURE'. Must be one of: trivial, medium, long, all" >&2
    exit 1
fi

check_prereqs() {
    if ! command -v cargo &> /dev/null; then
        echo "Error: cargo not found" >&2
        exit 1
    fi

    if ! command -v /usr/local/bin/opencode &> /dev/null && ! command -v opencode &> /dev/null; then
        echo "Warning: opencode not found. Skipping opencode benchmarks." >&2
    fi

    case "$PROVIDER" in
        minimax)
            if [[ -z "${MINIMAX_API_KEY:-}" ]]; then
                echo "Error: MINIMAX_API_KEY must be set for minimax provider" >&2
                exit 1
            fi
            ;;
        anthropic)
            if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
                echo "Error: ANTHROPIC_API_KEY must be set for anthropic provider" >&2
                exit 1
            fi
            ;;
        openai)
            if [[ -z "${OPENAI_API_KEY:-}" ]]; then
                echo "Error: OPENAI_API_KEY must be set for openai provider" >&2
                exit 1
            fi
            ;;
    esac

    if [[ "$(uname)" == "Darwin" ]]; then
        if ! command -v gtime &> /dev/null; then
            echo "Error: GNU time (gtime) is required on macOS for memory measurement." >&2
            echo "Install with: brew install coreutils" >&2
            exit 1
        fi
    fi
}

measure_command() {
    local stdout_file stderr_file time_file exit_code=0
    local time_cmd="/usr/bin/time"
    if command -v gtime &> /dev/null; then
        time_cmd="gtime"
    fi

    stdout_file=$(mktemp)
    stderr_file=$(mktemp)
    time_file=$(mktemp)

    $time_cmd -v -o "$time_file" "$@" >"$stdout_file" 2>"$stderr_file" || exit_code=$?

    local stdout_bytes stderr_bytes combined_bytes final_answer_bytes
    stdout_bytes=$(wc -c <"$stdout_file" | tr -d ' ')
    stderr_bytes=$(wc -c <"$stderr_file" | tr -d ' ')
    combined_bytes=$((stdout_bytes + stderr_bytes))

    local completion_text=""
    completion_text=$(jq -r 'select(.type == "completion") | .result' "$stdout_file" 2>/dev/null | tail -n 1 || true)
    if [[ -n "$completion_text" ]]; then
        final_answer_bytes=$(printf '%s' "$completion_text" | wc -c | tr -d ' ')
    elif jq -e . "$stdout_file" >/dev/null 2>&1; then
        final_answer_bytes=$(jq -rj 'select(.type == "text") | .text' "$stdout_file" 2>/dev/null | wc -c | tr -d ' ')
    else
        final_answer_bytes=$(perl -0pe 's/[\r\n]+\z//' "$stdout_file" | wc -c | tr -d ' ')
    fi

    local real_ms user_ms sys_ms max_rss_kb
    real_ms=$(grep "Elapsed (wall clock) time" "$time_file" | awk '{print $NF}' | { read -r t; parse_time_to_ms "$t"; })
    user_ms=$(awk -F: '/User time/ {printf "%.0f", $2 * 1000}' "$time_file")
    sys_ms=$(awk -F: '/System time/ {printf "%.0f", $2 * 1000}' "$time_file")
    max_rss_kb=$(awk -F: '/Maximum resident set size/ {gsub(/ /, "", $2); print $2}' "$time_file")

    local output_preview=""
    if [[ "$exit_code" -ne 0 ]]; then
        output_preview=$({ head -c 150 "$stdout_file"; printf ' '; head -c 150 "$stderr_file"; } | tr '\n' ' ' | sed 's/"/\\"/g')
    fi

    jq -n \
        --argjson real_ms "${real_ms:-0}" \
        --argjson user_ms "${user_ms:-0}" \
        --argjson sys_ms "${sys_ms:-0}" \
        --argjson max_rss_kb "${max_rss_kb:-0}" \
        --argjson exit_code "$exit_code" \
        --argjson stdout_bytes "$stdout_bytes" \
        --argjson stderr_bytes "$stderr_bytes" \
        --argjson combined_bytes "$combined_bytes" \
        --argjson final_answer_bytes "$final_answer_bytes" \
        --arg output_preview "$output_preview" \
        '{
          real_ms: $real_ms,
          user_ms: $user_ms,
          sys_ms: $sys_ms,
          max_rss_kb: $max_rss_kb,
          exit_code: $exit_code,
          stdout_bytes: $stdout_bytes,
          stderr_bytes: $stderr_bytes,
          combined_bytes: $combined_bytes,
          final_answer_bytes: $final_answer_bytes,
          output_preview: $output_preview
        }'

    rm -f "$stdout_file" "$stderr_file" "$time_file"
}

parse_time_to_ms() {
    local time_str="${1:-}"
    local minutes seconds ms

    if [[ -z "$time_str" ]]; then
        echo "0"
        return
    fi

    if [[ "$time_str" =~ ^([0-9]+):([0-9]+)\.([0-9]+)$ ]]; then
        minutes="${BASH_REMATCH[1]}"
        seconds="${BASH_REMATCH[2]}"
        ms="${BASH_REMATCH[3]}"
        ms="${ms}00"
        ms="${ms:0:3}"
        echo $(( (10#$minutes * 60 + 10#$seconds) * 1000 + 10#$ms ))
    elif [[ "$time_str" =~ ^([0-9]+):([0-9]+):([0-9]+)\.([0-9]+)$ ]]; then
        local hours="${BASH_REMATCH[1]}"
        minutes="${BASH_REMATCH[2]}"
        seconds="${BASH_REMATCH[3]}"
        ms="${BASH_REMATCH[4]}"
        ms="${ms}00"
        ms="${ms:0:3}"
        echo $(( (10#$hours * 3600 + 10#$minutes * 60 + 10#$seconds) * 1000 + 10#$ms ))
    else
        echo "0"
    fi
}

# Regression tests: octal-safe parsing (seconds/minutes 08-09)
_r() {
    local expected="$1" got="$2" label="$3"
    if [[ "$got" != "$expected" ]]; then
        echo "FAIL $label: expected=$expected got=$got" >&2
        exit 1
    fi
}
_r 8090  "$(parse_time_to_ms "0:08.09")" "0:08.09"
_r 9080  "$(parse_time_to_ms "0:09.08")" "0:09.08"
_r 3728090 "$(parse_time_to_ms "1:02:08.09")" "1:02:08.09"

run_fixture() {
    local fixture_name="$1"
    local fixture_file="${FIXTURES_DIR}/${fixture_name}.txt"
    local prompt_file
    local fixture_tmp
    local sned_results=()
    local opencode_results=()
    local i

    if [[ ! -f "$fixture_file" ]]; then
        echo "Error: Fixture file not found: $fixture_file" >&2
        return 1
    fi

    prompt_file=$(mktemp)
    cp "$fixture_file" "$prompt_file"
    fixture_tmp=$(mktemp -d "${TMPDIR:-/tmp}/sned-opencode-bench-${fixture_name}.XXXXXX")
    local prompt
    prompt=$(<"$prompt_file")

    echo "Running fixture: $fixture_name ($ITERATIONS iterations)" >&2

    local sned_bin="${SNED_DIR}/target/release/sned"
    if [[ ! -x "$sned_bin" ]]; then
        echo "Error: sned binary not found at $sned_bin. Run: cargo build --release" >&2
        rm -f "$prompt_file"
        rm -rf "$fixture_tmp"
        return 1
    fi

    local opencode_bin=""
    if command -v /usr/local/bin/opencode &> /dev/null; then
        opencode_bin="/usr/local/bin/opencode"
    elif command -v opencode &> /dev/null; then
        opencode_bin="opencode"
    fi

    for ((i = 1; i <= ITERATIONS; i++)); do
        echo "  sned iteration $i/$ITERATIONS..." >&2
        local result
        local sned_workdir="${fixture_tmp}/sned-${i}"
        local sned_home="${fixture_tmp}/sned-home-${i}"
        mkdir -p "$sned_workdir" "$sned_home"
        result=$(cd "$sned_workdir" && export SNED_DIR="$sned_home" && measure_command "$sned_bin" --provider "$PROVIDER" --model "$MODEL" --yolo --json --no-token-display "$prompt")
        local rc
        rc=$(echo "$result" | jq '.exit_code')
        if [[ "$rc" -ne 0 ]]; then
            local preview
            preview=$(echo "$result" | jq -r '.output_preview')
            echo "  sned iteration $i failed (exit $rc): $preview" >&2
            rm -f "$prompt_file"
            rm -rf "$fixture_tmp"
            return 1
        fi
        sned_results+=("$result")
    done

    if [[ -n "$opencode_bin" ]]; then
        for ((i = 1; i <= ITERATIONS; i++)); do
            echo "  opencode iteration $i/$ITERATIONS..." >&2
            local result
            local opencode_workdir="${fixture_tmp}/opencode-${i}"
            mkdir -p "$opencode_workdir"
            result=$(cd "$opencode_workdir" && measure_command "$opencode_bin" run --dangerously-skip-permissions -m "$OPENCODE_MODEL" "$prompt")
            local rc
            rc=$(echo "$result" | jq '.exit_code')
            if [[ "$rc" -ne 0 ]]; then
                local preview
                preview=$(echo "$result" | jq -r '.output_preview')
                echo "  opencode iteration $i failed (exit $rc): $preview" >&2
                rm -f "$prompt_file"
                rm -rf "$fixture_tmp"
                return 1
            fi
            opencode_results+=("$result")
        done
    else
        echo "  Skipping opencode (not installed)" >&2
    fi

    rm -f "$prompt_file"
    rm -rf "$fixture_tmp"

    local sned_avg_real=0 sned_avg_user=0 sned_avg_sys=0 sned_avg_rss=0
    local sned_avg_stdout_bytes=0 sned_avg_stderr_bytes=0 sned_avg_combined_bytes=0 sned_avg_final_answer_bytes=0
    local opencode_avg_real=0 opencode_avg_user=0 opencode_avg_sys=0 opencode_avg_rss=0
    local opencode_avg_stdout_bytes=0 opencode_avg_stderr_bytes=0 opencode_avg_combined_bytes=0 opencode_avg_final_answer_bytes=0

    for result in "${sned_results[@]}"; do
        sned_avg_real=$((sned_avg_real + $(echo "$result" | jq '.real_ms')))
        sned_avg_user=$((sned_avg_user + $(echo "$result" | jq '.user_ms')))
        sned_avg_sys=$((sned_avg_sys + $(echo "$result" | jq '.sys_ms')))
        sned_avg_rss=$((sned_avg_rss + $(echo "$result" | jq '.max_rss_kb')))
        sned_avg_stdout_bytes=$((sned_avg_stdout_bytes + $(echo "$result" | jq '.stdout_bytes')))
        sned_avg_stderr_bytes=$((sned_avg_stderr_bytes + $(echo "$result" | jq '.stderr_bytes')))
        sned_avg_combined_bytes=$((sned_avg_combined_bytes + $(echo "$result" | jq '.combined_bytes')))
        sned_avg_final_answer_bytes=$((sned_avg_final_answer_bytes + $(echo "$result" | jq '.final_answer_bytes')))
    done
    sned_avg_real=$((sned_avg_real / ITERATIONS))
    sned_avg_user=$((sned_avg_user / ITERATIONS))
    sned_avg_sys=$((sned_avg_sys / ITERATIONS))
    sned_avg_rss=$((sned_avg_rss / ITERATIONS))
    sned_avg_stdout_bytes=$((sned_avg_stdout_bytes / ITERATIONS))
    sned_avg_stderr_bytes=$((sned_avg_stderr_bytes / ITERATIONS))
    sned_avg_combined_bytes=$((sned_avg_combined_bytes / ITERATIONS))
    sned_avg_final_answer_bytes=$((sned_avg_final_answer_bytes / ITERATIONS))

    if [[ ${#opencode_results[@]} -gt 0 ]]; then
        for result in "${opencode_results[@]}"; do
            opencode_avg_real=$((opencode_avg_real + $(echo "$result" | jq '.real_ms')))
            opencode_avg_user=$((opencode_avg_user + $(echo "$result" | jq '.user_ms')))
            opencode_avg_sys=$((opencode_avg_sys + $(echo "$result" | jq '.sys_ms')))
            opencode_avg_rss=$((opencode_avg_rss + $(echo "$result" | jq '.max_rss_kb')))
            opencode_avg_stdout_bytes=$((opencode_avg_stdout_bytes + $(echo "$result" | jq '.stdout_bytes')))
            opencode_avg_stderr_bytes=$((opencode_avg_stderr_bytes + $(echo "$result" | jq '.stderr_bytes')))
            opencode_avg_combined_bytes=$((opencode_avg_combined_bytes + $(echo "$result" | jq '.combined_bytes')))
            opencode_avg_final_answer_bytes=$((opencode_avg_final_answer_bytes + $(echo "$result" | jq '.final_answer_bytes')))
        done
        opencode_avg_real=$((opencode_avg_real / ITERATIONS))
        opencode_avg_user=$((opencode_avg_user / ITERATIONS))
        opencode_avg_sys=$((opencode_avg_sys / ITERATIONS))
        opencode_avg_rss=$((opencode_avg_rss / ITERATIONS))
        opencode_avg_stdout_bytes=$((opencode_avg_stdout_bytes / ITERATIONS))
        opencode_avg_stderr_bytes=$((opencode_avg_stderr_bytes / ITERATIONS))
        opencode_avg_combined_bytes=$((opencode_avg_combined_bytes / ITERATIONS))
        opencode_avg_final_answer_bytes=$((opencode_avg_final_answer_bytes / ITERATIONS))
    fi

    local user_cpu_speedup="null"
    local real_time_ratio="null"
    local memory_ratio="null"

    if [[ $sned_avg_user -gt 0 && $opencode_avg_user -gt 0 ]]; then
        user_cpu_speedup=$(awk "BEGIN {printf \"%.2f\", $opencode_avg_user / $sned_avg_user}")
    fi
    if [[ $opencode_avg_real -gt 0 ]]; then
        real_time_ratio=$(awk "BEGIN {printf \"%.2f\", $sned_avg_real / $opencode_avg_real}")
    fi
    if [[ $opencode_avg_rss -gt 0 ]]; then
        memory_ratio=$(awk "BEGIN {printf \"%.2f\", $sned_avg_rss / $opencode_avg_rss}")
    fi

    if [[ ${#opencode_results[@]} -gt 0 ]]; then
        jq -n \
            --arg fixture "$fixture_name" \
            --argjson iterations "$ITERATIONS" \
            --arg provider "$PROVIDER" \
            --arg model "$MODEL" \
            --arg opencode_model "$OPENCODE_MODEL" \
            --argjson sned_avg_real "$sned_avg_real" \
            --argjson sned_avg_user "$sned_avg_user" \
            --argjson sned_avg_sys "$sned_avg_sys" \
            --argjson sned_avg_rss "$sned_avg_rss" \
            --argjson sned_avg_stdout_bytes "$sned_avg_stdout_bytes" \
            --argjson sned_avg_stderr_bytes "$sned_avg_stderr_bytes" \
            --argjson sned_avg_combined_bytes "$sned_avg_combined_bytes" \
            --argjson sned_avg_final_answer_bytes "$sned_avg_final_answer_bytes" \
            --argjson opencode_avg_real "$opencode_avg_real" \
            --argjson opencode_avg_user "$opencode_avg_user" \
            --argjson opencode_avg_sys "$opencode_avg_sys" \
            --argjson opencode_avg_rss "$opencode_avg_rss" \
            --argjson opencode_avg_stdout_bytes "$opencode_avg_stdout_bytes" \
            --argjson opencode_avg_stderr_bytes "$opencode_avg_stderr_bytes" \
            --argjson opencode_avg_combined_bytes "$opencode_avg_combined_bytes" \
            --argjson opencode_avg_final_answer_bytes "$opencode_avg_final_answer_bytes" \
            --argjson user_cpu_speedup "${user_cpu_speedup:-0}" \
            --argjson real_time_ratio "${real_time_ratio:-0}" \
            --argjson memory_ratio "${memory_ratio:-0}" \
            '{
              fixture: $fixture,
              iterations: $iterations,
              provider: $provider,
              model: $model,
              opencode_model: $opencode_model,
              sned: {
                avg_real_ms: $sned_avg_real,
                avg_user_ms: $sned_avg_user,
                avg_sys_ms: $sned_avg_sys,
                avg_max_rss_kb: $sned_avg_rss,
                avg_stdout_bytes: $sned_avg_stdout_bytes,
                avg_stderr_bytes: $sned_avg_stderr_bytes,
                avg_combined_bytes: $sned_avg_combined_bytes,
                avg_final_answer_bytes: $sned_avg_final_answer_bytes
              },
              opencode: {
                avg_real_ms: $opencode_avg_real,
                avg_user_ms: $opencode_avg_user,
                avg_sys_ms: $opencode_avg_sys,
                avg_max_rss_kb: $opencode_avg_rss,
                avg_stdout_bytes: $opencode_avg_stdout_bytes,
                avg_stderr_bytes: $opencode_avg_stderr_bytes,
                avg_combined_bytes: $opencode_avg_combined_bytes,
                avg_final_answer_bytes: $opencode_avg_final_answer_bytes
              },
              comparison: {
                user_cpu_speedup: $user_cpu_speedup,
                real_time_ratio: $real_time_ratio,
                memory_ratio: $memory_ratio
              }
            }'
    else
        jq -n \
            --arg fixture "$fixture_name" \
            --argjson iterations "$ITERATIONS" \
            --arg provider "$PROVIDER" \
            --arg model "$MODEL" \
            --arg opencode_model "$OPENCODE_MODEL" \
            --argjson sned_avg_real "$sned_avg_real" \
            --argjson sned_avg_user "$sned_avg_user" \
            --argjson sned_avg_sys "$sned_avg_sys" \
            --argjson sned_avg_rss "$sned_avg_rss" \
            --argjson sned_avg_stdout_bytes "$sned_avg_stdout_bytes" \
            --argjson sned_avg_stderr_bytes "$sned_avg_stderr_bytes" \
            --argjson sned_avg_combined_bytes "$sned_avg_combined_bytes" \
            --argjson sned_avg_final_answer_bytes "$sned_avg_final_answer_bytes" \
            '{
              fixture: $fixture,
              iterations: $iterations,
              provider: $provider,
              model: $model,
              opencode_model: $opencode_model,
              sned: {
                avg_real_ms: $sned_avg_real,
                avg_user_ms: $sned_avg_user,
                avg_sys_ms: $sned_avg_sys,
                avg_max_rss_kb: $sned_avg_rss,
                avg_stdout_bytes: $sned_avg_stdout_bytes,
                avg_stderr_bytes: $sned_avg_stderr_bytes,
                avg_combined_bytes: $sned_avg_combined_bytes,
                avg_final_answer_bytes: $sned_avg_final_answer_bytes
              },
              opencode: null,
              comparison: {
                user_cpu_speedup: null,
                real_time_ratio: null,
                memory_ratio: null
              }
            }'
    fi
}

main() {
    check_prereqs

    echo "Building sned..." >&2
    cargo build --release --manifest-path "${SNED_DIR}/Cargo.toml" >&2

    local fixtures=()
    if [[ "$FIXTURE" == "all" ]]; then
        fixtures=("trivial" "medium" "long")
    else
        fixtures=("$FIXTURE")
    fi

    local results=()
    for fixture in "${fixtures[@]}"; do
        local result
        result=$(run_fixture "$fixture")
        if ! echo "$result" | jq -e . >/dev/null 2>&1; then
            echo "Invalid JSON from fixture: $fixture" >&2
            printf '%s\n' "$result" >&2
            exit 1
        fi
        results+=("$result")
    done

    local json_output
    json_output=$(printf '%s\n' "${results[@]}" | jq -s '.')

    if [[ -n "$OUTPUT_FILE" ]]; then
        echo "$json_output" > "$OUTPUT_FILE"
        echo "Results written to: $OUTPUT_FILE" >&2
    else
        echo "$json_output" | jq '.'
    fi
}

main
