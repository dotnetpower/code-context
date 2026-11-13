---
title: "Frequently Asked Questions (FAQ)"
description: "Direct technical answers about Code Context (cctx): comparisons with ripgrep, LSP, and vector RAG, VS Code Native vs MCP, token & AI Credit savings, Windows support, and privacy."
category: evidence
verified: 2026-11-14
sidebar:
  order: 7
---

## How is Code Context (`cctx`) different from `ripgrep` (`rg`)?

`ripgrep` is a fast stateless CLI scanner—Code Context actually uses `rg` during initial authorized workspace discovery. However, in multi-turn coding agents, raw `rg` forces a costly two-step loop (`rg` match lines → multiple 100–200 line `view` calls) and re-scans files on every turn.

`cctx` keeps a **warm in-memory trigram index** (`17.22×` faster mixed retrieval) and returns **AST block-clamped definitions** in **one call** (`#cctxNativeContext` / `code_context`), cutting lookup payload tokens by **73.2% to 91.3%** and agent round-trip tool calls by **61.9%**.

---

## How does `cctx` compare to vector embeddings / semantic RAG?

Vector RAG requires chunking, embedding models (often cloud APIs or heavy local GPUs), approximate nearest-neighbor indexes that lag behind rapid branch switches, and fuzzy similarity matches that frequently miss exact symbol names, error codes, or regex patterns.

`cctx` is **100% deterministic, exact, local, and SHA-256 verified**: zero API keys, zero GPU overhead, sub-millisecond warm queries, and instant incremental updates on file save.

---

## Do I need MCP if I install the VS Code extension?

For foreground VS Code Copilot sessions (VS Code 1.140+), **no MCP configuration is required**—the Marketplace extension (`dotnetpower.code-context-native`) registers native Language Model Tools directly.

Adding `cctx shared-mcp` is recommended when you also run **background/unattended subagents** without UI extension-host dispatch, or external MCP clients such as **Claude Code, Cursor, or custom CLI agents**. Both surfaces share the exact same background Rust daemon and warm cache.

---

## Does `cctx` upload my code or telemetry anywhere?

**No.** All indexing, trigram postings, document extraction, `pctx` probes, `cctx journal` entries, and `cctx usage` analytics run 100% locally on your machine. No source code, filenames, prompts, or metrics are transmitted to external servers.

---

## Why does `code_read` show `0.43×` (slower) on small known-file reads in the benchmark?

Because direct in-process buffered reads of a tiny file already in the OS page cache take ~`0.081 ms`, whereas sending a request over a Unix socket / loopback RPC with policy validation, SHA-256 verification, and JSON serialization takes ~`0.190 ms`. We keep small known reads in the benchmark suite deliberately so our reported **`17.22×` mixed speedup** reflects realistic agent workloads rather than cherry-picked search-only numbers.

---

## What happens when `cctx` returns `CACHE_MISS`?

`CACHE_MISS` means the workspace root was cold and has just been queued for bounded background indexing—**never** that the symbol or file is absent. The agent performs a single normal host read/search for that immediate turn without waiting, and automatically gets warm `0.23 ms` hits on subsequent turns.

---

## Does `cctx` run natively on Windows without WSL?

**Yes.** Build `cctx.exe` with `cargo build --locked --release` and place it at `%USERPROFILE%\.local\bin\cctx.exe`. The VS Code extension starts `cctx.exe daemon` automatically on demand and uses `rg` on `PATH` or VS Code's bundled `rg.exe`. WSL2 and Windows Agent Sessions are also fully supported—see [Installation & Platform Setup](../installation/).

---

## How does `cctx` relate to `agent-tokenomics` and `tokprof`?

[`agent-tokenomics`](https://dotnetpower.github.io/agent-tokenomics/) is our companion engineering playbook and zero-code LLM request profiler (`tokprof`). Use **`cctx`** inside your IDE and MCP clients to cut lookup tokens, clamp context, and accelerate coding tasks; use **`tokprof`** when building or operating custom agents to attribute every input token across system prompts, tool schemas, and conversation history.
