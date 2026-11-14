---
title: "Zero-Schema Probes (pctx), Journal & Formula Prompts"
description: "Use pctx to inspect JSON, APIs, tables, images, and logs; preserve compact continuity with cctx journal and compile formula prompts."
category: tools
verified: 2026-11-14
sidebar:
  order: 4
---

Coding agents lose tens of thousands of tokens per session on tasks outside source code: inspecting 1 MB API payloads, writing throwaway `python3 -c` or `jq` scripts, re-running noisy test suites, and losing architectural state after context compaction.

Code Context solves all three in native Rust CLI subcommands (`pctx` / `cctx probe`, `cctx journal`, and `cctx prompt`) with **`0` standing MCP tool-schema tokens** and **`0` LLM script-authoring turns**.

---

## 1. Declarative Probes (`pctx` / `cctx probe`)

`pctx` (installed alongside `cctx` as a direct alias for `cctx probe`) gives agents six deterministic inspection primitives with automatic secret/token scrubbing and ANSI stripping:

| Command | Replaces | Example Usage |
|---|---|---|
| **`pctx json`** | `jq`, `python3 -c "import json..."` | `pctx json response.json --where status=500 --count-by service` |
| **`pctx http`** | Multi-curl loops & OpenAPI/STAC scripts | `pctx http "https://api.example.com/{svc}/health" --var svc=auth,billing` |
| **`pctx table`** | Ad-hoc `sqlite3` / Parquet / CSV Python scripts | `pctx table metrics.db --sql "SELECT model, AVG(ms) FROM runs GROUP BY 1"` |
| **`pctx image`** | Blindly attaching multi-megabyte PNGs | `pctx image ui-after.png --expect 1440x900` (dimensions, channels, exposure check) |
| **`pctx batch`** | Raw multi-command log dumps | `pctx batch --cmd "cargo test --locked" --query "FAILED\|panicked"` |
| **`pctx log`** | Re-running slow builds to grep a different error | `pctx log @latest "deadlock\|timeout"` (queries saved `@sN` spool in ~`11 ms`) |

### Measured `pctx` payload reductions

- **`pctx json`** on `5,000` API records (`1,021,281 B`): emits a **`662 B`** structured schema + filtered count digest in **`7.82 ms`** (**`-99.94%`** payload bytes).
- **`pctx batch` + `pctx log @latest`** on a `2,400`-line cluster log (`226,918 B`): spools output once as `@s1`, preserves code fences and stack traces intact, and answers follow-up **Okapi BM25 + Trigram Reciprocal Rank Fusion (RRF)** queries in **`11.00 ms`** (`2,273 B`, **`-99.00%`**) with **zero command re-execution**.

---

## 2. Compaction Continuity Journal (`cctx journal`)

Long coding sessions inevitably hit context-window compaction, causing agents to forget subtle invariants, rejected approaches, or verified root paths. `cctx journal` maintains a project-scoped continuity log that resumes into strictly **`<= 2,048 B` (`2 KiB`)**:

```sh
# Record a verified architectural decision or milestone
cctx --root /path/to/project journal record \
  --summary "Switched planner literal extraction to ASCII case-folded trigrams"

# Emit <= 2 KiB <cctx_journal_state> block + top BM25/RRF recall after compaction
cctx --root /path/to/project journal resume

# Search historical session decisions
cctx --root /path/to/project journal search "cgroup memory reserve"

# Print fail-open hook configuration for agent harnesses
cctx --root /path/to/project journal hook-config
```

- **Measured compression:** Compresses a `40`-step (`434,271 B`) engineering session into **`1,829 B`** (**`-99.58%`**) in **`5.51 ms`**.
- **Fail-open safety:** Hook integration never blocks tool execution (`0` `PreToolUse` denials).

---

## 3. BPE-Aligned Formula Prompts (`cctx prompt`)

Verbose prose instructions in `AGENTS.md`, `CLAUDE.md`, MCP descriptions, and subagent definitions get re-tokenized on every single turn. `cctx prompt` compiles agent instructions and user task prompts into compact, BPE-aligned symbolic specs while preserving domain nouns and safety invariants:

```sh
# Render BPE-aligned formula instructions (-63.1% tokens: 2,048 -> 756 tok)
cctx --root /path/to/project prompt --style formula --target instructions

# Compress an English or Korean task prompt while preserving technical identifiers
cctx prompt --compress "Analyze planner.rs and fix case-insensitive regex fallback" --json
```

Supported `--target` outputs include `instructions`, `mcp`, `subagent-yaml`, `subagent-md`, and `claude-md`.

Continue to [Benchmarks & Token Suites](../benchmarks/) for the full experimental methodology and confidence intervals.
