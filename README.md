# sned

Rust CLI. No Node, no MCP, no runtime dependencies.

## build

```bash
cargo build
./target/debug/sned "hello"
```

First build downloads `vte` from crates.io. Needs internet once.

**Requires:** macOS or Linux, Rust 1.93+.

```bash
# language parsers (default: rust, javascript, python, typescript, go)
cargo build --features "lang-rust,lang-python,lang-javascript"

# more languages
cargo build --features "lang-c,lang-cpp,lang-ruby,lang-java,lang-php,lang-swift"

# terminal UI
cargo build --features terminal

# faster builds
cargo install sccache && export RUSTC_WRAPPER=sccache
```

## run

```bash
sned "fix the bug" --act                    # auto-execute tools
sned "hello" --provider anthropic --model claude-3-5-sonnet-20240620

# custom OpenAI-compatible endpoint
sned --api-key sk-xxx --base-url https://pass.wafer.ai/v1 --model Qwen3.5-397B-A17B

# or env vars
export OPENAI_API_KEY=sk-xxx
export OPENAI_API_BASE=https://pass.wafer.ai/v1
sned --model Qwen3.5-397B-A17B

# Gemini (native API)
sned --provider gemini --model gemini-3.1-pro-preview
export GEMINI_API_KEY=xxx
sned --provider gemini --model gemini-3-flash-preview --thinking 1024

# resume
sned --continue
sned --task-id <id>
```

Flags: `--yolo`, `--plan`, `--subagents`, `--json`, `--export <path>`, `--double-check-completion`, `--track-changes`, `--verbose`, `--no-token-display`, `--thinking <budget>`.

## what makes this different

**Hash-anchored edits.** Edits lock to content hashes at specific lines. If the file changed between read and edit, the anchor fails instead of silently patching the wrong code.

**AST-native tools.** Symbol lookup, rename, structural edits go through tree-sitter. Renaming `foo` won't touch a comment that mentions `foo`.

**Multi-file batch edits.** One tool call hits multiple files. Each file is independently anchored. Failures report per-file; successful edits don't get rolled back.

**Context compaction.** Tracks what's been read, edited, what's stale. Auto-condense on by default. Not naive truncation.

**Shadow git.** `--track-changes` auto-commits agent work to `.sned/.git-agent/`. Your actual `.git/` stays clean.

```
/undo              # revert last agent turn
/diff              # changes from last turn
/log               # last 10 turns
/commit "message"  # finalize into your real repo
```

**Hooks.** `PreToolUse`, `PostToolUse`, `TaskStart`, `TaskComplete`, `TaskResume`, `UserPromptSubmit`, `PreCompact`. Can block, modify args, or run side effects. `--hooks-dir` for runtime injection.

**Session resume.** `--continue` picks up the last session. `--task-id <id>` picks a specific one. State, history, context all restore.

**Output.** Compact tool display. OSC 8 hyperlinks (iTerm2, Kitty, WezTerm, VS Code). Context warnings at 80%/95%. Cost thresholds via `SNED_COST_WARN`. Edits to unread files get flagged. `/stats` for token usage and cost.

## command safety

`execute_command` checks against a safe list before auto-approving. Unsafe patterns (`rm`, `make`, command substitution, output redirection) require review.

Prompt: `(y/n/a)` — y approves once, n denies, a auto-approves for the session. Explicit `y` or `a` always runs — safety checks only gate auto-approval, never override user decisions.

Custom safe commands:
```bash
export SNED_SAFE_COMMANDS="npm,pnpm,yarn,cargo,make"
```

## test

```bash
cargo test
cargo test -p sned
```

## providers

| Provider | Env Var | Models | Notes |
|---|---|---|---|
| Anthropic | `ANTHROPIC_API_KEY` | claude-3-5-sonnet, claude-3-7-sonnet, claude-4 | Default |
| Gemini | `GEMINI_API_KEY` | gemini-3.1-pro-preview, gemini-3-flash-preview, gemini-2.5-pro | Native API, thought signatures |
| OpenAI | `OPENAI_API_KEY` | gpt-4o, gpt-4.1, o3, o4-mini | OpenAI-native or compatible |
| Minimax | `MINIMAX_API_KEY` | MiniMax-M2.7 | |
| DeepSeek | `DEEPSEEK_API_KEY` | deepseek-chat, deepseek-coder | |
| Groq | `GROQ_API_KEY` | llama-3.3-70b-versatile, mixtral-8x7b | |
| OpenRouter | `OPENROUTER_API_KEY` | anthropic/claude-sonnet-4.5, meta-llama/llama-3-70b | Aggregates providers |
| XAI | `XAI_API_KEY` | grok-3, grok-2 | |

Custom OpenAI-compatible: `--base-url` + `--api-key` works with any OpenAI-compatible API.

## config

- `~/.sned/` — config, auth, settings
- `~/.sned/data/` — task history, session state
- `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, etc.
- `OPENAI_API_KEY` + `OPENAI_API_BASE` for custom providers
- CLI flags override env vars: `--api-key`, `--base-url`
