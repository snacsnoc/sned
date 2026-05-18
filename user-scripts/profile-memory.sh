#!/usr/bin/env bash
# Automated Memory Profiling for Sned Native
# 
# This script:
# 1. Builds sned-native with dhat-heap feature
# 2. Runs a realistic workload (or custom command)
# 3. Analyzes heap allocations with categorization
# 4. Generates a comprehensive report
#
# Usage: ./profile-memory.sh [options]
#   --workload <name>   Workload to run: basic, edit, search, all (default: basic)
#   --output <dir>      Output directory for reports (default: ./target/memory-profiles)
#   --keep-json         Keep raw dhat-heap.json files (default: clean up)
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

# Colors
RED='\033[0;31m'
YELLOW='\033[1;33m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

show_help() {
    cat << 'EOF'
Automated Memory Profiling for Sned Native

Usage: ./profile-memory.sh [options]

Options:
  --workload <name>   Workload to run: basic, edit, search, all (default: basic)
  --output <dir>      Output directory for reports (default: ./target/memory-profiles)
  --keep-json         Keep raw dhat-heap.json files (default: clean up)
  --help              Show this help

Workloads:
  basic    Simple command parsing and initialization
  edit     File editing operations with anchor reconciliation
  search   File search and symbol indexing
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

# Setup output directory
RUN_DIR="$OUTPUT_DIR/profile-${TIMESTAMP}"
mkdir -p "$RUN_DIR"

echo "=============================================="
echo "  Sned Native Memory Profiling"
echo "=============================================="
echo ""
echo "  Workload:     $WORKLOAD"
echo "  Output Dir:   $RUN_DIR"
echo "  Timestamp:    $TIMESTAMP"
echo ""

# Check for dhat-heap feature in Cargo.toml
if ! grep -q 'dhat-heap' sned-native/Cargo.toml; then
    echo -e "${RED}Error: dhat-heap feature not found in sned-native/Cargo.toml${NC}"
    echo "Add this to Cargo.toml:"
    echo "  dhat = { version = \"0.3\", optional = true }"
    exit 1
fi

# Build with dhat-heap
echo -e "${BLUE}[1/4] Building with dhat-heap feature...${NC}"
cd sned-native
BUILD_LOG="$RUN_DIR/build.log"
mkdir -p "$RUN_DIR"  # Ensure directory exists before tee
if cargo build --features dhat-heap --release > "$BUILD_LOG" 2>&1; then
    echo -e "${GREEN}✓ Build complete${NC}"
else
    echo -e "${RED}Build failed! Check $BUILD_LOG${NC}"
    cd ..
    exit 1
fi
cd ..

echo -e "${GREEN}✓ Build complete${NC}"
echo ""

# Run workload
echo -e "${BLUE}[2/4] Running workload: $WORKLOAD${NC}"
echo ""

run_basic_workload() {
    echo "  Running: --help (initialization only)"
    ./sned-native/target/release/sned-native --help > /dev/null 2>&1 || true
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
TESTFILE

    # Run edit command (will fail but exercises the code paths)
    cd "$temp_workspace"
    timeout 10s "$REPO_ROOT/sned-native/target/release/sned-native" \
        "Edit line 3 to say 'MODIFIED LINE 3'" \
        2>&1 || true
    cd "$REPO_ROOT"
    
    # Cleanup
    rm -rf "$temp_workspace"
}

run_search_workload() {
    echo "  Running: File search and symbol indexing"
    
    # Run search command (will fail but exercises code paths)
    timeout 10s ./sned-native/target/release/sned-native \
        "Search for all Rust files in this project" \
        2>&1 || true
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

# Check for dhat output
Dhat_JSON="dhat-heap.json"
if [ ! -f "$Dhat_JSON" ]; then
    # Try alternate location
    if [ -f "sned-native/$Dhat_JSON" ]; then
        Dhat_JSON="sned-native/$Dhat_JSON"
    else
        echo -e "${YELLOW}Warning: dhat-heap.json not found${NC}"
        echo "The workload may not have triggered heap allocations."
        echo "Creating empty report..."
        
        cat > "$RUN_DIR/summary.txt" << EOF
Memory Profile Summary
======================
Timestamp: $TIMESTAMP
Workload: $WORKLOAD
Status: No allocations recorded

dhat-heap.json was not generated. This could mean:
1. The workload completed without heap allocations
2. dhat was not properly initialized
3. The program exited before dhat could write output

Try running with --workload all for more comprehensive profiling.
EOF
        exit 0
    fi
fi

# Copy dhat output
if [ "$KEEP_JSON" = true ]; then
    cp "$Dhat_JSON" "$RUN_DIR/dhat-heap.json"
    echo -e "${GREEN}✓ Saved dhat-heap.json${NC}"
fi

# Analyze with Python script
echo -e "${BLUE}[3/4] Analyzing heap allocations...${NC}"
if "$SCRIPT_DIR/analyze-dhat-heap.sh" "$Dhat_JSON" > "$RUN_DIR/allocations.txt" 2>&1; then
    echo -e "${GREEN}✓ Analysis complete${NC}"
else
    echo -e "${YELLOW}⚠ Analysis had warnings (see allocations.txt)${NC}"
fi
echo ""

# Generate summary report
echo -e "${BLUE}[4/4] Generating summary report...${NC}"

# Extract key metrics from dhat JSON
python3 << PYTHON_SCRIPT > "$RUN_DIR/summary.txt"
import json
import sys
from datetime import datetime

with open("$Dhat_JSON", 'r') as f:
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

leak_ratio = (final_live / total_allocated * 100) if total_allocated > 0 else 0

# Find top 5 allocations by final_live
sorted_pps = sorted(enumerate(pps), key=lambda x: x[1].get('gb', 0) + x[1].get('eb', 0), reverse=True)

print("=" * 70)
print("  Sned Native Memory Profile Summary")
print("=" * 70)
print()
print(f"  Timestamp:    $TIMESTAMP")
print(f"  Workload:     $WORKLOAD")
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

# Health assessment
if leak_ratio < 5:
    health = "✅ HEALTHY"
    health_desc = "< 5% unfreed (normal)"
elif leak_ratio < 20:
    health = "⚠️  WARNING"
    health_desc = f"{leak_ratio:.1f}% unfreed (review recommended)"
else:
    health = "🔴 CRITICAL"
    health_desc = f"{leak_ratio:.1f}% unfreed (investigate immediately)"

print(f"  Memory Health:      {health} ({health_desc})")
print()

print("📈 Top 5 Allocations by Final Live Bytes")
print("-" * 70)

for i, (idx, p) in enumerate(sorted_pps[:5]):
    final = p.get('gb', 0) + p.get('eb', 0)
    if final == 0:
        continue
    
    frame_idx = p['fs'][0] if p.get('fs') and len(p['fs']) > 0 else 0
    frame_str = ftbl[frame_idx] if frame_idx < len(ftbl) else "unknown"
    func_name = frame_str.split(': ', 1)[1].split(' (')[0] if ': ' in frame_str else frame_str
    if len(func_name) > 60:
        func_name = func_name[:57] + "..."
    
    print(f"  {i+1}. {format_bytes(final):>12}  {func_name}")

print()

# Categorize allocations
categories = {'std_lib': 0, 'profiler': 0, 'runtime': 0, 'application': 0}
for p in pps:
    frame_idx = p['fs'][0] if p.get('fs') and len(p['fs']) > 0 else 0
    frame_str = ftbl[frame_idx] if frame_idx < len(ftbl) else ""
    frame_lower = frame_str.lower()
    
    final = p.get('gb', 0) + p.get('eb', 0)
    
    if any(x in frame_lower for x in ['<alloc::', 'alloc::alloc::global', 'alloc::boxed', 'box_assume_init', 'raw_vec']):
        categories['std_lib'] += final
    elif '<dhat' in frame_lower or 'dhat::' in frame_lower:
        categories['profiler'] += final
    elif any(x in frame_lower for x in ['tokio', 'regex', 'tracing', 'serde_json', 'hyper', 'reqwest', 'mio']):
        categories['runtime'] += final
    else:
        categories['application'] += final

print("📦 Allocation Categories (Final Live)")
print("-" * 70)
print(f"  Standard Library:   {format_bytes(categories['std_lib']):>12}  (Rust allocator - NOT leaks)")
print(f"  Profiler Overhead:  {format_bytes(categories['profiler']):>12}  (dhat - NOT leaks)")
print(f"  Runtime:            {format_bytes(categories['runtime']):>12}  (tokio/serde - expected)")
print(f"  Application:        {format_bytes(categories['application']):>12}  (review if >1MB)")
print()

# Recommendations
print("💡 Recommendations")
print("-" * 70)

if categories['application'] > 1048576:  # > 1MB
    print(f"  🔴 Application has {format_bytes(categories['application'])} unfreed")
    print("     → Review allocations.txt for specific allocation sites")
    print("     → Check for: Arc cycles, missing Drop impls, global caches")
elif categories['application'] > 102400:  # > 100KB
    print(f"  🟡 Application has {format_bytes(categories['application'])} unfreed")
    print("     → Monitor for growth over multiple runs")
else:
    print(f"  ✅ Application memory usage is healthy")

if leak_ratio > 50:
    print("  🔴 CRITICAL: >50% leak ratio indicates serious memory leak")
    print("     → Run with tokio-console to check for task leaks")
    print("     → Check channel sender/receiver lifecycle")

print()
print("=" * 70)
print("  Full analysis: allocations.txt")
print("  Raw data:      dhat-heap.json (if --keep-json)")
print("=" * 70)
PYTHON_SCRIPT

echo -e "${GREEN}✓ Summary report generated${NC}"
echo ""

# Show summary
echo "=============================================="
echo "  Summary"
echo "=============================================="
echo ""
cat "$RUN_DIR/summary.txt"
echo ""

# Cleanup
if [ "$KEEP_JSON" = false ] && [ -f "$Dhat_JSON" ]; then
    rm -f "$Dhat_JSON"
    echo -e "${BLUE}Cleaned up dhat-heap.json (use --keep-json to retain)${NC}"
fi

echo ""
echo "=============================================="
echo "  Profile Complete"
echo "=============================================="
echo ""
echo "  Reports saved to: $RUN_DIR/"
echo "    - summary.txt     (this summary)"
echo "    - allocations.txt (detailed breakdown)"
if [ "$KEEP_JSON" = true ]; then
    echo "    - dhat-heap.json  (raw data)"
fi
echo ""
echo "  To view interactive dhat report:"
echo "    1. Run with --keep-json"
echo "    2. Open https://nnethercote.github.io/dhat-viewer/"
echo "    3. Upload $RUN_DIR/dhat-heap.json"
echo ""
