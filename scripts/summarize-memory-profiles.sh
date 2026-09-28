#!/usr/bin/env bash
# One report across every kept workload profile in a run directory.
#
# Usage: summarize-memory-profiles.sh [profile-dir]
#   profile-dir defaults to the newest run under ./target/memory-profiles.
#
# Each kept dhat-*.json first goes through the per-workload analyzer so the
# full breakdowns stay on disk next to this cross-workload comparison.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
cd "$REPO_ROOT"

# An explicit directory wins; otherwise compare the most recent run, since
# consecutive profile runs are how regressions get spotted.
if [ $# -ge 1 ]; then
    PROFILE_DIR="$1"
else
    PROFILE_DIR="$(ls -dt ./target/memory-profiles/profile-*/ 2>/dev/null | head -n 1 || true)"
fi

if [ -z "${PROFILE_DIR:-}" ] || [ ! -d "$PROFILE_DIR" ]; then
    echo "Error: no profile directory found." >&2
    echo "Usage: $0 [profile-dir]  (default: newest ./target/memory-profiles/profile-*/)" >&2
    exit 1
fi

shopt -s nullglob
DHAT_FILES=("$PROFILE_DIR"/dhat-*.json)
shopt -u nullglob
if [ ${#DHAT_FILES[@]} -eq 0 ]; then
    echo "Error: no dhat-*.json profiles kept in $PROFILE_DIR" >&2
    echo "Re-run profile-memory.sh with --keep-json first." >&2
    exit 1
fi

# Keep the detailed per-workload breakdowns beside the comparison.
for WL_JSON in "${DHAT_FILES[@]}"; do
    WL_NAME="$(basename "$WL_JSON" .json)"
    WL_NAME="${WL_NAME#dhat-}"
    if ! "$SCRIPT_DIR/analyze-dhat-heap.sh" "$WL_JSON" > "$PROFILE_DIR/analysis-${WL_NAME}.txt" 2>&1; then
        echo "Warning: per-workload analysis had warnings for $WL_NAME" >&2
    fi
done

python3 - "$PROFILE_DIR" << 'PYEOF'
import glob
import json
import os
import sys
from collections import defaultdict

profile_dir = sys.argv[1]
paths = sorted(glob.glob(os.path.join(profile_dir, "dhat-*.json")))

# Leaf frames only move bytes for a caller below; attribute to that caller.
ALLOCATOR_FRAME_MARKERS = [
    "<dhat::alloc", "dhat::alloc", "core::alloc::", "alloc::alloc::",
    "alloc::raw_vec::", "alloc::vec::", "alloc::slice::", "alloc::boxed::",
    "alloc::string::", "globalalloc", "exchange_malloc", "__rust_alloc",
]

def attributing_frame(frames, ftbl):
    for idx in frames:
        frame_str = ftbl[idx] if idx < len(ftbl) else ""
        if not any(m in frame_str.lower() for m in ALLOCATOR_FRAME_MARKERS):
            return frame_str
    if frames:
        idx = frames[0]
        return ftbl[idx] if idx < len(ftbl) else ""
    return ""

def short_func(frame_str, width=64):
    func = frame_str.split(": ", 1)[1].split(" (")[0] if ": " in frame_str else frame_str
    return func[: width - 3] + "..." if len(func) > width else func

def first_sned_frame(frames, ftbl):
    for idx in frames:
        frame_str = ftbl[idx] if idx < len(ftbl) else ""
        lowered = frame_str.lower()
        if "sned::" in lowered or "sned-" in lowered:
            return short_func(frame_str)
    return None

def categorize(frame_str):
    lowered = frame_str.lower()
    if "dhat::" in lowered or "<dhat" in lowered:
        return "profiler"
    if "sned::" in lowered or "sned-" in lowered:
        return "application"
    if any(x in lowered for x in ["<alloc::", "alloc::alloc::global", "box_assume_init", "raw_vec"]):
        return "std_lib"
    if any(x in lowered for x in ["tokio", "regex", "tracing", "mio", "serde_json", "hyper", "reqwest"]):
        return "runtime"
    return "application"

def format_bytes(b):
    if b >= 1048576:
        return f"{b / 1048576:.2f} MB"
    if b >= 1024:
        return f"{b / 1024:.2f} KB"
    return f"{b} B"

# Stack shapes that explain retention without implying a defect.
# Each hit names the stack frame that matched, not the attributing frame,
# so the flag points at the pattern itself.
def pattern_evidence(stack, lowered):
    evidence = {}
    for orig, low in zip(stack, lowered):
        if ("trackeddocument" in low and "clone" in low) or "anchorstorage::load" in low \
                or ("indexmap" in low and "clone" in low):
            evidence.setdefault("whole-map clone", short_func(orig))
        if ("sned" in low and "persist" in low) or "save_checkpoint" in low \
                or "serde_json::ser::to_writer" in low:
            evidence.setdefault("full-state persist", short_func(orig))
    regex_frame = next((orig for orig, low in zip(stack, lowered)
                        if "regex_automata" in low or "regex_syntax" in low or "::regex::" in low), None)
    once_init = any("get_or_init" in low or "once_lock" in low or "once_box" in low for low in lowered)
    if regex_frame is not None and once_init:
        evidence.setdefault("static regex init", short_func(regex_frame))
    return evidence

results = []
for path in paths:
    name = os.path.basename(path)[len("dhat-"):-len(".json")]
    with open(path) as f:
        data = json.load(f)
    ftbl = data.get("ftbl", [])
    pps = data.get("pps", [])
    total = sum(p["tb"] for p in pps)
    live = sum(p["gb"] + p["eb"] for p in pps)
    cats = defaultdict(int)
    sites = {}
    patterns = defaultdict(int)
    pattern_example = {}
    for p in pps:
        frames = p.get("fs", [])
        attr = attributing_frame(frames, ftbl)
        retained = p["gb"] + p["eb"]
        cats[categorize(attr)] += retained
        stack = [ftbl[i] if i < len(ftbl) else "" for i in frames]
        for label, frame in pattern_evidence(stack, [f.lower() for f in stack]).items():
            patterns[label] += retained
            pattern_example.setdefault(label, frame)
        if retained > 0:
            # One row per (attributing site, nearest sned frame) pair; bytes
            # grouped by site alone would credit one sned frame with stacks
            # that merely share a leaf allocator call.
            site = short_func(attr)
            key = (site, first_sned_frame(frames, ftbl))
            entry = sites.get(key)
            if entry is None:
                sites[key] = retained
            else:
                sites[key] += retained
    top = sorted(((total, site, sned) for (site, sned), total in sites.items()),
                 key=lambda entry: entry[0], reverse=True)
    results.append({
        "name": name, "total": total, "live": live,
        "ratio": (live / total * 100) if total else 0,
        "allocs": sum(p["tbk"] for p in pps),
        "cats": cats, "top": top[:5],
        "patterns": patterns, "pattern_example": pattern_example,
    })

print("=" * 70)
print(f"  Memory Profile Comparison: {profile_dir}")
print(f"  Workloads: {', '.join(r['name'] for r in results)}")
print("=" * 70)

print("\nPer-workload totals")
print("-" * 70)
for r in results:
    print(f"  [{r['name']}] allocated {format_bytes(r['total']):>10}"
          f"  live {format_bytes(r['live']):>10}  ({r['ratio']:.2f}%)"
          f"  {r['allocs']:,} allocs")

print("\nTop-5 attributing sites (nearest sned frame)")
print("-" * 70)
for r in results:
    print(f"  [{r['name']}]")
    for i, (retained, site, sned_frame) in enumerate(r["top"]):
        print(f"    {i + 1}. {format_bytes(retained):>10}  {site}")
        print(f"       sned: {sned_frame or '(no sned frame on stack)'}")

print("\nApp / runtime / profiler split (retained)")
print("-" * 70)
for r in results:
    c = r["cats"]
    print(f"  [{r['name']}] application {format_bytes(c['application']):>10}"
          f"  runtime {format_bytes(c['runtime']):>10}"
          f"  profiler {format_bytes(c['profiler']):>10}"
          f"  std {format_bytes(c['std_lib']):>10}")

print("\nCross-workload comparison")
print("-" * 70)
baseline = min(results, key=lambda r: r["live"])
print(f"  Leanest retained: {baseline['name']} ({format_bytes(baseline['live'])})")
for r in results:
    if r is not baseline:
        delta = r["live"] - baseline["live"]
        print(f"  {r['name']} retains {format_bytes(delta)} more than {baseline['name']}")
if len(results) > 1:
    seen = defaultdict(dict)
    for r in results:
        for retained, site, _ in r["top"]:
            seen[site][r["name"]] = retained
    shared = {s: m for s, m in seen.items() if len(m) > 1}
    if shared:
        print("  Shared top sites:")
        for site, per_wl in sorted(shared.items(), key=lambda kv: -sum(kv[1].values())):
            detail = ", ".join(f"{wl} {format_bytes(b)}" for wl, b in per_wl.items())
            print(f"    - {short_func(site)} ({detail})")
    else:
        print("  No shared top-5 sites across workloads.")

print("\nKnown-cost pattern flags")
print("-" * 70)
for r in results:
    print(f"  [{r['name']}]")
    for label in ("whole-map clone", "full-state persist", "static regex init"):
        retained = r["patterns"].get(label, 0)
        if retained:
            print(f"    FLAG {label}: {format_bytes(retained)}"
                  f" e.g. {r['pattern_example'].get(label, '')}")
        else:
            print(f"    ok   {label}: not observed")
print()
print("=" * 70)
print(f"  Per-workload detail: {profile_dir}/analysis-<workload>.txt")
print("=" * 70)
PYEOF
