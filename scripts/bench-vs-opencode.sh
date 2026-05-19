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
#   --keep-artifacts <dir> Preserve per-run stdout/stderr/time/workdir artifacts
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
KEEP_ARTIFACTS_DIR=""
ARTIFACT_RUN_DIR=""
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
        --keep-artifacts)
            KEEP_ARTIFACTS_DIR="$2"
            shift 2
            ;;
        --help)
            sed -n '4,26p' "$0" | sed 's/^# \?//'
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
    local run_dir="$1"
    shift

    local stdout_file stderr_file time_file exit_code=0
    local time_cmd="/usr/bin/time"
    if command -v gtime &> /dev/null; then
        time_cmd="gtime"
    fi

    mkdir -p "$run_dir"
    stdout_file="${run_dir}/stdout.txt"
    stderr_file="${run_dir}/stderr.txt"
    time_file="${run_dir}/time.txt"

    $time_cmd -v -o "$time_file" "$@" >"$stdout_file" 2>"$stderr_file" || exit_code=$?

    local stdout_bytes stderr_bytes combined_bytes final_answer_bytes final_answer_json
    stdout_bytes=$(wc -c <"$stdout_file" | tr -d ' ')
    stderr_bytes=$(wc -c <"$stderr_file" | tr -d ' ')
    combined_bytes=$((stdout_bytes + stderr_bytes))

    local final_answer final_answer_source
    final_answer_json=$(extract_final_answer "${BENCH_TOOL_KIND:-generic}" "$stdout_file" "$stderr_file")
    final_answer=$(echo "$final_answer_json" | jq -r '.text')
    final_answer_source=$(echo "$final_answer_json" | jq -r '.source')
    final_answer_bytes=$(printf '%s' "$final_answer" | wc -c | tr -d ' ')

    local inventory_json files_created file_bytes_created top_level_file_preview
    inventory_json=$(artifact_inventory)
    files_created=$(echo "$inventory_json" | jq '.files_created')
    file_bytes_created=$(echo "$inventory_json" | jq '.file_bytes_created')
    top_level_file_preview=$(echo "$inventory_json" | jq '.top_level_file_preview')

    local real_ms user_ms sys_ms max_rss_kb
    real_ms=$(grep "Elapsed (wall clock) time" "$time_file" | awk '{print $NF}' | { read -r t; parse_time_to_ms "$t"; })
    user_ms=$(awk -F: '/User time/ {printf "%.0f", $2 * 1000}' "$time_file")
    sys_ms=$(awk -F: '/System time/ {printf "%.0f", $2 * 1000}' "$time_file")
    max_rss_kb=$(awk -F: '/Maximum resident set size/ {gsub(/ /, "", $2); print $2}' "$time_file")

    local output_preview=""
    if [[ "$exit_code" -ne 0 ]]; then
        output_preview=$({ head -c 150 "$stdout_file"; printf ' '; head -c 150 "$stderr_file"; } | tr '\n' ' ' | sed 's/"/\\"/g')
    fi

    local artifacts_kept="false"
    local artifacts_dir=""
    if [[ -n "$KEEP_ARTIFACTS_DIR" ]]; then
        artifacts_kept="true"
        artifacts_dir="$run_dir"
    fi

    jq -n \
        --argjson real_ms "${real_ms:-0}" \
        --argjson user_ms "${user_ms:-0}" \
        --argjson sys_ms "${sys_ms:-0}" \
        --argjson max_rss_kb "${max_rss_kb:-0}" \
        --argjson exit_code "$exit_code" \
        --argjson artifacts_kept "$artifacts_kept" \
        --arg artifacts_dir "$artifacts_dir" \
        --argjson stdout_bytes "$stdout_bytes" \
        --argjson stderr_bytes "$stderr_bytes" \
        --argjson combined_bytes "$combined_bytes" \
        --argjson final_answer_bytes "$final_answer_bytes" \
        --arg final_answer_source "$final_answer_source" \
        --argjson files_created "$files_created" \
        --argjson file_bytes_created "$file_bytes_created" \
        --argjson top_level_file_preview "$top_level_file_preview" \
        --arg output_preview "$output_preview" \
        '{
          real_ms: $real_ms,
          user_ms: $user_ms,
          sys_ms: $sys_ms,
          max_rss_kb: $max_rss_kb,
          exit_code: $exit_code,
          artifacts_kept: $artifacts_kept,
          artifacts_dir: (if $artifacts_kept then $artifacts_dir else null end),
          stdout_bytes: $stdout_bytes,
          stderr_bytes: $stderr_bytes,
          combined_bytes: $combined_bytes,
          final_answer_bytes: $final_answer_bytes,
          final_answer_source: $final_answer_source,
          files_created: $files_created,
          file_bytes_created: $file_bytes_created,
          top_level_file_preview: $top_level_file_preview,
          output_preview: $output_preview
        }'
}

trim_file() {
    perl -0pe 's/[\r\n]+\z//' "$1"
}

extract_final_answer() {
    local tool_kind="$1"
    local stdout_file="$2"
    local stderr_file="$3"
    local text=""
    local source="empty"

    if [[ "$tool_kind" == "sned" ]]; then
        text=$(jq -rs '[.[] | select(type == "object" and .type == "completion") | (.completion.result? // .result? // empty) | select(type == "string" and length > 0)] | last // ""' "$stdout_file" 2>/dev/null || true)
        if [[ -n "$text" ]]; then
            source="sned_completion_result"
        else
            text=$(jq -rs '[.[] | select(type == "object" and .type == "text") | .text? | select(type == "string")] | join("")' "$stdout_file" 2>/dev/null || true)
            if [[ -n "$text" ]]; then
                source="sned_text_events"
            fi
        fi
    elif [[ "$tool_kind" == "opencode" ]]; then
        text=$(jq -rs '
            def render_text:
                if type == "string" then .
                elif type == "array" then [.. | strings] | join("")
                else empty
                end;
            [.[] | .. | objects | (.result?, .text?, .message?, .content?, .output?, .response?, .answer?) | select(. != null) | render_text | select(length > 0)] | last // ""
        ' "$stdout_file" 2>/dev/null || true)
        if [[ -n "$text" ]]; then
            source="opencode_json_field"
        fi
    fi

    if [[ -z "$text" ]]; then
        text=$(trim_file "$stdout_file")
        if [[ -n "$text" ]]; then
            source="stdout_trimmed"
        fi
    fi
    if [[ -z "$text" ]]; then
        text=$(trim_file "$stderr_file")
        if [[ -n "$text" ]]; then
            source="stderr_trimmed"
        fi
    fi

    jq -n --arg text "$text" --arg source "$source" '{text: $text, source: $source}'
}

file_size_bytes() {
    if stat -f %z "$1" >/dev/null 2>&1; then
        stat -f %z "$1"
    else
        stat -c %s "$1"
    fi
}

artifact_inventory() {
    local files_created=0
    local file_bytes_created=0
    local file size

    while IFS= read -r -d '' file; do
        size=$(file_size_bytes "$file")
        files_created=$((files_created + 1))
        file_bytes_created=$((file_bytes_created + size))
    done < <(
        find . -type f \
            ! -path './stdout.txt' \
            ! -path './stderr.txt' \
            ! -path './time.txt' \
            ! -path './.sned' \
            ! -path './.sned/*' \
            ! -path './.dirac' \
            ! -path './.dirac/*' \
            ! -path './sned-home-*' \
            ! -path './sned-home-*/*' \
            -print0
    )

    local top_level_file_preview
    top_level_file_preview=$(
        find . -maxdepth 1 -mindepth 1 \
            ! -name 'stdout.txt' \
            ! -name 'stderr.txt' \
            ! -name 'time.txt' \
            ! -name '.sned' \
            ! -name '.dirac' \
            ! -name 'sned-home-*' \
            -print |
            sed 's#^\./##' |
            sort |
            head -20 |
            jq -R . |
            jq -s .
    )

    jq -n \
        --argjson files_created "$files_created" \
        --argjson file_bytes_created "$file_bytes_created" \
        --argjson top_level_file_preview "$top_level_file_preview" \
        '{files_created: $files_created, file_bytes_created: $file_bytes_created, top_level_file_preview: $top_level_file_preview}'
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

run_self_tests() {
    local tmp
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/sned-bench-self-test.XXXXXX")

    printf '%s\n' '{"type":"completion","completion":{"result":"done"}}' >"${tmp}/sned-stdout.jsonl"
    : >"${tmp}/empty-stderr.txt"
    local extracted
    extracted=$(extract_final_answer sned "${tmp}/sned-stdout.jsonl" "${tmp}/empty-stderr.txt")
    _r "done" "$(echo "$extracted" | jq -r '.text')" "sned completion extraction"
    _r "sned_completion_result" "$(echo "$extracted" | jq -r '.source')" "sned completion source"

    printf '%s\n' '{"type":"text","text":"hello "}' '{"type":"text","text":"world"}' >"${tmp}/sned-text.jsonl"
    extracted=$(extract_final_answer sned "${tmp}/sned-text.jsonl" "${tmp}/empty-stderr.txt")
    _r "hello world" "$(echo "$extracted" | jq -r '.text')" "sned text extraction"
    _r "sned_text_events" "$(echo "$extracted" | jq -r '.source')" "sned text source"

    printf '%s\n' '{"message":{"content":[{"text":"nested answer"}]}}' >"${tmp}/opencode-stdout.jsonl"
    extracted=$(extract_final_answer opencode "${tmp}/opencode-stdout.jsonl" "${tmp}/empty-stderr.txt")
    _r "nested answer" "$(echo "$extracted" | jq -r '.text')" "opencode json extraction"
    _r "opencode_json_field" "$(echo "$extracted" | jq -r '.source')" "opencode json source"

    mkdir -p "${tmp}/inventory/.sned"
    printf 'abc' >"${tmp}/inventory/result.rs"
    printf 'ignore' >"${tmp}/inventory/stdout.txt"
    printf 'state' >"${tmp}/inventory/.sned/state.json"
    local inventory
    inventory=$(cd "${tmp}/inventory" && artifact_inventory)
    _r "1" "$(echo "$inventory" | jq -r '.files_created')" "artifact file count"
    _r "3" "$(echo "$inventory" | jq -r '.file_bytes_created')" "artifact byte count"

    rm -rf "$tmp"
}

if [[ "${BENCH_VS_OPENCODE_SELF_TEST:-}" == "1" ]]; then
    run_self_tests
    exit 0
fi

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
    if [[ -n "$KEEP_ARTIFACTS_DIR" ]]; then
        fixture_tmp="${ARTIFACT_RUN_DIR}/${fixture_name}"
        mkdir -p "$fixture_tmp"
    else
        fixture_tmp=$(mktemp -d "${TMPDIR:-/tmp}/sned-opencode-bench-${fixture_name}.XXXXXX")
    fi
    local prompt
    prompt=$(<"$prompt_file")

    echo "Running fixture: $fixture_name ($ITERATIONS iterations)" >&2

    local sned_bin="${SNED_DIR}/target/release/sned"
    if [[ ! -x "$sned_bin" ]]; then
        echo "Error: sned binary not found at $sned_bin. Run: cargo build --release" >&2
        rm -f "$prompt_file"
        if [[ -z "$KEEP_ARTIFACTS_DIR" ]]; then
            rm -rf "$fixture_tmp"
        fi
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
        result=$(cd "$sned_workdir" && export SNED_DIR="$sned_home" && BENCH_TOOL_KIND=sned measure_command "$sned_workdir" "$sned_bin" --provider "$PROVIDER" --model "$MODEL" --yolo --json --no-token-display "$prompt")
        local rc
        rc=$(echo "$result" | jq '.exit_code')
        if [[ "$rc" -ne 0 ]]; then
            local preview
            preview=$(echo "$result" | jq -r '.output_preview')
            echo "  sned iteration $i failed (exit $rc): $preview" >&2
            rm -f "$prompt_file"
            if [[ -z "$KEEP_ARTIFACTS_DIR" ]]; then
                rm -rf "$fixture_tmp"
            fi
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
            result=$(cd "$opencode_workdir" && BENCH_TOOL_KIND=opencode measure_command "$opencode_workdir" "$opencode_bin" run --format json --dangerously-skip-permissions -m "$OPENCODE_MODEL" "$prompt")
            local rc
            rc=$(echo "$result" | jq '.exit_code')
            if [[ "$rc" -ne 0 ]]; then
                local preview
                preview=$(echo "$result" | jq -r '.output_preview')
                echo "  opencode iteration $i failed (exit $rc): $preview" >&2
                rm -f "$prompt_file"
                if [[ -z "$KEEP_ARTIFACTS_DIR" ]]; then
                    rm -rf "$fixture_tmp"
                fi
                return 1
            fi
            opencode_results+=("$result")
        done
    else
        echo "  Skipping opencode (not installed)" >&2
    fi

    rm -f "$prompt_file"
    if [[ -z "$KEEP_ARTIFACTS_DIR" ]]; then
        rm -rf "$fixture_tmp"
    fi

    local sned_avg_real=0 sned_avg_user=0 sned_avg_sys=0 sned_avg_rss=0
    local sned_avg_stdout_bytes=0 sned_avg_stderr_bytes=0 sned_avg_combined_bytes=0 sned_avg_final_answer_bytes=0
    local sned_avg_files_created=0 sned_avg_file_bytes_created=0
    local opencode_avg_real=0 opencode_avg_user=0 opencode_avg_sys=0 opencode_avg_rss=0
    local opencode_avg_stdout_bytes=0 opencode_avg_stderr_bytes=0 opencode_avg_combined_bytes=0 opencode_avg_final_answer_bytes=0
    local opencode_avg_files_created=0 opencode_avg_file_bytes_created=0

    for result in "${sned_results[@]}"; do
        sned_avg_real=$((sned_avg_real + $(echo "$result" | jq '.real_ms')))
        sned_avg_user=$((sned_avg_user + $(echo "$result" | jq '.user_ms')))
        sned_avg_sys=$((sned_avg_sys + $(echo "$result" | jq '.sys_ms')))
        sned_avg_rss=$((sned_avg_rss + $(echo "$result" | jq '.max_rss_kb')))
        sned_avg_stdout_bytes=$((sned_avg_stdout_bytes + $(echo "$result" | jq '.stdout_bytes')))
        sned_avg_stderr_bytes=$((sned_avg_stderr_bytes + $(echo "$result" | jq '.stderr_bytes')))
        sned_avg_combined_bytes=$((sned_avg_combined_bytes + $(echo "$result" | jq '.combined_bytes')))
        sned_avg_final_answer_bytes=$((sned_avg_final_answer_bytes + $(echo "$result" | jq '.final_answer_bytes')))
        sned_avg_files_created=$((sned_avg_files_created + $(echo "$result" | jq '.files_created')))
        sned_avg_file_bytes_created=$((sned_avg_file_bytes_created + $(echo "$result" | jq '.file_bytes_created')))
    done
    sned_avg_real=$((sned_avg_real / ITERATIONS))
    sned_avg_user=$((sned_avg_user / ITERATIONS))
    sned_avg_sys=$((sned_avg_sys / ITERATIONS))
    sned_avg_rss=$((sned_avg_rss / ITERATIONS))
    sned_avg_stdout_bytes=$((sned_avg_stdout_bytes / ITERATIONS))
    sned_avg_stderr_bytes=$((sned_avg_stderr_bytes / ITERATIONS))
    sned_avg_combined_bytes=$((sned_avg_combined_bytes / ITERATIONS))
    sned_avg_final_answer_bytes=$((sned_avg_final_answer_bytes / ITERATIONS))
    sned_avg_files_created=$((sned_avg_files_created / ITERATIONS))
    sned_avg_file_bytes_created=$((sned_avg_file_bytes_created / ITERATIONS))

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
            opencode_avg_files_created=$((opencode_avg_files_created + $(echo "$result" | jq '.files_created')))
            opencode_avg_file_bytes_created=$((opencode_avg_file_bytes_created + $(echo "$result" | jq '.file_bytes_created')))
        done
        opencode_avg_real=$((opencode_avg_real / ITERATIONS))
        opencode_avg_user=$((opencode_avg_user / ITERATIONS))
        opencode_avg_sys=$((opencode_avg_sys / ITERATIONS))
        opencode_avg_rss=$((opencode_avg_rss / ITERATIONS))
        opencode_avg_stdout_bytes=$((opencode_avg_stdout_bytes / ITERATIONS))
        opencode_avg_stderr_bytes=$((opencode_avg_stderr_bytes / ITERATIONS))
        opencode_avg_combined_bytes=$((opencode_avg_combined_bytes / ITERATIONS))
        opencode_avg_final_answer_bytes=$((opencode_avg_final_answer_bytes / ITERATIONS))
        opencode_avg_files_created=$((opencode_avg_files_created / ITERATIONS))
        opencode_avg_file_bytes_created=$((opencode_avg_file_bytes_created / ITERATIONS))
    fi

    local sned_runs sned_final_answer_sources opencode_runs opencode_final_answer_sources
    sned_runs=$(printf '%s\n' "${sned_results[@]}" | jq -s '.')
    sned_final_answer_sources=$(printf '%s\n' "${sned_results[@]}" | jq -s '[.[].final_answer_source]')
    opencode_runs='[]'
    opencode_final_answer_sources='[]'
    if [[ ${#opencode_results[@]} -gt 0 ]]; then
        opencode_runs=$(printf '%s\n' "${opencode_results[@]}" | jq -s '.')
        opencode_final_answer_sources=$(printf '%s\n' "${opencode_results[@]}" | jq -s '[.[].final_answer_source]')
    fi

    local artifacts_kept="false"
    if [[ -n "$KEEP_ARTIFACTS_DIR" ]]; then
        artifacts_kept="true"
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
            --argjson sned_avg_files_created "$sned_avg_files_created" \
            --argjson sned_avg_file_bytes_created "$sned_avg_file_bytes_created" \
            --argjson sned_runs "$sned_runs" \
            --argjson sned_final_answer_sources "$sned_final_answer_sources" \
            --argjson opencode_avg_real "$opencode_avg_real" \
            --argjson opencode_avg_user "$opencode_avg_user" \
            --argjson opencode_avg_sys "$opencode_avg_sys" \
            --argjson opencode_avg_rss "$opencode_avg_rss" \
            --argjson opencode_avg_stdout_bytes "$opencode_avg_stdout_bytes" \
            --argjson opencode_avg_stderr_bytes "$opencode_avg_stderr_bytes" \
            --argjson opencode_avg_combined_bytes "$opencode_avg_combined_bytes" \
            --argjson opencode_avg_final_answer_bytes "$opencode_avg_final_answer_bytes" \
            --argjson opencode_avg_files_created "$opencode_avg_files_created" \
            --argjson opencode_avg_file_bytes_created "$opencode_avg_file_bytes_created" \
            --argjson opencode_runs "$opencode_runs" \
            --argjson opencode_final_answer_sources "$opencode_final_answer_sources" \
            --argjson artifacts_kept "$artifacts_kept" \
            --arg artifacts_dir "$fixture_tmp" \
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
                artifacts_kept: $artifacts_kept,
                artifacts_dir: (if $artifacts_kept then $artifacts_dir else null end),
                avg_real_ms: $sned_avg_real,
                avg_user_ms: $sned_avg_user,
                avg_sys_ms: $sned_avg_sys,
                avg_max_rss_kb: $sned_avg_rss,
                avg_stdout_bytes: $sned_avg_stdout_bytes,
                avg_stderr_bytes: $sned_avg_stderr_bytes,
                avg_combined_bytes: $sned_avg_combined_bytes,
                avg_final_answer_bytes: $sned_avg_final_answer_bytes,
                avg_files_created: $sned_avg_files_created,
                avg_file_bytes_created: $sned_avg_file_bytes_created,
                final_answer_source: (if ($sned_final_answer_sources | unique | length) == 1 then $sned_final_answer_sources[0] else "mixed" end),
                final_answer_sources: $sned_final_answer_sources,
                runs: $sned_runs
              },
              opencode: {
                artifacts_kept: $artifacts_kept,
                artifacts_dir: (if $artifacts_kept then $artifacts_dir else null end),
                avg_real_ms: $opencode_avg_real,
                avg_user_ms: $opencode_avg_user,
                avg_sys_ms: $opencode_avg_sys,
                avg_max_rss_kb: $opencode_avg_rss,
                avg_stdout_bytes: $opencode_avg_stdout_bytes,
                avg_stderr_bytes: $opencode_avg_stderr_bytes,
                avg_combined_bytes: $opencode_avg_combined_bytes,
                avg_final_answer_bytes: $opencode_avg_final_answer_bytes,
                avg_files_created: $opencode_avg_files_created,
                avg_file_bytes_created: $opencode_avg_file_bytes_created,
                final_answer_source: (if ($opencode_final_answer_sources | unique | length) == 1 then $opencode_final_answer_sources[0] else "mixed" end),
                final_answer_sources: $opencode_final_answer_sources,
                runs: $opencode_runs
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
            --argjson sned_avg_files_created "$sned_avg_files_created" \
            --argjson sned_avg_file_bytes_created "$sned_avg_file_bytes_created" \
            --argjson sned_runs "$sned_runs" \
            --argjson sned_final_answer_sources "$sned_final_answer_sources" \
            --argjson artifacts_kept "$artifacts_kept" \
            --arg artifacts_dir "$fixture_tmp" \
            '{
              fixture: $fixture,
              iterations: $iterations,
              provider: $provider,
              model: $model,
              opencode_model: $opencode_model,
              sned: {
                artifacts_kept: $artifacts_kept,
                artifacts_dir: (if $artifacts_kept then $artifacts_dir else null end),
                avg_real_ms: $sned_avg_real,
                avg_user_ms: $sned_avg_user,
                avg_sys_ms: $sned_avg_sys,
                avg_max_rss_kb: $sned_avg_rss,
                avg_stdout_bytes: $sned_avg_stdout_bytes,
                avg_stderr_bytes: $sned_avg_stderr_bytes,
                avg_combined_bytes: $sned_avg_combined_bytes,
                avg_final_answer_bytes: $sned_avg_final_answer_bytes,
                avg_files_created: $sned_avg_files_created,
                avg_file_bytes_created: $sned_avg_file_bytes_created,
                final_answer_source: (if ($sned_final_answer_sources | unique | length) == 1 then $sned_final_answer_sources[0] else "mixed" end),
                final_answer_sources: $sned_final_answer_sources,
                runs: $sned_runs
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
    if [[ -n "$KEEP_ARTIFACTS_DIR" ]]; then
        mkdir -p "$KEEP_ARTIFACTS_DIR"
        local artifacts_root
        artifacts_root=$(cd "$KEEP_ARTIFACTS_DIR" && pwd)
        ARTIFACT_RUN_DIR="${artifacts_root}/$(date -u +%Y%m%dT%H%M%SZ)"
        mkdir -p "$ARTIFACT_RUN_DIR"
        echo "Keeping benchmark artifacts under: $ARTIFACT_RUN_DIR" >&2
    fi

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
