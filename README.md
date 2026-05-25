# sned

LLM CLI in Rust, no Node, no MCP, no runtime garbage. 

hash-anchored edits / AST-native precision / multi-file batching / auto context / shadow git / hooks / command safety

## build

```bash
cargo build
./target/debug/sned "fix the bug"
```

Default parsers: rust, javascript, python, typescript, go. 
More:
```bash
cargo build --features "lang-c,lang-cpp,lang-ruby,lang-java,lang-php,lang-swift"
```

Faster rebuilds:
```bash
cargo install sccache && export RUSTC_WRAPPER=sccache
```

## how it works

You give it a task. It opens a session, reads your workspace, calls tools, gets results, decides next step. The model drives the loop. You approve or deny.

Edits are hash-anchored. Every line gets a content hash on read. Before writing, sned re-hashes. If anything shifted, the edit fails. You get a conflict report, the agent retries. No silent wrong-line patching.

Context is managed automatically. Tracks what's been read, what changed, what's stale. Compacts before each API call instead of naively truncating.

Anchors persist across sessions. State saved to `~/.sned/data/cache/anchors.json`, loaded on startup.

Sessions live in `~/.sned/data/`. `--continue` resumes. `--task-id` targets a specific session.

## skills and rules

Scans for `.agents/`, `.claude/`, `.ai/`, `.codex/` directories. Loads `AGENTS.md` files and `SKILL.md` skills from these locations.

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

# Gemini
sned --provider gemini --model gemini-3.1-pro-preview
export GEMINI_API_KEY=xxx

# resume
sned --continue
sned --task-id <id>
```

Other flags: `--yolo`, `--plan`, `--subagents`, `--json`, `--export <path>`, `--double-check-completion`, `--track-changes`, `--verbose`, `--no-token-display`, `--thinking <budget>`.

## hash-anchored edits

Every line is hashed on read. Before writing, sned re-hashes. If the file changed between read and write, the hash won't match and the edit fails. You get a conflict report, the agent re-reads and retries. No silent wrong-line patching.

Anchors survive restarts (saved to `~/.sned/data/cache/anchors.json`).

## AST-native precision

Rename, references, structural edits go through tree-sitter. Renaming `foo` operates on actual code symbols, not text matches. Won't touch strings, comments, or variable names that happen to contain "foo".

## multi-file batch edits

One tool call, multiple files. Each independently anchored. Failures report per-file. Partial success is possible.

## context management

Auto-condenses before API calls. Tracks what's been read, edited, stale. Not naive truncation.

## shadow git

`--track-changes` maintains a shadow git repo at `.sned/.git-agent/`. Every agent turn is a real commit. Your real `.git/` is never touched.

Turns are real git commits. You can diff, log, undo, or checkpoint-restore to any turn. `/commit "message"` pushes the last turn to your real repo when you're satisfied. No more "please undo all that crap you just wrote".

```
/undo
/diff
/log
/commit "message"
/checkpoint list
/checkpoint restore N
/checkpoint undo
```

## hooks

PreToolUse, PostToolUse, TaskStart, TaskCancel, TaskComplete, TaskResume, PreCompact. Can block, modify args, run side effects. `--hooks-dir` to inject.

Workspace hooks: opt-in only, restricted env, warns if unverified.

## command safety

Safe-list for auto-approved commands. Unsafe patterns (rm, command substitution, output redirection) require manual review.

Prompt: y/n/a. Y approves once, n denies, a auto-approves for the session. Safety checks only gate auto-approval. Never override your explicit decision.

Custom safe commands:
```bash
export SNED_SAFE_COMMANDS="npm,pnpm,yarn,cargo,make"
```

Some commands are always denied regardless of SNED_SAFE_COMMANDS or explicit approval: rm, dd, mkfs, curl, wget, nc, ncat, netcat, ssh, sudo, chmod, chown, kill, killall, reboot, shutdown, poweroff, insmod, rmmod, modprobe, apt-get, yum, dnf, apt. This list cannot be bypassed. Not by settings, not by --yolo, not by explicit user approval. Hardcoded deny-only.

## providers

| Provider | Env Var | Example Models |
|---|---|---|
| Anthropic | ANTHROPIC_API_KEY | claude-sonnet-4-6, claude-haiku-4-5 |
| Gemini | GEMINI_API_KEY | gemini-3.1-pro-preview, gemini-2.5-pro |
| OpenAI | OPENAI_API_KEY | gpt-4o, gpt-4.1, o3, o4-mini |
| Minimax | MINIMAX_API_KEY | MiniMax-M2.7, MiniMax-M2.5 |
| DeepSeek | DEEPSEEK_API_KEY | deepseek-chat, deepseek-reasoner |
| Groq | GROQ_API_KEY | llama-3.3-70b-versatile, llama-3.1-8b-instant |
| OpenRouter | OPENROUTER_API_KEY | various (100+ models) |
| XAI | XAI_API_KEY | grok-4.3, grok-4.20 |

Model support changes frequently. Check provider docs for latest availability.

Custom OpenAI-compatible endpoint: `--base-url` + `--model` +  `--api-key`

## config

- `~/.sned/`: config, auth, settings
- `~/.sned/data/`: task history, session state
- API keys via env vars (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, etc)
- CLI flags override env vars: `--api-key`, `--base-url`

## test

```bash
cargo test
cargo test -p sned
```

## license

GPL-3.0-only OR Apache-2.0

This project is based on [Dirac](https://github.com/dirac-run/dirac) by Dirac Delta Labs, licensed under Apache 2.0.
Modifications, adaptations, and Rust port are original work. See [LICENSE](./LICENSE) and [LICENSE-APACHE](./LICENSE-APACHE).