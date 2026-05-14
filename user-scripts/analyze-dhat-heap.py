#!/usr/bin/env python3
"""
Analyze dhat-heap.json and categorize allocations by source.
Filters out standard library and profiler noise to show actual potential leaks.
"""

import json
import sys
from collections import defaultdict

def format_bytes(b):
    """Convert bytes to human-readable format."""
    if b >= 1048576:
        return f"{b / 1048576:.2f} MB"
    elif b >= 1024:
        return f"{b / 1024:.2f} KB"
    return f"{b} B"

def categorize_allocation(frame_str):
    """Categorize allocation by source based on frame string."""
    if not frame_str:
        return "unknown"
    
    frame_lower = frame_str.lower()
    
    # Standard library (not leaks - these are Rust's allocator)
    if any(x in frame_lower for x in ['<alloc::', 'alloc::alloc::global', 'box_assume_init', 'raw_vec']):
        return "std_lib"
    
    # Profiler overhead (not leaks)
    if '<dhat' in frame_lower or 'dhat::' in frame_lower:
        return "profiler"
    
    # Runtime infrastructure (expected to persist)
    if any(x in frame_lower for x in ['tokio', 'regex', 'tracing', 'mio', 'serde_json', 'hyper', 'reqwest']):
        return "runtime"
    
    # Your code or external libraries (potential leaks)
    return "application"

def extract_function_name(frame_str):
    """Extract readable function name from frame string."""
    if not frame_str:
        return "unknown"
    
    # Remove address prefix
    parts = frame_str.split(': ', 1)
    if len(parts) > 1:
        func_part = parts[1]
    else:
        func_part = parts[0]
    
    # Remove file location
    func_part = func_part.split(' (')[0]
    
    # Truncate long names
    if len(func_part) > 80:
        func_part = func_part[:77] + "..."
    
    return func_part

def analyze_dhat_heap(json_file):
    """Analyze dhat heap JSON and print categorized report."""
    
    with open(json_file, 'r') as f:
        data = json.load(f)
    
    # Get frame table
    ftbl = data.get('ftbl', [])
    pps = data.get('pps', [])
    
    # Calculate totals
    total_allocated = sum(p['tb'] for p in pps)
    total_freed = sum(p['tb'] - p['gb'] - p['eb'] for p in pps)
    final_live = sum(p['gb'] + p['eb'] for p in pps)
    
    print("=" * 70)
    print("  dhat Heap Analysis Report")
    print("=" * 70)
    print()
    
    print("📊 Overall Statistics")
    print("-" * 70)
    print(f"  Total Allocated:  {format_bytes(total_allocated)}")
    print(f"  Total Freed:      {format_bytes(total_freed)}")
    print(f"  Final Live:       {format_bytes(final_live)}")
    print(f"  Leak Ratio:       {(final_live / total_allocated * 100):.2f}%" if total_allocated > 0 else "  Leak Ratio:       N/A")
    print()
    
    # Categorize allocations
    categories = defaultdict(list)
    for i, p in enumerate(pps):
        frame_idx = p['fs'][0] if p.get('fs') else 0
        frame_str = ftbl[frame_idx] if frame_idx < len(ftbl) else ""
        
        category = categorize_allocation(frame_str)
        categories[category].append({
            'index': i,
            'frame': frame_str,
            'func': extract_function_name(frame_str),
            'max_bytes': p['mb'],
            'unfreed': p['gb'] + p['eb'],
            'allocations': p['tbk']
        })
    
    print("📈 Allocations by Category")
    print("-" * 70)
    
    # Standard Library
    std_lib = sorted(categories['std_lib'], key=lambda x: x['max_bytes'], reverse=True)[:5]
    print(f"\n📦 Standard Library ({len(categories['std_lib'])} allocations)")
    print("   Rust's allocator internals - NOT leaks, expected to persist")
    for alloc in std_lib:
        print(f"   • {alloc['func']}")
        print(f"     Max: {format_bytes(alloc['max_bytes'])}, Unfreed: {format_bytes(alloc['unfreed'])}")
    
    # Profiler
    profiler = sorted(categories['profiler'], key=lambda x: x['max_bytes'], reverse=True)[:3]
    if profiler:
        print(f"\n⚙️  Profiler Overhead ({len(categories['profiler'])} allocations)")
        print("   Dhat's own tracking structures - NOT leaks")
        for alloc in profiler:
            print(f"   • {alloc['func']}")
            print(f"     Max: {format_bytes(alloc['max_bytes'])}, Unfreed: {format_bytes(alloc['unfreed'])}")
    
    # Runtime
    runtime = sorted(categories['runtime'], key=lambda x: x['max_bytes'], reverse=True)[:5]
    if runtime:
        print(f"\n⚙️  Runtime Infrastructure ({len(categories['runtime'])} allocations)")
        print("   Tokio, regex, tracing, serialization - expected to persist")
        for alloc in runtime:
            print(f"   • {alloc['func']}")
            print(f"     Max: {format_bytes(alloc['max_bytes'])}, Unfreed: {format_bytes(alloc['unfreed'])}")
    
    # Application/Potential Leaks
    app = sorted(categories['application'], key=lambda x: x['unfreed'], reverse=True)[:10]
    app_significant = [a for a in app if a['unfreed'] > 102400]  # > 100KB
    
    print(f"\n🔍 Application Code & Libraries ({len(categories['application'])} allocations)")
    if app_significant:
        print("   ⚠️  Potentially suspicious (>100KB unfreed):")
        for alloc in app_significant:
            print(f"   • {alloc['func']}")
            print(f"     ⚠️  Unfreed: {format_bytes(alloc['unfreed'])} ({alloc['allocations']} allocations)")
    else:
        print("   ✅ No significant unfreed allocations detected")
        if app:
            print("   Minor allocations (<100KB each):")
            for alloc in app[:5]:
                print(f"     • {alloc['func']}: {format_bytes(alloc['unfreed'])}")
    
    # Summary
    print()
    print("=" * 70)
    print("  Summary & Recommendations")
    print("=" * 70)
    
    leak_ratio = (final_live / total_allocated * 100) if total_allocated > 0 else 0
    
    # Calculate "real" leak ratio (excluding std lib and profiler)
    std_lib_unfreed = sum(a['unfreed'] for a in categories['std_lib'])
    profiler_unfreed = sum(a['unfreed'] for a in categories['profiler'])
    runtime_unfreed = sum(a['unfreed'] for a in categories['runtime'])
    app_unfreed = sum(a['unfreed'] for a in categories['application'])
    
    expected_unfreed = std_lib_unfreed + profiler_unfreed + runtime_unfreed
    unexpected_unfreed = app_unfreed
    
    print()
    print(f"  Total Unfreed:      {format_bytes(final_live)}")
    print(f"  Expected (std+runtime): {format_bytes(expected_unfreed)}")
    print(f"  Unexpected (app):   {format_bytes(unexpected_unfreed)}")
    print()
    
    if unexpected_unfreed < 1048576:  # < 1MB
        print("  ✅ HEALTHY: No significant memory leaks detected")
        print()
        print("  The unfreed memory is from:")
        print("  • Standard library allocators (normal)")
        print("  • Runtime libraries (tokio, regex, etc.)")
        print("  • Profiler overhead (dhat)")
    elif unexpected_unfreed < 10485760:  # < 10MB
        print("  ⚠️  MODERATE: Some unfreed memory from application code")
        print()
        print("  Review the 'Application Code' section above.")
        print("  May be acceptable depending on your use case.")
    else:
        print("  🔴 CRITICAL: Significant memory leaks detected")
        print()
        print("  Review the 'Application Code' section above.")
        print("  Check for:")
        print("  • tokio::spawn tasks without abort handles")
        print("  • Arc reference cycles")
        print("  • Channel sender/receiver lifecycle issues")
        print("  • Mutex guards held across await points")
    
    print()
    print("=" * 70)

if __name__ == '__main__':
    if len(sys.argv) < 2:
        print("Usage: analyze_dhat_heap.py <dhat-heap.json>")
        sys.exit(1)
    
    analyze_dhat_heap(sys.argv[1])
