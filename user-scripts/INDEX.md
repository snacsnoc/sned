# Dirac Native User Scripts

Small helper scripts for task discovery, context packing, profiling, and a few repo setup checks.

## Quick Guide

Use the script that matches the job:

| Need to... | Run | Notes |
|---|---|---|
| Find open work | `list-open-todos.sh` | Reads `TODO.md` and prints heading line numbers |
| Read one task | `todo-section.sh <LINE>` | Pass the heading line from `list-open-todos.sh` |
| Build task context | `pack-task-context.sh <files>` | Use after reading the task and picking files |
| Profile memory | `profile-memory.sh` | Use for allocation or leak questions |
| Analyze dhat output | `analyze-dhat-heap.sh <json>` | Use after profiling |
| Regenerate repo map | `regen-infiniloom.sh` | Use after structural changes |
| Check Zig setup | `setup-zig-0.15.sh` | Only for libghostty-rs / Zig issues |

**Default flow**
1. If the user did not already name a task, run `list-open-todos.sh`.
2. If the task is non-trivial, read it with `todo-section.sh <LINE>`.
3. Build context with `pack-task-context.sh` when multiple files matter.
4. Implement, validate, and update `TODO.md`.

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

**When to Use (LLM Agent Decision Tree):**
- ✅ **Just implemented a feature** → Run `--workload basic` to verify no memory regressions
- ✅ **Modified file editing code** → Run `--workload edit` to check anchor reconciliation memory
- ✅ **Changed symbol indexing** → Run `--workload search` to validate parser allocations
- ✅ **Before marking task DONE** → Run `--workload all` if task is performance-related
- ✅ **User reports high memory** → Run `--workload all --keep-json` for detailed analysis
- ✅ **PR includes allocation changes** → Run and compare against `MEMORY_PROFILE_BASELINE.md`
- ❌ **Not needed for**: Documentation changes, test-only changes, bug fixes unrelated to memory

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

**When to Use (LLM Agent Decision Tree):**
- ✅ **After `profile-memory.sh`** → Always run this to analyze the generated `dhat-heap.json`
- ✅ **User provides `dhat-heap.json`** → Run this script to analyze it
- ✅ **Investigating memory leak** → Run after manual `cargo run --features dhat-heap`
- ✅ **Before marking DONE** → Run if task involves memory/performance optimizations
- ❌ **Not needed if**: Already ran `profile-memory.sh` (it calls this automatically)
- ❌ **Don't use alone**: Must have `dhat-heap.json` from profiling session first

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

**What:** Lists all open tasks from `TODO.md`.

**Usage:**
```bash
./user-scripts/list-open-todos.sh
```

**When to Use (LLM Agent Decision Tree):**
- ✅ **No user direction** → Run to check for open work
- ✅ **User says "what's next"** → Run to list available work
- ✅ **Multi-agent coordination** → Run to find unclaimed tasks
- ✅ **Claiming task** → Run to verify status before touching it
- ❌ **Don't run if**: User already gave you a task to do
- ❌ **Don't run if**: You already have the task heading line from a previous step

**Why Use It:**
- **Single source of truth**: Reads directly from `TODO.md` (the coordination ledger)
- **Filtered view**: Only shows actionable items (NOT STARTED, INCOMPLETE)
- **Fast**: Uses ripgrep for instant results

**Example Output:**
```
TODO.md:45:## Phase 2: Core Functionality [NOT STARTED]
TODO.md:52:### Bug 26: Fix used_words truncation [NOT STARTED]
TODO.md:58:### Feature: Add CLI completions [INCOMPLETE]
```

**Dependencies:** ripgrep (`rg`)

---

### `todo-section.sh`

**What:** Prints the full TODO section for one task heading.

**Usage:**
```bash
# Get line number from list-open-todos.sh output
./user-scripts/todo-section.sh 52
```

**When to Use (LLM Agent Decision Tree):**
- ✅ **After `list-open-todos.sh`** → Run with the heading line number
- ✅ **Before writing code for a TODO task** → Run to read scope, evidence, and acceptance criteria
- ✅ **During implementation** → Re-read if you need to check remaining checkboxes
- ✅ **User provides the line number** → Run to get the task context
- ❌ **Don't run without**: A heading line number if you can avoid it

**Why Use It:**
- **Context preservation**: Shows the full task definition, acceptance criteria, and validation notes
- **Flexible input**: If given a body line by mistake, it walks up to the nearest heading and reports the correction
- **Prevents scope creep**: Keeps implementation tied to the actual task section

**Example:**
```bash
$ ./user-scripts/todo-section.sh 1609
### 6.4c Native skill activation prints broken supporting-file paths [DONE]
...
```

**Dependencies:** None (uses bash built-ins)

---

## Context Building

### `pack-task-context.sh`

**What:** Packs selected files into a smaller context bundle for task work.

**Usage:**
```bash
# Pass files as arguments
./user-scripts/pack-task-context.sh src/core/file_editor.rs src/core/anchor_dictionary.rs

# Or pipe from stdin
printf '%s\n' src/**/*.rs | ./user-scripts/pack-task-context.sh
```

**When to Use (LLM Agent Decision Tree):**
- ✅ **After `todo-section.sh`** → Run with the files named in the task
- ✅ **Before writing code** → Run when more than one file matters
- ✅ **Debugging multi-file issues** → Run with the whole affected surface
- ❌ **Don't run for**: Single-file changes unless you need a compact packet
- ❌ **Don't run without**: Knowing which files are relevant

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

**When to Use (LLM Agent Decision Tree):**
- ✅ **User asks "what does this repo do"** → Run to generate fresh overview
- ✅ **After adding new modules** → Run to update `.infiniloom/map.md` with new structure
- ✅ **Starting work on unfamiliar codebase** → Run to get orientation materials
- ✅ **Post-refactor** → Run when file structure changed significantly
- ❌ **Don't run for**: Every task (only when structure changes or user explicitly asks)
- ❌ **Not needed if**: `.infiniloom/` was regenerated recently (< 1 week ago)

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

**When to Use (LLM Agent Decision Tree):**
- ✅ **Build fails with Zig error** → Run to diagnose version issue
- ✅ **User reports "zig not found"** → Run to provide installation instructions
- ✅ **Before `cargo build` in libghostty-rs** → Run once to verify setup
- ✅ **Seeing "Zig version mismatch"** → Run to get correct PATH instructions
- ❌ **Don't run for**: Regular dirac-native builds (only needed for libghostty-rs)
- ❌ **Not needed if**: User already has Zig 0.15.x in PATH (script will confirm)

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

### Development Flow

**User-directed (default):**
```bash
# 1. Load relevant skills per AGENTS.md
# 2. Implement the task
# 3. Validate (cargo test / cargo check)
# 4. Commit
```

**TODO-led (when open tasks exist):**
```bash
# 1. Find available tasks
./user-scripts/list-open-todos.sh

# 2. Read task details (replace LINE_NO with actual line number)
./user-scripts/todo-section.sh LINE_NO

# 3. Build context for the task
printf '%s\n' src/core/file_editor.rs | ./user-scripts/pack-task-context.sh

# 4. Implement...

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
