# sned

Rust CLI for LLM APIs. No Node, no MCP, no runtime deps.

Originally ported from dirac/dirac-run (TypeScript). Features like hash-anchored edits, AST-based tools, context management, tool system, and batch editing trace back to that source. Built in Rust from the ground up.

## build

```bash
cargo build
./target/debug/sned "fix the bug"
```

First build pulls `vte` from crates.io. Needs network once.

Requires: macOS or Linux, Rust 1.93+.

Language parsers included by default: rust, javascript, python, typescript, go.

```bash
# add more
cargo build --features "lang-c,lang-cpp,lang-ruby,lang-java,lang-php,lang-swift"

# terminal UI (needs zig)
cargo build --features terminal

# faster compiles
cargo install sccache && export RUSTC_WRAPPER=sccache
```

## how it works

Give sned a task, it opens an interactive session.

It loops: reads your workspace, calls tools, gets results, decides what to do next. The model runs the loop. You just approve or deny tool calls.

Edits are hash-anchored. When you call edit on a file, sned records hashes for each line. Before writing, it re-hashes. If anything shifted, the edit fails instead of silently patching the wrong line. You'll get a conflict report and the agent will re-read and retry.

Context is managed automatically. Sned tracks what files you've read, what changed, what's stale. It compacts context before each API call, not just truncation, removing noise.

Sessions are stored in `~/.sned/data/`. State, history, and context all persist. Use `--continue` to pick up where you left off, or `--task-id` to target a specific session.

Hooks run at specific points in the loop. PreToolUse can validate or modify arguments before a tool fires. PostToolUse can post-process results. TaskComplete can trigger side effects. A hook can also stop a dangerous tool call cold.

## run

```bash
sned "fix the bug" --act
sned "hello" --provider anthropic --model claude-3-5-sonnet-20240620

# custom OpenAI-compatible endpoint
sned --api-key sk-xxx --base-url https://pass.wafer.ai/v1 --model Qwen3.5-397B-A17B

# env vars also work
export OPENAI_API_KEY=sk-xxx
export OPENAI_API_BASE=https://pass.wafer.ai/v1
sned --model Qwen3.5-397B-A17B

# Gemini native API
sned --provider gemini --model gemini-3.1-pro-preview
export GEMINI_API_KEY=xxx
sned --provider gemini --model gemini-3-flash-preview --thinking 1024

# resume previous session
sned --continue
sned --task-id <id>
```

Other flags: `--yolo`, `--plan`, `--subagents`, `--json`, `--export <path>`, `--double-check-completion`, `--track-changes`, `--verbose`, `--no-token-display`, `--thinking <budget>`.

## what this does differently

**Hash-anchored edits.** Edits lock to content hashes at line level. If the file changed between read and write, the anchor fails and the edit is rejected. No silent wrong-code patching.

**AST-native precision.** Symbol rename, references, and structural edits go through tree-sitter. Renaming `foo` won't rename every string containing "foo". It operates on actual code symbols.

**Multi-file batch edits.** One tool call can hit multiple files. Each file is independently anchored. Failures report per-file; successes don't roll back on later errors.

**High-bandwidth context.** Tracks what's been read, edited, and what's stale. Auto-condenses before sending to the model, not naive truncation. Keeps context signal dense.

**Shadow git.** `--track-changes` auto-commits agent work to `.sned/.git-agent/` as it goes. Your actual `.git/` stays clean. Useful when the agent goes off the rails or you want to review changes before touching real history.

```
/undo              # undo last agent turn
/diff             # show changes from last turn
/log              # last 10 turns
/commit "message" # move into your real repo
```

**Hooks.** PreToolUse, PostToolUse, TaskStart, TaskComplete, TaskResume, UserPromptSubmit, PreCompact. Can block, modify args, or run side effects. `--hooks-dir` to inject at runtime.

**Session resume.** `--continue` picks up the last session. `--task-id` picks a specific one. State, history, context restore.

## command safety

`execute_command` has a safe-list for auto-approved commands. Unsafe patterns (rm, make, command substitution, output redirection) require manual review.

Prompt: `y/n/a`. Y approves once, n denies, a auto-approves for the session. Safety checks only gate auto-approval, never override your explicit decision.

Custom safe commands:
```bash
export SNED_SAFE_COMMANDS="npm,pnpm,yarn,cargo,make"
```

## providers

| Provider | Env Var | Models |
|---|---|---|
| Anthropic | ANTHROPIC_API_KEY | claude-3-5-sonnet, claude-3-7-sonnet, claude-4 |
| Gemini | GEMINI_API_KEY | gemini-3.1-pro-preview, gemini-3-flash-preview, gemini-2.5-pro |
| OpenAI | OPENAI_API_KEY | gpt-4o, gpt-4.1, o3, o4-mini |
| Minimax | MINIMAX_API_KEY | MiniMax-M2.7 |
| DeepSeek | DEEPSEEK_API_KEY | deepseek-chat, deepseek-coder |
| Groq | GROQ_API_KEY | llama-3.3-70b-versatile, mixtral-8x7b |
| OpenRouter | OPENROUTER_API_KEY | various |
| XAI | XAI_API_KEY | grok-3, grok-2 |

Custom OpenAI-compatible endpoint: `--base-url` + `--api-key`.

## config

- `~/.sned/`: config, auth, settings
- `~/.sned/data/`: task history, session state
- API keys via env vars: ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY, etc.
- CLI flags override env vars: `--api-key`, `--base-url`

## test

```bash
cargo test
cargo test -p sned
```