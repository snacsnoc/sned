#!/usr/bin/env bash
#
# bench-vs-opencode.sh — Compare sned vs opencode performance
#
# Usage:
#   ./scripts/bench-vs-opencode.sh [OPTIONS]
#
# Options:
#   --fixture <name>     Run specific fixture: trivial, medium, long (default: all)
#   --provider <name>    Provider to use (default: anthropic)
#   --model <name>       Model to use (default: claude-sonnet-4-20250514)
#   --iterations <n>     Number of iterations per fixture (default: 3)
#   --output <file>      Output JSON file (default: stdout)
#   --help               Show this help message
#
# Requirements:
#   - sned binary (cargo build --release)
#   - opencode binary (npm install -g opencode-ai)
#   - ANTHROPIC_API_KEY or OPENAI_API_KEY set
#
# Output:
#   JSON object with timing and memory comparison for each fixture
#

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURES_DIR="${SCRIPT_DIR}/bench-fixtures"
SNED_DIR="${SCRIPT_DIR}/.."

# Defaults
FIXTURE="all"
PROVIDER="anthropic"
MODEL="claude-sonnet-4-20250514"
ITERATIONS=3
OUTPUT_FILE=""

# Parse arguments
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

# Validate fixture
if [[ "$FIXTURE" != "all" && "$FIXTURE" != "trivial" && "$FIXTURE" != "medium" && "$FIXTURE" != "long" ]]; then
    echo "Error: Invalid fixture '$FIXTURE'. Must be one of: trivial, medium, long, all" >&2
    exit 1
fi

# Check prerequisites
check_prereqs() {
    if ! command -v cargo &> /dev/null; then
        echo "Error: cargo not found" >&2
        exit 1
    fi

    if ! command -v /usr/local/bin/opencode &> /dev/null && ! command -v opencode &> /dev/null; then
        echo "Warning: opencode not found. Skipping opencode benchmarks." >&2
    fi

    if [[ -z "${ANTHROPIC_API_KEY:-}" && -z "${OPENAI_API_KEY:-}" ]]; then
        echo "Error: ANTHROPIC_API_KEY or OPENAI_API_KEY must be set" >&2
        exit 1
    fi
}

# Measure command execution with /usr/bin/time
# Returns: {"real_ms": N, "user_ms": N, "sys_ms": N, "max_rss_kb": N}
measure_command() {
    local cmd="$1"
    local time_output
    local real_ms user_ms sys_ms max_rss_kb

    # Use /usr/bin/time for detailed stats
    time_output=$(/usr/bin/time -v bash -c "$cmd" 2>&1)

    # Parse time output (GNU time format)
    real_ms=$(echo "$time_output" | grep "Elapsed (wall clock) time" | awk '{print $NF}' | parse_time_to_ms)
    user_ms=$(echo "$time_output" | grep "User time (seconds)" | awk '{print $NF}' | awk '{printf "%.0f", $1 * 1000}')
    sys_ms=$(echo "$time_output" | grep "System time (seconds)" | awk '{print $NF}' | awk '{printf "%.0f", $1 * 1000}')
    max_rss_kb=$(echo "$time_output" | grep "Maximum resident set size" | awk '{print $NF}')

    # Handle missing values
    real_ms=${real_ms:-0}
    user_ms=${user_ms:-0}
    sys_ms=${sys_ms:-0}
    max_rss_kb=${max_rss_kb:-0}

    echo "{\"real_ms\": $real_ms, \"user_ms\": $user_ms, \"sys_ms\": $sys_ms, \"max_rss_kb\": $max_rss_kb}"
}

# Parse time string (M:SS.ss or H:MM:SS) to milliseconds
parse_time_to_ms() {
    local time_str="$1"
    local minutes seconds ms

    if [[ "$time_str" =~ ^([0-9]+):([0-9]+)\.([0-9]+)$ ]]; then
        minutes="${BASH_REMATCH[1]}"
        seconds="${BASH_REMATCH[2]}"
        ms="${BASH_REMATCH[3]}"
        # Pad ms to 3 digits
        ms="${ms}00"
        ms="${ms:0:3}"
        echo $(( (minutes * 60 + seconds) * 1000 + 10#$ms ))
    elif [[ "$time_str" =~ ^([0-9]+):([0-9]+):([0-9]+)\.([0-9]+)$ ]]; then
        local hours="${BASH_REMATCH[1]}"
        minutes="${BASH_REMATCH[2]}"
        seconds="${BASH_REMATCH[3]}"
        ms="${BASH_REMATCH[4]}"
        ms="${ms}00"
        ms="${ms:0:3}"
        echo $(( (hours * 3600 + minutes * 60 + seconds) * 1000 + 10#$ms ))
    else
        echo "0"
    fi
}

# Run benchmark for a single fixture
run_fixture() {
    local fixture_name="$1"
    local fixture_file="${FIXTURES_DIR}/${fixture_name}.txt"
    local prompt
    local sned_results=()
    local opencode_results=()
    local i

    if [[ ! -f "$fixture_file" ]]; then
        echo "Error: Fixture file not found: $fixture_file" >&2
        return 1
    fi

    prompt=$(cat "$fixture_file")

    echo "Running fixture: $fixture_name ($ITERATIONS iterations)"

    # Run sned iterations
    for ((i = 1; i <= ITERATIONS; i++)); do
        echo "  sned iteration $i/$ITERATIONS..."
        local result
        result=$(measure_command "cd '$SNED_DIR' && cargo run --release -- '$prompt' --provider '$PROVIDER' --model '$MODEL' 2>&1 | head -100")
        sned_results+=("$result")
    done

    # Run opencode iterations if available
    if command -v /usr/local/bin/opencode &> /dev/null || command -v opencode &> /dev/null; then
        for ((i = 1; i <= ITERATIONS; i++)); do
            echo "  opencode iteration $i/$ITERATIONS..."
            local result
            result=$(measure_command "echo '$prompt' | opencode --provider '$PROVIDER' --model '$MODEL' 2>&1 | head -100")
            opencode_results+=("$result")
        done
    else
        echo "  Skipping opencode (not installed)"
    fi

    # Calculate averages
    local sned_avg_real=0 sned_avg_user=0 sned_avg_sys=0 sned_avg_rss=0
    local opencode_avg_real=0 opencode_avg_user=0 opencode_avg_sys=0 opencode_avg_rss=0

    for result in "${sned_results[@]}"; do
        sned_avg_real=$((sned_avg_real + $(echo "$result" | jq '.real_ms')))
        sned_avg_user=$((sned_avg_user + $(echo "$result" | jq '.user_ms')))
        sned_avg_sys=$((sned_avg_sys + $(echo "$result" | jq '.sys_ms')))
        sned_avg_rss=$((sned_avg_rss + $(echo "$result" | jq '.max_rss_kb')))
    done
    sned_avg_real=$((sned_avg_real / ITERATIONS))
    sned_avg_user=$((sned_avg_user / ITERATIONS))
    sned_avg_sys=$((sned_avg_sys / ITERATIONS))
    sned_avg_rss=$((sned_avg_rss / ITERATIONS))

    if [[ ${#opencode_results[@]} -gt 0 ]]; then
        for result in "${opencode_results[@]}"; do
            opencode_avg_real=$((opencode_avg_real + $(echo "$result" | jq '.real_ms')))
            opencode_avg_user=$((opencode_avg_user + $(echo "$result" | jq '.user_ms')))
            opencode_avg_sys=$((opencode_avg_sys + $(echo "$result" | jq '.sys_ms')))
            opencode_avg_rss=$((opencode_avg_rss + $(echo "$result" | jq '.max_rss_kb')))
        done
        opencode_avg_real=$((opencode_avg_real / ITERATIONS))
        opencode_avg_user=$((opencode_avg_user / ITERATIONS))
        opencode_avg_sys=$((opencode_avg_sys / ITERATIONS))
        opencode_avg_rss=$((opencode_avg_rss / ITERATIONS))
    fi

    # Calculate speedup ratios
    local user_cpu_speedup="null"
    local real_time_ratio="null"
    local memory_ratio="null"

    if [[ $opencode_avg_user -gt 0 ]]; then
        user_cpu_speedup=$(awk "BEGIN {printf \"%.2f\", $opencode_avg_user / $sned_avg_user}")
    fi
    if [[ $opencode_avg_real -gt 0 ]]; then
        real_time_ratio=$(awk "BEGIN {printf \"%.2f\", $sned_avg_real / $opencode_avg_real}")
    fi
    if [[ $opencode_avg_rss -gt 0 ]]; then
        memory_ratio=$(awk "BEGIN {printf \"%.2f\", $sned_avg_rss / $opencode_avg_rss}")
    fi

    # Output JSON
    cat <<EOF
{
  "fixture": "$fixture_name",
  "iterations": $ITERATIONS,
  "provider": "$PROVIDER",
  "model": "$MODEL",
  "sned": {
    "avg_real_ms": $sned_avg_real,
    "avg_user_ms": $sned_avg_user,
    "avg_sys_ms": $sned_avg_sys,
    "avg_max_rss_kb": $sned_avg_rss
  },
  "opencode": $(if [[ ${#opencode_results[@]} -gt 0 ]]; then
    cat <<EOF2
{
    "avg_real_ms": $opencode_avg_real,
    "avg_user_ms": $opencode_avg_user,
    "avg_sys_ms": $opencode_avg_sys,
    "avg_max_rss_kb": $opencode_avg_rss
  }
EOF2
  else
    echo "null"
  fi),
  "comparison": {
    "user_cpu_speedup": $user_cpu_speedup,
    "real_time_ratio": $real_time_ratio,
    "memory_ratio": $memory_ratio
  }
}
EOF
}

# Main execution
main() {
    check_prereqs

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
        results+=("$result")
    done

    # Combine results into JSON array
    local json_output="["
    local first=true
    for result in "${results[@]}"; do
        if [[ "$first" == "true" ]]; then
            first=false
        else
            json_output+=","
        fi
        json_output+="$result"
    done
    json_output+="]"

    # Output results
    if [[ -n "$OUTPUT_FILE" ]]; then
        echo "$json_output" | jq '.' > "$OUTPUT_FILE"
        echo "Results written to: $OUTPUT_FILE"
    else
        echo "$json_output" | jq '.'
    fi
}

main
