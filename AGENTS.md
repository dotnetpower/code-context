# AGENTS.md — Code Context (`cctx`)

## Overview

`code-context` (`cctx`) is a shared local Rust source index and adaptive memory cache for AI coding agents, paired with a native VS Code Language Model Tools extension (`dotnetpower.code-context-native`) and an MCP server (`cctx shared-mcp`).

- **Rust backend** ([src/](src)): Trigram posting index ([src/resident.rs](src/resident.rs)), regex/case-folded HIR query planner ([src/planner.rs](src/planner.rs), [src/query.rs](src/query.rs)), smart block-clamped context assembly ([src/context.rs](src/context.rs)), incremental watcher reconciliation ([src/watched.rs](src/watched.rs)), adaptive memory controller ([src/memory.rs](src/memory.rs)), BPE-aligned formula prompt compiler ([src/prompt_formula.rs](src/prompt_formula.rs)), and daemon/IPC/MCP transports ([src/daemon.rs](src/daemon.rs), [src/shared_mcp.rs](src/shared_mcp.rs)).
- **VS Code extension** ([vscode-extension/](vscode-extension)): Contributes seven native Language Model Tools (`#cctxNativeContext`, `#cctxNativeGrep`, `#cctxNativeRead`, `#cctxNativeFiles`, `#cctxNativeStatus`, `#cctxNativeSync`, `#cctxNativeWorkspaces`) and automatic agent instructions ([vscode-extension/prompts/code-context.instructions.md](vscode-extension/prompts/code-context.instructions.md) with `applyTo: "**"`).

## Recommended Tool Usage for Agents

1. **Native-first priority in VS Code**: In foreground VS Code sessions, ALWAYS prefer `#cctxNativeContext`, `#cctxNativeRead`, `#cctxNativeGrep`, and `#cctxNativeFiles` (`code-context-native_*`) FIRST before MCP (`code_*`) or built-in search/read tools. Pass `root` explicitly when the project or worktree path is known, and keep read batches `<=150` lines (`<8KiB`) so outputs remain inline.
2. **Symbol or literal lookup + surrounding code**: Use `#cctxNativeContext` (or MCP `code_context`) FIRST to search and read in a single turn instead of separate `rg`/`grep` and `read_file`/`view` calls.
3. **Filename discovery**: Use `#cctxNativeFiles` (or MCP `code_files`) with a glob (`*.rs`, `*.py`, `*Service*.ts`).
4. **Exact or regex line search**: Use `#cctxNativeGrep` (or MCP `code_grep`). Set `regex: true` for alternation (`a|b`) or regex syntax; set `max_matches_per_file` to prevent noisy files from starving results and to prioritize symbol definitions.
5. **Known line ranges**: Use `#cctxNativeRead` (or MCP `code_read`) for batched 1-based inclusive line ranges with SHA-256 verification (supports `path:line:col`, `path#Lstart-Lend`, and Markdown section anchors such as `docs/REFERENCE.md#native-tools`).
6. **Cold cache handling**: `CACHE_MISS` means the requested root was queued for background loading, never zero matches or a missing file. Continue that single lookup with permitted host search/read tools without polling or running `code_sync` to wait.
7. **Subagents & deferred tool discovery**: Built-in subagents (`explore`, `general-purpose`, `code-review`, `rubber-duck`, `research`, `security-review`, `task`) may run different model families (Opus, Sonnet, Haiku, Terra, Luna) without custom workspace instructions — always pass the explicit absolute `root` in the delegation prompt and instruct the subagent to use `#cctxNative*` or `code-context-code_*` (loading via `tool_search_tool` first if hidden) before `view`/`rg`/`grep`. When tools are deferred (even if `grep`/`view`/`glob` are visible on turn 0), call `tool_search_tool` (`tool_search`) FIRST: query `"cctxNativeContext cctxNativeRead cctxNativeGrep cctxNativeFiles"` in foreground VS Code sessions, or `"code-context-code_context code-context-code_read code-context-code_grep code-context-code_files"` (regex `"cctxNative|code-context"`) when native dispatch is unavailable.
8. **BPE-aligned formula prompts (`cctx prompt`)**: Use `cctx --root /path/to/project prompt --style formula --target instructions` (or `--target mcp`, `subagent-yaml`, `subagent-md`, `claude-md`) and `cctx prompt --compress "<task>" --json` to render BPE-aligned symbolic instructions or compress English/Korean task prompts (`[ko] :: ...`) while preserving core domain nouns. Subagent definitions across WSL and Windows VS Code SDK bundles can be patched in formula mode via `python3 scripts/configure_subagents.py --style formula` (or `CCTX_PROMPT_STYLE=formula`).

## Build, Test, and Verification Commands

Run the smallest relevant check during iteration (prefer `#tctxNativeRun` with modified files or `cargo test --test <target> --locked`), and run the full verification suite before packaging or releasing:

```sh
# Fast targeted Rust test during iteration (e.g. planner, file_discovery, document_support)
cargo test --test planner --locked

# Full Rust backend unit & integration tests and clippy lint
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings

# Python benchmark & documentation contract tests
python3 -m unittest discover -s tests -p 'test_*.py'

# VS Code extension typecheck, lint, unit tests, and VSIX packaging
cd vscode-extension
npm run check
npm run package
cd ..
python3 scripts/verify_vsix.py vscode-extension/dist/code-context-native.vsix
```

## Repository Invariants

- Keep [README.md](README.md) and [vscode-extension/README.md](vscode-extension/README.md) at **100 lines or fewer** ([tests/test_benchmark.py](tests/test_benchmark.py)).
- Never commit machine-specific user paths in public markdown documentation; use portable placeholders such as `/path/to/project` or `$HOME`.
- Every relative link and image target in markdown files must resolve to an existing path in the repository.
- Preserve stable VS Code tool contributions (no `enabledApiProposals` or `languageModelToolSets` in [vscode-extension/package.json](vscode-extension/package.json)).
