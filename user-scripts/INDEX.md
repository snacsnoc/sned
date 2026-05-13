# Dirac Native User Scripts

Helper scripts for development workflow, task management, and memory profiling.

## Quick Reference

| Script | Purpose | When to Use |
|--------|---------|-------------|
| `profile-memory.sh` | Automated memory profiling | Baseline, regression testing |
| `analyze-dhat-heap.sh` | Memory leak analysis | After profiling with dhat |
| `list-open-todos.sh` | List open TODO tasks | Starting a new task |
| `todo-section.sh` | Read specific TODO section | Understanding task scope |
| `pack-task-context.sh` | Build task context | Before implementation |
| `regen-infiniloom.sh` | Regenerate repo overview | After major refactors |
| `setup-zig-0.15.sh` | Zig toolchain setup | Building libghostty-rs |

---

## Memory Profiling

### `profile-memory.sh`

**What:** Automated end-to-end memory profiling workflow with dhat.

**Usage:**
```bash
# Run all workloads (basic + edit + search)
./user-scripts/profile-memory.sh --workload all

# Run specific workload
./user-scripts/profile-memory.sh --workload edit

# Keep raw JSON for interactive viewer
./user-scripts/profile-memory.sh --workload all --keep-json

# View generated reports
cat target/memory-profiles/profile-*/summary.txt
cat target/memory-profiles/profile-*/allocations.txt
```

**When to Use:**
- **Baseline profiling**: Establish memory usage baseline for comparison
- **Regression testing**: After major changes, compare against baseline
- **Leak investigation**: Automated workload + analysis in one command
- **PR validation**: Verify no memory regressions introduced

**Why Use It:**
- **Fully automated**: Build → Run → Analyze → Report in one command
- **Smart categorization**: Filters out std library, profiler, runtime noise
- **Multiple workloads**: Tests different code paths (CLI init, editing, search)
- **Actionable reports**: Clear HEALTHY/WARNING/CRITICAL verdicts
- **Historical tracking**: Timestamped profiles for trend analysis

**Output:**
```
target/memory-profiles/profile-YYYYMMDD-HHMMSS/
├── summary.txt       # Human-readable summary with verdict
├── allocations.txt   # Categorized allocation breakdown
├── dhat-heap.json    # Raw data (if --keep-json)
└── build.log         # Compilation output
```

**Workloads:**
| Name | Description | Exercises |
|------|-------------|-----------|
| `basic` | `--help` command | CLI parsing, initialization |
| `edit` | File editing simulation | Anchor reconciliation, file I/O |
| `search` | File search command | Tree-sitter, symbol indexing |
| `all` | Sequential execution | Full code coverage |

**Dependencies:** Python 3, jq

**Related:** See `dirac-native/MEMORY_PROFILE_BASELINE.md` for baseline results.

---

### `analyze-dhat-heap.sh` / `analyze-dhat-heap.py`

**What:** Analyzes dhat heap profiling output to detect memory leaks.

**Why Two Files?**
- `analyze-dhat-heap.sh` - CLI wrapper (argument validation, help text, dependency checks)
- `analyze-dhat-heap.py` - Analysis engine (JSON parsing, categorization, reporting)
- **Users only interact with `.sh`** - it calls Python internally

**Usage:**
```bash
# 1. Run dirac-native with dhat feature
cargo run --features dhat-heap -- --task "count to 10"

# 2. Analyze the output (use .sh, not .py)
./user-scripts/analyze-dhat-heap.sh dirac-native/dhat-heap.json
```

**When to Use:**
- After running a memory profiling session
- Before marking performance-related tasks complete
- When investigating slow shutdown or high memory usage
- As part of PR review for performance-sensitive changes

**Why Use It:**
- **Filters noise**: Automatically categorizes standard library, profiler, and runtime allocations as "expected"
- **Focuses on your code**: Only flags application code with >100KB unfreed as potentially suspicious
- **Clear verdict**: Provides HEALTHY/WARNING/CRITICAL status based on actual leaks, not allocator internals
- **Prevents false alarms**: Rust's allocator (`alloc::alloc::Global`) shows up in every program - this tool knows to ignore it

**Output Example:**
```
📦 Standard Library (1516 allocations)
   Rust's allocator internals - NOT leaks, expected to persist

🔍 Application Code & Libraries (0 allocations)
   ✅ No significant unfreed allocations detected

✅ HEALTHY: No significant memory leaks detected
```

**Dependencies:** Python 3, jq

---

## TODO Management

### `list-open-todos.sh`

**What:** Lists all `[NOT STARTED]` and `[INCOMPLETE]` tasks from `TODO.md`.

**Usage:**
```bash
./user-scripts/list-open-todos.sh
```

**When to Use:**
- **Cold start**: First thing when beginning a development session
- **Task selection**: When looking for the next task to work on
- **Progress review**: Checking what remains in the current phase

**Why Use It:**
- **Single source of truth**: Reads directly from `TODO.md` (the project ledger)
- **Filtered view**: Only shows actionable items (NOT STARTED, INCOMPLETE)
- **Fast**: Uses ripgrep for instant results
- **Workflow gate**: Ensures you claim a task before implementing

**Example Output:**
```
TODO.md:45:## Phase 2: Core Functionality [NOT STARTED]
TODO.md:52:### Bug 26: Fix used_words truncation [NOT STARTED]
TODO.md:58:### Feature: Add CLI completions [INCOMPLETE]
```

**Dependencies:** ripgrep (`rg`)

---

### `todo-section.sh`

**What:** Extracts a specific TODO section by line number.

**Usage:**
```bash
# Get line number from list-open-todos.sh output
./user-scripts/todo-section.sh 52
```

**When to Use:**
- **After `list-open-todos.sh`**: Read the full details of a selected task
- **Before claiming**: Understand complete scope and requirements
- **During implementation**: Reference acceptance criteria

**Why Use It:**
- **Context preservation**: Shows the complete task definition including:
  - Problem description
  - Acceptance criteria
  - Related files
  - Validation requirements
- **Prevents scope creep**: Clear boundaries of what to implement
- **Workflow compliance**: Required by `AGENTS.md` before implementation

**Example:**
```bash
$ ./user-scripts/todo-section.sh 52
### Bug 26: Fix used_words truncation [NOT STARTED]
**Priority:** LOW
**Location:** dirac-native/src/core/file_editor.rs:384

## Problem
HashSet iteration order is non-deterministic...

## Acceptance Criteria
- [ ] Use BTreeSet for deterministic ordering
- [ ] Add test for truncation behavior
- [ ] Verify no active anchors removed
```

**Dependencies:** None (uses bash built-ins)

---

## Context Building

### `pack-task-context.sh`

**What:** Builds a compressed context packet from specified files for AI-assisted development.

**Usage:**
```bash
# Pass files as arguments
./user-scripts/pack-task-context.sh src/core/file_editor.rs src/core/anchor_dictionary.rs

# Or pipe from stdin
printf '%s\n' src/**/*.rs | ./user-scripts/pack-task-context.sh
```

**When to Use:**
- **Before implementation**: After reading TODO section, before writing code
- **During debugging**: When investigating a complex issue across multiple files
- **For code review**: Creating context for explaining changes

**Why Use It:**
- **Token efficiency**: Compresses files to fit within LLM context limits (8K default)
- **Smart filtering**: Excludes generated code, vendor directories, logs
- **Secret redaction**: Automatically removes API keys and credentials
- **Focused context**: Includes only relevant files, not entire codebase

**Output:**
- Default: `/tmp/dirac-task-context.md`
- Custom: `DIRAC_TASK_CONTEXT_OUT=./my-context.md ./user-scripts/pack-task-context.sh ...`

**Dependencies:** `infiniloom` CLI tool

---

### `regen-infiniloom.sh`

**What:** Regenerates the `.infiniloom/` directory with repo overview and context.

**Usage:**
```bash
./user-scripts/regen-infiniloom.sh
```

**When to Use:**
- **After major refactors**: File structure changed significantly
- **New module added**: When new directories/files need indexing
- **Cold start preparation**: Before a major development session
- **Weekly maintenance**: Keep repo overview current

**Why Use It:**
- **`map.md`**: Provides structural overview of `dirac-native/` for orientation
- **`context.md`**: Contains curated source code for common patterns
- **AI assistance**: Used by agents to understand repo structure without reading everything
- **Excludes noise**: Skips docs, benches, vendor, generated code

**Generates:**
- `dirac-native/.infiniloom/map.md` - Repository structure map
- `dirac-native/.infiniloom/context.md` - Source code context (12K tokens)

**Dependencies:** `infiniloom` CLI tool

---

## Toolchain Setup

### `setup-zig-0.15.sh`

**What:** Checks and provides instructions for Zig 0.15.x toolchain.

**Usage:**
```bash
# Check current setup
./user-scripts/setup-zig-0.15.sh

# Or source to see inline output
source ./user-scripts/setup-zig-0.15.sh
```

**When to Use:**
- **First build**: Before building `libghostty-rs` for the first time
- **Build errors**: When seeing Zig-related compilation failures
- **Version conflicts**: If Zig version mismatches occur

**Why Use It:**
- **Specific version**: `libghostty-rs` requires Zig 0.15.x (not latest)
- **Clear instructions**: Shows exact PATH to add to shell profile
- **Diagnostic**: Identifies if Zig is missing, wrong version, or correct

**Example Output:**
```
✓ Zig 0.15 found at /opt/homebrew/opt/zig@0.15
To add to PATH, run:
  export PATH="/opt/homebrew/opt/zig@0.15/bin:$PATH"
```

**Dependencies:** None (pure bash)

---

## Shared Libraries

### `infiniloom-common.sh`

**What:** Shared configuration for Infiniloom-based scripts.

**Not for direct use** - sourced by other scripts.

**Provides:**
- `infiniloom_repo_root()` - Get repository root directory
- `infiniloom_common_excludes` - Standard exclusions (vendor, .infiniloom, logs)
- `infiniloom_source_excludes` - Additional exclusions for source-focused packs (docs, benches)

**Used By:**
- `pack-task-context.sh`
- `regen-infiniloom.sh`

---

## Workflow Integration

### Standard Development Flow

```bash
# 1. Find available tasks
./user-scripts/list-open-todos.sh

# 2. Read task details (replace LINE_NO with actual line number)
./user-scripts/todo-section.sh LINE_NO

# 3. Build context for the task
printf '%s\n' src/core/file_editor.rs | ./user-scripts/pack-task-context.sh

# 4. Implement the fix...

# 5. If performance-related, profile with dhat
./user-scripts/profile-memory.sh --workload basic

# 6. Mark task complete in TODO.md
```

### Memory Profiling Flow

```bash
# 1. Run automated profiling (recommended)
./user-scripts/profile-memory.sh --workload all --keep-json

# 2. Review summary
cat target/memory-profiles/profile-*/summary.txt

# 3. If issues detected, investigate detailed breakdown
cat target/memory-profiles/profile-*/allocations.txt

# 4. Optional: View interactive report
# Upload target/memory-profiles/profile-*/dhat-heap.json to:
# https://nnethercote.github.io/dhat-viewer/

# 5. Fix identified issues, re-profile to verify
```

### Memory Leak Investigation Flow

```bash
# Option A: Automated (recommended)
./user-scripts/profile-memory.sh --workload edit

# Option B: Manual (for custom workloads)
cargo run --features dhat-heap -- --task "reproduce issue"
./user-scripts/analyze-dhat-heap.sh dirac-native/dhat-heap.json

# 3. If leaks detected, review "Application Code" section
# 4. Fix identified issues
# 5. Re-profile to verify fix
```

---

## Dependencies

| Tool | Required By | Install Command |
|------|-------------|-----------------|
| `jq` | profile-memory.sh, analyze-dhat-heap.sh | `brew install jq` |
| `python3` | profile-memory.sh, analyze-dhat-heap.py | Pre-installed on macOS |
| `ripgrep` | list-open-todos.sh | `brew install ripgrep` |
| `infiniloom` | pack-task-context.sh, regen-infiniloom.sh | `cargo install infiniloom` |
| `zig@0.15` | Building libghostty-rs | `brew install zig@0.15` |

---

## Troubleshooting

### "dhat-heap.json not found"
```bash
# The workload may not have triggered heap allocations
# Try running with a more comprehensive workload:
./user-scripts/profile-memory.sh --workload all --keep-json

# Or check if dhat feature is properly enabled:
grep dhat-heap dirac-native/Cargo.toml
```

### "python3 not found"
```bash
# macOS: python3 is pre-installed
# If missing, install from python.org or:
brew install python3
```

### "jq not found"
```bash
brew install jq
```

### "ripgrep not found"
```bash
brew install ripgrep
```

### "infiniloom not found"
```bash
cargo install infiniloom
```

### "Zig version mismatch"
```bash
# Install correct version
brew install zig@0.15
export PATH="/opt/homebrew/opt/zig@0.15/bin:$PATH"
```

---

## Adding New Scripts

When adding a new script to `user-scripts/`:

1. **Follow naming**: `kebab-case.sh` or `kebab-case.py`
2. **Add shebang**: `#!/usr/bin/env bash` or `#!/usr/bin/env python3`
3. **Make executable**: `chmod +x user-scripts/script-name.sh`
4. **Document here**: Add entry to this INDEX.md
5. **Error handling**: Use `set -euo pipefail` for bash scripts
6. **Help text**: Include usage information in `--help` or when called without args
