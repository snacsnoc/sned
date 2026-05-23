# sned

Rust CLI for LLM APIs. No Node, no MCP, no runtime deps.

Ported from dirac/dirac-run (TypeScript). Hash-anchored edits, AST tools, context management, and batch editing. Built in Rust from scratch.

## build

```bash
cargo build
./target/debug/sned "fix the bug"
```

First build pulls `vte` from crates.io. Needs network once. macOS or Linux, Rust 1.93+.

Default language parsers: rust, javascript, python, typescript, go.

```bash
# more parsers
cargo build --features "lang-c,lang-cpp,lang-ruby,lang-java,lang-php,lang-swift"

# faster rebuilds
cargo install sccache && export RUSTC_WRAPPER=sccache
```

## what happens

You give sned a task. It opens an interactive session and loops: reads your workspace, calls tools, gets results, decides next step. The model runs the loop. You approve or deny tool calls.

Edits are hash-anchored. Every line gets a content hash when you read a file. Before writing, sned re-hashes. If anything moved, the edit fails — no silent wrong-line patching. You get a conflict report, the agent re-reads and retries.

Context is managed automatically. Sned tracks what you've read, what changed, what's stale. It compacts before each API call instead of just truncating.

Sessions live in `~/.sned/data/`. State, history, context all persist. `--continue` picks up where you left off. `--task-id` targets a specific session.

Hooks run at loop boundaries: PreToolUse can validate or modify args before a tool fires. PostToolUse can post-process. TaskComplete can trigger side effects. A hook can also stop a dangerous call dead.

## run

```bash
sned "fix the bug" --act
sned "hello" --provider anthropic --model claude-3-5-sonnet-20240620

# custom OpenAI-compatible endpoint
sned --api-key sk-xxx --base-url https://pass.wafer.ai/v1 --model Qwen3.5-397B-A17B

# env vars work too
export OPENAI_API_KEY=sk-xxx
export OPENAI_API_BASE=https://pass.wafer.ai/v1
sned --model Qwen3.5-397B-A17B

# Gemini native API
sned --provider gemini --model gemini-3.1-pro-preview
export GEMINI_API_KEY=xxx
sned --provider gemini --model gemini-3-flash-preview --thinking 1024

# resume
sned --continue
sned --task-id <id>
```

Other flags: `--yolo`, `--plan`, `--subagents`, `--json`, `--export <path>`, `--double-check-completion`, `--track-changes`, `--verbose`, `--no-token-display`, `--thinking <budget>`.

## hash-anchored edits

The core idea: edits lock to content hashes at line level. File changed between read and write? Anchor fails, edit rejected. No silent patching of the wrong code.

## AST-native precision

Symbol rename, references, structural edits go through tree-sitter. Renaming `foo` won't touch every string containing "foo" — it operates on actual code symbols.

## multi-file batch edits

One tool call, multiple files. Each file is independently anchored. Failures report per-file. Successes don't roll back if a later file fails.

## high-bandwidth context

Tracks what's been read, edited, and what's stale. Auto-condenses before sending to the model — not naive truncation. Keeps the signal dense.

## shadow git

`--track-changes` auto-commits agent work to `.sned/.git-agent/` as it goes. Your real `.git/` stays untouched. When the agent goes off the rails, you can undo without drama.

```
/undo              # undo last agent turn
/diff             # show changes from last turn
/log              # last 10 turns
/commit "message" # move into your real repo
```

## hooks

PreToolUse, PostToolUse, TaskStart, TaskComplete, TaskResume, UserPromptSubmit, PreCompact. Can block, modify args, or run side effects. `--hooks-dir` to inject at runtime.

## command safety

`execute_command` has a safe-list for auto-approved commands. Unsafe patterns (rm, make, command substitution, output redirection) require manual review.

Prompt: `y/n/a`. Y approves once, n denies, a auto-approves for the session. Safety checks only gate auto-approval — they never override your explicit decision.

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
- API keys via env vars (ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY, etc.)
- CLI flags override env vars: `--api-key`, `--base-url`

## test

```bash
cargo test
cargo test -p sned
```
