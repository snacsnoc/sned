#!/bin/bash
# Analyze dhat-heap.json memory profiling output
# Usage: ./analyze-dhat-heap.sh <path-to-dhat-heap.json>
#
# This script uses analyze-dhat-heap.py for intelligent categorization
# to filter out standard library noise and show actual potential leaks.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Check argument
if [ $# -lt 1 ]; then
    echo "Usage: $0 <dhat-heap.json>"
    echo ""
    echo "Analyzes dhat heap profiling output for memory leaks."
    echo "Categorizes allocations to filter out standard library noise."
    echo ""
    echo "Output:"
    echo "  📦 Standard Library - Rust allocator (NOT leaks)"
    echo "  ⚙️  Profiler - Dhat overhead (NOT leaks)"
    echo "  ⚙️  Runtime - Tokio/regex/tracing (NOT leaks)"
    echo "  🔍 Application - Your code (review if >100KB unfreed)"
    exit 1
fi

JSON_FILE="$1"

if [ ! -f "$JSON_FILE" ]; then
    echo "Error: File not found: $JSON_FILE"
    exit 1
fi

# Check for Python
if ! command -v python3 &> /dev/null; then
    echo "Error: python3 is required but not installed."
    exit 1
fi

# Run Python analyzer
python3 "$SCRIPT_DIR/analyze-dhat-heap.py" "$JSON_FILE"
exit $?
