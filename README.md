# sned

sned is a Rust CLI for working with LLMs on a codebase.

It is built around hash-anchored edits, AST-aware tools, multi-file patching, provider swapping, Qwen-specific tool prompting, and a shadow git repo the model can use for undo.

Models should not blindly patch text they have not recently read. sned hashes the lines it shows to the model, checks those hashes before writing, and rejects stale edits instead of guessing.

For larger changes, sned can batch edits across files, use tree-sitter for symbol-level work, and track accepted agent turns in a separate `.sned/.git-agent/` shadot git repo. 



## Build

```bash
cargo build
# non-interactive
./target/debug/sned "fix the bug"

# TUI
sned --base-url "https://your-custom-endpoint/v1" --model qwen3.6-27b --api-key "apikeygoeshere 
```

Default parsers:

```text
rust, javascript, python, typescript, go
```

More parsers:

```bash
cargo build --features "lang-c,lang-cpp,lang-ruby,lang-java,lang-php,lang-swift"
```

Faster rebuilds:

```bash
cargo install sccache
export RUSTC_WRAPPER=sccache
```

## run

```bash
sned "fix the bug" --act
sned "explain this module"
sned --continue
sned --task-id <id>
```

Provider examples:

```bash
sned "fix the bug" --provider anthropic --model <model>

sned "fix the bug" \
  --api-key sk-xxx \
  --base-url https://example.com/v1 \
  --model qwen3-coder

export OPENAI_API_KEY=sk-xxx
export OPENAI_API_BASE=https://example.com/v1
sned --model qwen3-coder
```

Other useful flags:

```text
--yolo                   # no approval prompts, run anything without asking
--plan                   # produce a plan, get approval, then execute
--subagents              # delegate parts of the task to subagents in parallel
--json                   # one JSON object per event on stdout, no TUI
--export <path>          # write the full session transcript to <path>
--double-check-completion # re-read the changes after the model claims done
--track-changes          # snapshot files into shadow git for undo
--verbose                # surface internal events to the TUI/log
--no-token-display       # suppress the 50/80/95% context-window warnings
--thinking <budget>      # ask the model to spend up to <budget> tokens thinking first
```

## Overview

### Hash-anchored edits

When the model reads a file, sned records a hash for each line it saw. When it sends an edit back, sned checks those hashes again before writing. If the file changed in the meantime, the edit is rejected and the model has to read the file again.

This is there because line-based patching falls apart once the file moves under you.

Anchors are cached across restarts in:

Anchors survive restarts:

```text
~/.sned/data/cache/anchors.json
```

### AST-native precision

For supported languages, symbol work goes through tree-sitter.

A rename is not a text replacement. It operates on code symbols, so renaming `foo` should not rewrite comments, strings, or unrelated names that happen to contain `foo`.

Default language support:

```text
rust, javascript, python, typescript, go
```

Extra language support is feature-gated to keep the default binary smaller.

### multi-file batching

The model can send one batch edit touching multiple files.

Each file is checked independently. One bad anchor does not hide the others. The result reports what applied, what failed, and what needs to be re-read.

### shadow git

`--track-changes` creates a separate git repo for agent work:

```text
.sned/.git-agent/
```

It is a real git repo, but it is not your project `.git/`.

Each agent turn becomes a commit in the shadow repo. That gives the agent a real undo system without touching your real history.

Useful commands:

```text
/undo
/diff
/log
/commit "message"
/checkpoint list
/checkpoint restore N
/checkpoint undo
```

`/commit "message"` applies the last accepted turn to your real repo when you want it.

### Qwen handling

The generic Claude/GPT-style prompts in tools I've used made qwen invent tool names, e.g. a qwen agent would try `readFile` when the schema wants `read_file`, or emit `{"file_path": "src/main.rs"}` when the parser wants `{"paths": ["src/main.rs"]}`. sned detects the Qwen family and injects toolcall examples in the format it actually parses.

sned detects Qwen family model IDs, including routed names like `openai/qwen-...`, and adds Qwen-specific tool-call examples to the system prompt. 
Non-Qwen models keep the normal prompt path. 

You can prepend local system text with:


```bash
export SNED_SYSTEM_MD=/path/to/system.md
```

sned separates that content from the built-in prompt with `---`.

### context management

sned tracks what the model has read, what changed, and what is stale.

Before each API call it compacts context instead of just chopping tokens from the front.

Sessions live here:

```text
~/.sned/data/
```

### hooks

Hooks exist for people who want them:

```text
PreToolUse
PostToolUse
TaskStart
TaskCancel
TaskComplete
TaskResume
PreCompact
```

Use `--hooks-dir` to load a hook directory.

Workspace hooks are opt-in.

### command safety

Commands run through an approval gate.

Safe commands can be auto-approved for the session. Risky commands require review. Some commands are hard-denied.

```bash
export SNED_SAFE_COMMANDS="npm,pnpm,yarn,cargo,make"
```

Hard-denied commands include:

```text
rm, dd, mkfs, curl, wget, nc, ncat, netcat, ssh, sudo,
chmod, chown, kill, killall, reboot, shutdown, poweroff,
insmod, rmmod, modprobe, apt-get, yum, dnf, apt
```

`--yolo` does not bypass the hard deny list.

### environment sandbox

Model-run commands get a filtered environment.

Allowed by default:

```text
PATH, HOME, USER, LANG, LC_ALL, TERM, TERM_PROGRAM, TZ, SHELL,
PWD, TMPDIR, XDG_CACHE_HOME, XDG_CONFIG_HOME, XDG_DATA_HOME,
XDG_STATE_HOME, EDITOR, VISUAL, PAGER, LESS, MORE, LOGNAME,
HOSTNAME, DOCKER_HOST, CARGO_HOME, RUSTUP_HOME, GOPATH,
PYTHONPATH, NODE_PATH, NPM_CONFIG_PREFIX
```

API keys and tokens are not passed through unless you opt in:

```bash
export SNED_ALLOW_ENV="API_KEY,AWS_ACCESS_KEY_ID,MY_CUSTOM_VAR"
```

`SNED_*` internal vars are dropped.

## interactive mode

- Slash commands autocomplete.
- `Tab` or `Enter` accepts partial matches like `/pl` -> `/plan`.
- Reasoning state is shown without dumping token noise into the main view.
- Context usage lives in the status bar.
- `--export <path>` writes the transcript even if the turn errors.

## env vars

| Variable | Purpose | Default |
|---|---|---|
| `SNED_ALLOW_ENV` | Comma-separated env vars to pass through sandbox | none |
| `SNED_SAFE_COMMANDS` | Comma-separated commands to auto-approve | none |
| `SNED_STREAM_OUTPUT_LINES` | Live streaming output line limit | `20` |
| `SNED_COMMAND_OUTPUT_LIMIT` | Command output truncation limit in bytes | `10240` |
| `SNED_SEARCH_TIMEOUT_SECS` | File search timeout | `30` |
| `SNED_FETCH_TIMEOUT_SECS` | Web fetch timeout | `30` |
| `SNED_HOOK_TIMEOUT_MS` | Hook execution timeout | `10000` |
| `SNED_DIR` | Config directory | `~/.sned` |
| `SNED_DATA_DIR` | Data directory | `~/.sned/data` |
| `SNED_NO_ALTERNATE_SCREEN` | Use inline viewport | unset |
| `SNED_SYSTEM_MD` | Extra system prompt file prepended before built-in prompt | unset |
| `RUST_LOG` | Log level filter | `sned=warn` |

## providers

| Provider | Env Var |
|---|---|
| Anthropic | `ANTHROPIC_API_KEY` |
| Gemini | `GEMINI_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| Minimax | `MINIMAX_API_KEY` |
| DeepSeek | `DEEPSEEK_API_KEY` |
| Groq | `GROQ_API_KEY` |
| OpenRouter | `OPENROUTER_API_KEY` |
| XAI | `XAI_API_KEY` |

Custom OpenAI-compatible endpoint:

```bash
sned \
  --base-url https://example.com/v1 \
  --api-key sk-xxx \
  --model <model>
```

## config

```text
~/.sned/       config, auth, settings
~/.sned/data/  task history and session state
```

CLI flags override env vars.

## test

```bash
cargo test
cargo test -p sned
```

## license

GPL-3.0-only OR Apache-2.0

This project is based on [Dirac](https://github.com/dirac-run/dirac) by Dirac Delta Labs, licensed under Apache 2.0.

Modifications, adaptations, and the Rust port are original work. See [LICENSE](./LICENSE) and [LICENSE-APACHE](./LICENSE-APACHE).
