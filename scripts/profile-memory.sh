#!/usr/bin/env bash
# Automated Memory Profiling for sned
# 
# This script:
# 1. Builds sned with dhat-heap feature
# 2. Runs a realistic workload (or custom command)
# 3. Analyzes heap allocations with categorization
# 4. Generates a comprehensive report
#
# Usage: ./profile-memory.sh [options]
#   --workload <name>   Workload to run: basic, edit, search, all (default: basic)
#   --output <dir>      Output directory for reports (default: ./target/memory-profiles)
#   --keep-json         Keep raw dhat-heap.json files (default: clean up)
#   --base-url <url>    Provider base URL (default: $SNED_PROFILE_BASE_URL or Salad gateway)
#   --model <id>        Model for agent workloads (default: $SNED_PROFILE_MODEL or qwen3.5-35b-a3b)
#   --api-key <key>     Provider API key (default: $SNED_PROFILE_API_KEY or $SALAD_CLOUD_API_KEY)
#   --timeout <secs>    Per-workload timeout for agent runs (default: 600)
#   --help              Show this help

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
cd "$REPO_ROOT"

# Defaults
WORKLOAD="basic"
OUTPUT_DIR="./target/memory-profiles"
KEEP_JSON=false
TIMESTAMP=$(date +%Y%m%d-%H%M%S)
# Agent workloads need a funded provider; default to the Salad gateway.
PROFILE_BASE_URL="${SNED_PROFILE_BASE_URL:-https://ai.salad.cloud/v1}"
PROFILE_MODEL="${SNED_PROFILE_MODEL:-qwen3.5-35b-a3b}"
PROFILE_API_KEY="${SNED_PROFILE_API_KEY:-${SALAD_CLOUD_API_KEY:-}}"
# Agent turns are the signal (cross-turn accumulation needs a long-lived
# process), so the timeout must let the loop finish and exit cleanly;
# a kill leaves no dhat output at all.
PROFILE_TIMEOUT="${SNED_PROFILE_TIMEOUT:-600}"

# Colors
RED='\033[0;31m'
YELLOW='\033[1;33m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

show_help() {
    cat << 'EOF'
Automated Memory Profiling for sned

Usage: ./profile-memory.sh [options]

Options:
  --workload <name>   Workload to run: basic, edit, search, all (default: basic)
  --output <dir>      Output directory for reports (default: ./target/memory-profiles)
  --keep-json         Keep raw dhat-heap.json files (default: clean up)
  --base-url <url>    Provider base URL (default: Salad gateway)
  --model <id>        Model for agent workloads (default: qwen3.5-35b-a3b)
  --api-key <key>     Provider API key (default: $SALAD_CLOUD_API_KEY)
  --timeout <secs>    Per-workload timeout for agent runs (default: 600)
  --help              Show this help

Workloads:
  basic    Simple command parsing and initialization
  edit     Multi-step file editing with anchor reconciliation (several turns)
  search   Multi-step file search and symbol indexing (several turns)
  all      Run all workloads sequentially

Examples:
  ./profile-memory.sh
  ./profile-memory.sh --workload edit
  ./profile-memory.sh --workload all --keep-json

Output:
  Reports saved to: <output-dir>/profile-<timestamp>/
  - summary.txt     Human-readable summary
  - allocations.txt Categorized allocation breakdown
  - dhat-heap.json  Raw dhat output (if --keep-json)
EOF
}

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --workload)
            WORKLOAD="$2"
            shift 2
            ;;
        --output)
            OUTPUT_DIR="$2"
            shift 2
            ;;
        --keep-json)
            KEEP_JSON=true
            shift
            ;;
        --base-url)
            PROFILE_BASE_URL="$2"
            shift 2
            ;;
        --model)
            PROFILE_MODEL="$2"
            shift 2
            ;;
        --api-key)
            PROFILE_API_KEY="$2"
            shift 2
            ;;
        --timeout)
            PROFILE_TIMEOUT="$2"
            shift 2
            ;;
        --help)
            show_help
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            show_help
            exit 1
            ;;
    esac
done

# Validate workload
case $WORKLOAD in
    basic|edit|search|all)
        ;;
    *)
        echo -e "${RED}Error: Unknown workload '$WORKLOAD'${NC}"
        echo "Valid options: basic, edit, search, all"
        exit 1
        ;;
esac

# Agent workloads need a funded provider key.
case $WORKLOAD in
    edit|search|all)
        if [ -z "$PROFILE_API_KEY" ]; then
            echo -e "${RED}Error: no provider API key for the '$WORKLOAD' workload${NC}"
            echo "Set SALAD_CLOUD_API_KEY (or SNED_PROFILE_API_KEY), or pass --api-key <key>"
            exit 1
        fi
        ;;
esac

# A non-numeric timeout would fail obscurely inside timeout(1).
case $PROFILE_TIMEOUT in
    ''|*[!0-9]*)
        echo -e "${RED}Error: --timeout must be seconds as a number, got '$PROFILE_TIMEOUT'${NC}"
        exit 1
        ;;
esac

# Setup output directory
RUN_DIR="$OUTPUT_DIR/profile-${TIMESTAMP}"
mkdir -p "$RUN_DIR"

echo "=============================================="
echo "  sned Memory Profiling"
echo "=============================================="
echo ""
echo "  Workload:     $WORKLOAD"
echo "  Output Dir:   $RUN_DIR"
echo "  Timestamp:    $TIMESTAMP"
echo ""

# Check for dhat-heap feature in Cargo.toml
if ! grep -q 'dhat-heap' Cargo.toml; then
    echo -e "${RED}Error: dhat-heap feature not found in Cargo.toml${NC}"
    echo "Add this to Cargo.toml:"
    echo "  dhat = { version = \"0.3\", optional = true }"
    exit 1
fi

# Build with dhat-heap
echo -e "${BLUE}[1/4] Building with dhat-heap feature...${NC}"
BUILD_LOG="$RUN_DIR/build.log"
mkdir -p "$RUN_DIR"  # Ensure directory exists before tee
# The release profile strips symbols, which leaves dhat frames unattributed
# (every site reports as __mh_execute_header); keep debug info so the heap
# report names the allocating functions without changing optimization.
if cargo build --features dhat-heap --release \
    --config 'profile.release.strip=false' \
    --config 'profile.release.debug=1' > "$BUILD_LOG" 2>&1; then
    echo -e "${GREEN}✓ Build complete${NC}"
else
    echo -e "${RED}Build failed! Check $BUILD_LOG${NC}"
    exit 1
fi

echo -e "${GREEN}✓ Build complete${NC}"
echo ""

# Run workload
echo -e "${BLUE}[2/4] Running workload: $WORKLOAD${NC}"
echo ""

run_basic_workload() {
    # `config list` exercises startup plus config loading and returns
    # normally; --help/--version exit inside clap, skipping dhat output.
    echo "  Running: config list (initialization only)"
    ./target/release/sned config list > /dev/null 2>&1 || true
    collect_dhat basic
}

# dhat writes dhat-heap.json to the process CWD at clean exit only: a
# timeout kill or crash leaves nothing. Capture each workload's output
# immediately so one bad run cannot wipe out the others.
# The edit workload runs with --cwd in a temp workspace, so its profile
# lands there; an optional second argument names that directory.
collect_dhat() {
    local name="$1"
    local src_dir="${2:-.}"
    if [ -f "$src_dir/dhat-heap.json" ]; then
        mv "$src_dir/dhat-heap.json" "$RUN_DIR/dhat-${name}.json"
        echo -e "${GREEN}  ✓ Captured heap profile for '$name'${NC}"
    else
        echo -e "${YELLOW}  ⚠ No heap profile for '$name' (killed, crashed, or instant exit)${NC}"
    fi
}

run_edit_workload() {
    echo "  Running: File editing simulation"
    
    # Create temp workspace
    local temp_workspace=$(mktemp -d)
    local test_file="$temp_workspace/test_edit.txt"
    
    # Create test file
    cat > "$test_file" << 'TESTFILE'
Line 1: This is a test file for memory profiling
Line 2: It contains multiple lines of text
Line 3: To simulate realistic editing operations
Line 4: The anchor system will hash each line
Line 5: And track changes for incremental edits
Line 6: Extra content so later turns have more to reconcile
Line 7: Final line of the fixture
TESTFILE

    # Run edit command against the profile provider (--yolo so approval
    # prompts cannot stall the workload; the temp workspace is disposable).
    # --cwd keeps the process in the repo root so dhat-heap.json lands where
    # collect_dhat expects it instead of the deleted temp workspace.
    # Multi-step prompt drives several agent turns in one process, which is
    # what exposes cross-turn accumulation in the heap profile.
    timeout "${PROFILE_TIMEOUT}s" "$REPO_ROOT/target/release/sned" --yolo \
        --cwd "$temp_workspace" \
        --base-url "$PROFILE_BASE_URL" \
        --model "$PROFILE_MODEL" \
        --api-key "$PROFILE_API_KEY" \
        "In test_edit.txt: (1) change line 3 to say 'MODIFIED LINE 3', (2) append a new line 8 saying 'APPENDED LINE 8', (3) change line 1 to say 'MODIFIED LINE 1'. Then report the final contents." \
        2>&1 || true
    collect_dhat edit "$temp_workspace"

    # Cleanup
    rm -rf "$temp_workspace"
}

run_search_workload() {
    echo "  Running: File search and symbol indexing"

    # Run search command against the profile provider (--yolo so approval
    # prompts cannot stall the workload; the prompt is read-only).
    # Multi-step prompt drives several agent turns in one process, which is
    # what exposes cross-turn accumulation in the heap profile.
    timeout "${PROFILE_TIMEOUT}s" ./target/release/sned --yolo \
        --base-url "$PROFILE_BASE_URL" \
        --model "$PROFILE_MODEL" \
        --api-key "$PROFILE_API_KEY" \
        "Do these in order, reporting each result: (1) list all Rust files under src/storage, (2) find where persist_full_global_state is defined, (3) find all callers of persist_global_state. Then summarize." \
        2>&1 || true
    collect_dhat search
}

case $WORKLOAD in
    basic)
        run_basic_workload
        ;;
    edit)
        run_edit_workload
        ;;
    search)
        run_search_workload
        ;;
    all)
        echo "  Running all workloads sequentially..."
        echo ""
        echo "  === Basic Workload ==="
        run_basic_workload
        echo ""
        echo "  === Edit Workload ==="
        run_edit_workload
        echo ""
        echo "  === Search Workload ==="
        run_search_workload
        ;;
esac

echo ""
echo -e "${GREEN}✓ Workload complete${NC}"
echo ""

# Check for captured dhat output (one file per workload)
shopt -s nullglob
Dhat_FILES=("$RUN_DIR"/dhat-*.json)
shopt -u nullglob
if [ ${#Dhat_FILES[@]} -eq 0 ]; then
    echo -e "${YELLOW}Warning: no heap profiles captured${NC}"
    echo "No workload exited cleanly with dhat output."
    echo "Creating empty report..."

        cat > "$RUN_DIR/summary.txt" << EOF
Memory Profile Summary
======================
Timestamp: $TIMESTAMP
Workload: $WORKLOAD
Status: No allocations recorded

No dhat output was captured. This could mean:
1. A workload was killed by timeout (dhat only writes at clean exit)
2. A workload crashed before exit
3. dhat was not properly initialized

Re-run the failing workload on its own and check its exit status.
EOF
    exit 0
fi

echo -e "${GREEN}Captured ${#Dhat_FILES[@]} heap profile(s)${NC}"
echo ""

# Analyze with Python script
echo -e "${BLUE}[3/4] Analyzing heap allocations...${NC}"
for WL_JSON in "${Dhat_FILES[@]}"; do
    WL_NAME="$(basename "$WL_JSON" .json)"
    WL_NAME="${WL_NAME#dhat-}"
    if "$SCRIPT_DIR/analyze-dhat-heap.sh" "$WL_JSON" > "$RUN_DIR/allocations-${WL_NAME}.txt" 2>&1; then
        echo -e "${GREEN}✓ Analysis complete ($WL_NAME)${NC}"
    else
        echo -e "${YELLOW}⚠ Analysis had warnings for $WL_NAME (see allocations-${WL_NAME}.txt)${NC}"
    fi
done
echo ""

# Generate summary report
echo -e "${BLUE}[4/4] Generating summary report...${NC}"

# Extract key metrics from each captured dhat JSON
for WL_JSON in "${Dhat_FILES[@]}"; do
    WL_NAME="$(basename "$WL_JSON" .json)"
    WL_NAME="${WL_NAME#dhat-}"
    WL_LABEL="$WL_NAME"
    if [ "$WORKLOAD" = "all" ]; then
        WL_LABEL="all/$WL_NAME"
    fi
python3 << PYTHON_SCRIPT > "$RUN_DIR/summary-${WL_NAME}.txt"
import json
import sys
from datetime import datetime

with open("$WL_JSON", 'r') as f:
    data = json.load(f)

pps = data.get('pps', [])
ftbl = data.get('ftbl', [])

# Calculate metrics
total_allocated = sum(p['tb'] for p in pps)
total_freed = sum(p['tb'] - p['gb'] - p['eb'] for p in pps)
final_live = sum(p['gb'] + p['eb'] for p in pps)
allocation_count = sum(p['tbk'] for p in pps)
free_count = sum(p['tbk'] - p['gbk'] - p['ebk'] for p in pps)

def format_bytes(b):
    if b >= 1073741824:
        return f"{b / 1073741824:.2f} GB"
    elif b >= 1048576:
        return f"{b / 1048576:.2f} MB"
    elif b >= 1024:
        return f"{b / 1024:.2f} KB"
    return f"{b} B"

# Same shim-skipping rule as analyze-dhat-heap.py: the leaf frame is
# always an allocator, so top sites must name the caller beneath it.
ALLOCATOR_FRAME_MARKERS = [
    '<dhat::alloc', 'dhat::alloc', 'core::alloc::', 'alloc::alloc::',
    'alloc::raw_vec::', 'alloc::vec::', 'alloc::slice::', 'alloc::boxed::',
    'alloc::string::', 'globalalloc', 'exchange_malloc', '__rust_alloc',
]

def attributing_frame(frames):
    for idx in frames:
        frame_str = ftbl[idx] if idx < len(ftbl) else ""
        if not any(m in frame_str.lower() for m in ALLOCATOR_FRAME_MARKERS):
            return frame_str
    if frames:
        idx = frames[0]
        return ftbl[idx] if idx < len(ftbl) else ""
    return ""

leak_ratio = (final_live / total_allocated * 100) if total_allocated > 0 else 0

# Find top 5 allocations by final_live
sorted_pps = sorted(enumerate(pps), key=lambda x: x[1].get('gb', 0) + x[1].get('eb', 0), reverse=True)

print("=" * 70)
print("  sned Memory Profile Summary")
print("=" * 70)
print()
print(f"  Timestamp:    $TIMESTAMP")
print(f"  Workload:     $WL_LABEL")
print(f"  Report Dir:   $RUN_DIR")
print()
print("📊 Overall Statistics")
print("-" * 70)
print(f"  Total Allocated:    {format_bytes(total_allocated)}")
print(f"  Total Freed:        {format_bytes(total_freed)}")
print(f"  Final Live:         {format_bytes(final_live)}")
print(f"  Leak Ratio:         {leak_ratio:.2f}%")
print(f"  Allocation Count:   {allocation_count:,}")
print(f"  Free Count:         {free_count:,}")
print(f"  Avg Alloc Size:     {format_bytes(total_allocated // allocation_count) if allocation_count > 0 else 'N/A'}")
print()

print("📈 Top 5 Allocations by Final Live Bytes")
print("-" * 70)

for i, (idx, p) in enumerate(sorted_pps[:5]):
    final = p.get('gb', 0) + p.get('eb', 0)
    if final == 0:
        continue

    frame_str = attributing_frame(p.get('fs', [])) or "unknown"
    func_name = frame_str.split(': ', 1)[1].split(' (')[0] if ': ' in frame_str else frame_str
    if len(func_name) > 60:
        func_name = func_name[:57] + "..."

    print(f"  {i+1}. {format_bytes(final):>12}  {func_name}")

print()

# Categorize allocations
categories = {'std_lib': 0, 'profiler': 0, 'runtime': 0, 'application': 0}
for p in pps:
    frame_str = attributing_frame(p.get('fs', []))
    frame_lower = frame_str.lower()

    final = p.get('gb', 0) + p.get('eb', 0)

    if 'dhat::' in frame_lower or '<dhat' in frame_lower:
        categories['profiler'] += final
    elif 'sned::' in frame_lower or 'sned-' in frame_lower:
        categories['application'] += final
    elif any(x in frame_lower for x in ['<alloc::', 'alloc::alloc::global', 'alloc::boxed', 'box_assume_init', 'raw_vec']):
        categories['std_lib'] += final
    elif any(x in frame_lower for x in ['tokio', 'regex', 'tracing', 'serde_json', 'hyper', 'reqwest', 'mio']):
        categories['runtime'] += final
    else:
        categories['application'] += final

print("📦 Allocation Categories (Final Live)")
print("-" * 70)
print(f"  Standard Library:   {format_bytes(categories['std_lib']):>12}  (Rust allocator - NOT leaks)")
print(f"  Profiler Overhead:  {format_bytes(categories['profiler']):>12}  (dhat - NOT leaks)")
print(f"  Runtime:            {format_bytes(categories['runtime']):>12}  (tokio/serde - expected)")
print(f"  Application:        {format_bytes(categories['application']):>12}")
print()
print("=" * 70)
print(f"  Full analysis: allocations-$WL_NAME.txt")
print(f"  Raw data:      dhat-$WL_NAME.json (if --keep-json)")
print("=" * 70)
PYTHON_SCRIPT
done

echo -e "${GREEN}✓ Summary report(s) generated${NC}"
echo ""

# Show summaries
echo "=============================================="
echo "  Summaries"
echo "=============================================="
echo ""
for WL_JSON in "${Dhat_FILES[@]}"; do
    WL_NAME="$(basename "$WL_JSON" .json)"
    WL_NAME="${WL_NAME#dhat-}"
    cat "$RUN_DIR/summary-${WL_NAME}.txt"
    echo ""
done

# Cleanup
if [ "$KEEP_JSON" = false ]; then
    rm -f "$RUN_DIR"/dhat-*.json
    echo -e "${BLUE}Cleaned up raw dhat JSON (use --keep-json to retain)${NC}"
fi

echo ""
echo "=============================================="
echo "  Profile Complete"
echo "=============================================="
echo ""
echo "  Reports saved to: $RUN_DIR/"
for WL_JSON in "${Dhat_FILES[@]}"; do
    WL_NAME="$(basename "$WL_JSON" .json)"
    WL_NAME="${WL_NAME#dhat-}"
    echo "    - summary-${WL_NAME}.txt / allocations-${WL_NAME}.txt"
    if [ "$KEEP_JSON" = true ]; then
        echo "    - dhat-${WL_NAME}.json (raw data)"
    fi
done
echo ""
echo "  To view interactive dhat report:"
echo "    1. Run with --keep-json"
echo "    2. Open https://nnethercote.github.io/dhat-viewer/"
echo "    3. Upload a dhat-<workload>.json file from $RUN_DIR/"
echo ""
