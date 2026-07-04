# sned Scripts

Helper scripts for profiling, memory analysis, TUI testing, build, signing, and benchmarking.

## Quick guide

| Need to... | Run |
|---|---|
| Profile memory | `./scripts/profile-memory.sh --workload all` |
| Analyze a dhat heap dump | `./scripts/analyze-dhat-heap.sh dhat-heap.json` |
| Test the TUI | `./scripts/tui-smoke-test.sh` |
| Build a universal macOS binary | `./scripts/build-universal-macos.sh` |
| Build and package Linux amd64 | `./scripts/build-linux-amd64.sh` |
| Build and package FreeBSD amd64 | `./scripts/build-freebsd-amd64.sh` |
| Sign a macOS binary | `./scripts/sign-macos.sh` |
| Notarize a macOS binary | `./scripts/notarize-macos.sh` |
| Update the Homebrew formula | `./scripts/homebrew-formula.sh` |
| Run the opencode comparison benchmark | `./scripts/bench-vs-opencode.sh` |

The remaining files (`sned-pty-helper`, `sned-pty-helper.c`) are an internal helper used by `tui-smoke-test.sh`. You don't call them directly.

---

## Memory profiling

### `profile-memory.sh`

Runs sned through several workloads under dhat and produces a heap allocation summary per workload.

```bash
./scripts/profile-memory.sh --workload all       # basic + edit + search
./scripts/profile-memory.sh --workload edit      # just the edit workload
./scripts/profile-memory.sh --workload all --keep-json   # keep raw dhat-heap.json
```

Output goes to `target/memory-profiles/profile-<timestamp>/`:

```
├── summary.txt         # verdict + per-category counts
├── allocations.txt     # breakdown by category
├── dhat-heap.json      # raw data (only with --keep-json)
└── build.log           # compile log
```

When to run it:
- After a feature lands that touches allocations
- After changing file editing or anchor reconciliation code
- After changing symbol indexing or tree-sitter usage
- When memory usage feels off

Dependencies: `python3`, `jq`.

### `analyze-dhat-heap.sh` / `analyze-dhat-heap.py`

Standalone dhat analyzer. Use this directly when you already have a `dhat-heap.json` from somewhere else.

```bash
./scripts/analyze-dhat-heap.sh dhat-heap.json
```

The `.sh` is the entry point — it handles argument validation and calls `analyze-dhat-heap.py` for the actual analysis. Don't call the `.py` directly.

`profile-memory.sh` calls this automatically; you only need to run it standalone if you produced `dhat-heap.json` some other way (e.g. `cargo run --features dhat-heap`).

Output is a categorized allocation report with a HEALTHY / WARNING / CRITICAL verdict. Library and runtime noise is filtered out — only application code with significant unfreed allocations is flagged.

---

## TUI testing

### `tui-smoke-test.sh`

Smoke test for the ratatui interactive shell. Builds sned, runs it inside a pty harness, sends `/exit`, and checks a handful of CLI paths.

```bash
./scripts/tui-smoke-test.sh                     # run everything
./scripts/tui-smoke-test.sh --test tui-startup-exit   # run one test
./scripts/tui-smoke-test.sh --list              # list all test names
./scripts/tui-smoke-test.sh --verbose           # capture full pty/cmd output
```

Run it after any change to `src/cli/interactive.rs`, anything under `src/cli/tui/`, anything that touches the OutputWriter / channel / drain pipeline, or anything that changes agent streaming. It uses the mock provider, so no API key or network is needed.

The `sned-pty-helper` binary is required; if it's missing the test prints the rebuild command:

```bash
gcc -o scripts/sned-pty-helper scripts/sned-pty-helper.c
chmod +x scripts/sned-pty-helper
```

### `sned-pty-helper` / `sned-pty-helper.c`

Small C helper that sets the pty window size (TIOCSWINSZ ioctl) and then execs sned. Called by `tui-smoke-test.sh` so ratatui gets a stable 80x24 viewport. Not for direct use.

Rebuild if missing:

```bash
gcc -o scripts/sned-pty-helper scripts/sned-pty-helper.c
chmod +x scripts/sned-pty-helper
```

Dependencies: `python3`, `gcc`/`clang` (only for the rebuild).

---

## macOS release

### `build-universal-macos.sh`

Builds a universal (arm64 + x86_64) release binary.

### `build-release-package.sh`

Shared helper used by the Linux and FreeBSD packaging wrappers. Builds a
single target triple and writes a tar.gz release artifact under
`target/dist/<suffix>/`.

### `build-linux-amd64.sh`

Builds `x86_64-unknown-linux-gnu` and packages `sned-<version>-linux-amd64.tar.gz`.

### `build-freebsd-amd64.sh`

Builds `x86_64-unknown-freebsd` and packages `sned-<version>-freebsd-amd64.tar.gz`.

### `sign-macos.sh`

Signs the binary with the Apple Developer ID. Requires `APPLE_SIGNING_IDENTITY` and `APPLE_SIGNING_KEYCHAIN_PROFILE` env vars (or use `xcrun notarytool` setup).

### `notarize-macos.sh`

Submits the signed binary to Apple notarization service and staples the ticket.

### `homebrew-formula.sh`

Regenerates the Homebrew formula after a release. Run from the sned repo root after tagging a release.

---

## Benchmarking

### `bench-vs-opencode.sh`

End-to-end comparison between sned and opencode on a set of representative tasks. Output goes to `target/bench-vs-opencode/`.

### `bench-fixtures/`

Sample inputs used by `bench-vs-opencode.sh`. Three sizes: `trivial.txt`, `medium.txt`, `long.txt`.

### Criterion benches

`cargo bench --bench <name>` runs the per-subsystem Criterion benchmarks. Each bench file under `benches/` corresponds to one benchmark target (anchor reconciliation, edit application, memory, etc.).

---

## Dependencies

| Tool | Required by | Install |
|---|---|---|
| `jq` | profile-memory.sh, analyze-dhat-heap.sh | `brew install jq` |
| `python3` | profile-memory.sh, analyze-dhat-heap.py | pre-installed on macOS |
| `gcc`/`clang` | rebuilding sned-pty-helper | Xcode Command Line Tools |
| `cargo` | bench-vs-opencode.sh, criterion benches | `rustup` |

---

## Troubleshooting

**dhat-heap.json not found after profiling.** The workload may not have triggered heap allocations. Try `--workload all` and check that `dhat-heap` is enabled in `Cargo.toml`.

**sned-pty-helper missing.** Run the gcc command shown in the TUI smoke test section above.

**macOS signing fails.** Verify `APPLE_SIGNING_IDENTITY` is set and that your keychain profile is configured per `xcrun notarytool store-credentials`.

---

## Adding new scripts

1. Name: `kebab-case.sh` or `kebab-case.py`.
2. Shebang: `#!/usr/bin/env bash` or `#!/usr/bin/env python3`.
3. Make executable: `chmod +x scripts/<name>`.
4. Add an entry to this file.
5. Bash scripts should use `set -euo pipefail`.
6. Include usage info on `--help` or when called without arguments.
