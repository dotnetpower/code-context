---
title: "7 Native VS Code & MCP Tools"
description: "Reference for 7 native VS Code and MCP tools: context, grep, read, files, status, sync, and workspaces, with inline budgets and document anchors."
category: tools
verified: 2026-11-14
sidebar:
  order: 3
---

Code Context exposes **seven identical capabilities** across three local transports: VS Code Native Language Model Tools (`dotnetpower.code-context-native`), Model Context Protocol (`cctx shared-mcp`), and the `cctx` CLI.

## Tool mapping overview

| VS Code `toolReferenceName` | Internal Native ID | MCP Tool | CLI Subcommand | Mutates Index? |
|---|---|---|---|---|
| `#cctxNativeContext` | `code-context-native_codeContext` | `code_context` | `cctx --backend shared context` | Queues cold root nonblockingly |
| `#cctxNativeGrep` | `code-context-native_codeGrep` | `code_grep` | `cctx --backend shared grep` | Queues cold root nonblockingly |
| `#cctxNativeFiles` | `code-context-native_codeFiles` | `code_files` | `cctx --backend shared files` | Queues cold root nonblockingly |
| `#cctxNativeRead` | `code-context-native_codeRead` | `code_read` | `cctx --backend shared read` | Live disk fallback + hash check |
| `#cctxNativeStatus` | `code-context-native_codeStatus` | `code_status` | `cctx --backend shared status` | **Never** (purely observational) |
| `#cctxNativeSync` | `code-context-native_codeSync` | `code_sync` | `cctx --backend shared sync` | Blocking full-source barrier |
| `#cctxNativeWorkspaces` | `code-context-native_codeWorkspaces` | `code_workspaces` | `cctx workspaces` | **Never** (lists catalog) |

---

## 1. `#cctxNativeContext` / `code_context` (Primary Tool)

Searches saved source for a literal substring or regular expression and returns **block-clamped surrounding code** in a single round trip.

- **Key parameters:**
  - `pattern` *(required)*: Literal string by default; set `regex: true` for alternation (`foo|bar`) or regex patterns.
  - `root`: Explicit absolute workspace or Git worktree root.
  - `radius` (`0..100`): Requested context radius around matches, automatically clamped to clean function, struct, class, or heading boundaries so unrelated neighboring functions are excluded.
  - `limit` (`1..64`): Total match cap across the response.
  - `max_matches_per_file` (`1..64`): Caps noisy files and **prioritizes symbol definitions** (`fn`, `pub struct`, `class`, `def`, `interface`, `export function`, etc.) ahead of call-site noise.
  - `paths` / `glob` / `ignore_case`: Narrow scope to relative directory prefixes or file patterns.

### Supported polyglot block clamping

Boundary clamping understands declarations and structural blocks across **Rust, Python, TypeScript/JavaScript/JSX/TSX (React 19), Vue, Svelte, Go, Kotlin, Swift, Elixir, Ruby, SQL, Protobuf, Terraform, Makefile, Dockerfile, and Markdown**.

---

## 2. `#cctxNativeGrep` / `code_grep`

Fast trigram-indexed line search when you only need matching line numbers and text rather than surrounding code blocks.

- Uses ASCII and UTF-8 case-folded trigram postings (`src/resident.rs`) plus an HIR literal extractor (`src/planner.rs`) to eliminate >99% of non-matching files before running `grep-regex`.
- Pass `max_matches_per_file` (for example `5` or `10`) so generated or test fixtures cannot starve results from core modules, while keeping total output strictly below the `<8 KiB` host offload threshold.

---

## 3. `#cctxNativeFiles` / `code_files`

Sub-millisecond filename and relative-path discovery against the in-memory workspace catalog.

- Pass `glob` such as `"*.rs"`, `"*Service*.ts"`, `"**/*.test.tsx"`, or a bare stem.
- Combine with `paths: ["src", "crates"]` and `limit` (`1..1000`).

---

## 4. `#cctxNativeRead` / `code_read` & Rich Document Anchors

Batched, SHA-256-verified 1-based inclusive line reads with transparent live-disk recovery if a file was just edited moments before watcher reconciliation.

### `<8 KiB` inline budget & pagination

Host agent harnesses often offload tool outputs larger than `8 KiB` into temporary `/tmp` files, costing extra tool calls to inspect. Keep batched `ranges` to **`<= 150` total lines** for instant inline delivery, or pass `page: true` (and follow `next_ranges` until `complete: true`) for structured streaming pagination.

### Rich document virtual `#toc` & `#slide-N-M` anchors

`code_read` extracts clean, token-compact Markdown/text from binary and structured documents with **zero external runtime dependencies** (powered by an internal 9-bit $O(1)$ DEFLATE decoder that runs **5.4× faster**):

| Document Format | Supported Virtual Anchors | Token Savings |
|---|---|---|
| **Markdown (`.md`, `.mdx`)** | `docs/REFERENCE.md#native-tools`, `#toc` | Reads exact section slug without loading whole file |
| **PowerPoint (`.pptx`) & ODP** | `deck.pptx#toc`, `deck.pptx#slide-3`, `deck.pptx#slide-3-5` | **-86.3% to -98.9%** (hoists repeated slide headers/footers & compresses charts) |
| **Word (`.docx`), ODT & RTF** | `spec.docx#toc`, `spec.docx#outline`, `spec.docx#architecture`, `{#bookmark}` | Jumps straight to heading or bookmark span |
| **Excel (`.xlsx`) & ODS** | `metrics.xlsx#toc`, sheet & bounded table ranges | Prunes empty columns/rows automatically |
| **PDF, EPUB, Jupyter (`.ipynb`), SRT/VTT, XLIFF** | `#toc`, page/cell/cue ranges | Clean text projection without raw JSON/XML markup tax |

---

## 5. `#cctxNativeStatus`, `#cctxNativeSync` & `#cctxNativeWorkspaces`

- **`#cctxNativeStatus` / `code_status`**: Returns index state (`warm`, `loading`, `cold`), memory ceiling, cgroup v2 reserve, file counts, and latency telemetry **without triggering background indexing**.
- **`#cctxNativeSync` / `code_sync`**: Waits for a full source synchronization barrier. **Use only when proving exhaustive symbol absence** across an entire workspace—never call `code_sync` before ordinary lookups.
- **`#cctxNativeWorkspaces` / `code_workspaces`**: Lists registered roots, allowed directory scopes, globs, and active exclusion profiles.

Next, see how [`pctx` probes, `cctx journal`, and `cctx prompt`](../probes-and-journal/) eliminate non-code token waste.
