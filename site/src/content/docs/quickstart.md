---
title: "Quickstart & Agent Workflow"
description: "Run your first block-clamped symbol lookup with #cctxNativeContext, handle cold-cache responses cleanly, configure subagents, and inspect live AIC/USD savings."
category: setup
verified: 2026-11-14
sidebar:
  order: 2
---

Once [installed](../installation/), Code Context works automatically in trusted VS Code workspaces: the extension contributes seven native Language Model Tools and injects compact BPE-aligned agent instructions (`applyTo: "**"`) so Copilot prefers fast, bounded lookups without manual prompting.

## 1. Try your first prompt in VS Code Chat

Open a trusted repository and ask your coding agent:

```text
Use #cctxNativeFiles to find *planner* files, then #cctxNativeContext to inspect plan_query and summarize how trigram candidates are intersected.
```

Instead of spawning multiple `rg` subprocesses and reading arbitrary 200-line file slices, the agent executes:

1. **`#cctxNativeFiles`** (`glob: "*planner*"`) — returns matching relative paths from the resident catalog in ~`0.25 ms`.
2. **`#cctxNativeContext`** (`pattern: "plan_query"`, `radius: 12`) — intersects trigram postings, locates the symbol definition, clamps the snippet cleanly to the enclosing function boundary within `<8 KiB`, and verifies the whole-file SHA-256 hash.

---

## 2. Recommended tool dispatch priority

Code Context is an **optional nonblocking accelerator**, never a fragile gate. Agents follow a deterministic priority hierarchy:

```text
VS Code Native (#cctxNative*) > MCP (code_*) > CLI (cctx --backend shared) > Host (rg, view, glob)
```

| Lookup Intent | First-Choice Tool | Key Guardrails |
|---|---|---|
| **Symbol or literal + nearby code** | `#cctxNativeContext` / `code_context` | Prefer **1 call** (`radius: 8..50`, `limit: 1..64`) over separate `grep` + `read` turns. |
| **Filename or path discovery** | `#cctxNativeFiles` / `code_files` | Pass `glob` (`*.rs`, `src/**/*.ts`); does not scan file contents. |
| **Exact, case-folded, or regex lines** | `#cctxNativeGrep` / `code_grep` | Set `regex: true` for alternation (`a\|b`); set `max_matches_per_file` to prioritize definitions. |
| **Known line ranges or doc anchors** | `#cctxNativeRead` / `code_read` | Batch multiple edits into one `ranges` array (`<=150` lines total, `<8 KiB`); supports `doc.pptx#toc` and `#slide-3-5`. |

---

## 3. Understanding cold-cache `CACHE_MISS`

When an agent queries an unindexed workspace root for the very first time, Code Context returns `CACHE_MISS` in ~`0.2 ms` and schedules bounded background indexing:

- **`CACHE_MISS` does NOT mean zero matches or a missing file.**
- The agent performs **one** normal host lookup (`rg` or `view`) for that immediate turn—never stalling the user—and automatically resumes `#cctxNative*` / `code_*` on the next turn once warm (~`19.7 ms` median sync on a 68-file polyglot repo).
- Never poll `#cctxNativeStatus` or call `#cctxNativeSync` merely to wait for warmup; `code_sync` is reserved strictly for exhaustive absence proofs.

---

## 4. Delegating to subagents

Built-in subagents (`explore`, `general-purpose`, `code-review`, `research`, `security-review`) may run in separate sessions or model families without workspace prompt files. For maximum cache hits:

1. Always pass the explicit absolute `root` path in your subagent prompt.
2. If tools are deferred in the subagent harness, have it discover `cctxNative*` or `code-context-code_*` via `tool_search` first.
3. Optional: patch local subagent definitions into compact BPE-aligned formula specs so every subagent turn saves ~`63.1%` of instruction overhead:

```sh
cctx --root /path/to/project prompt --style formula --target instructions
```

---

## 5. Inspect live token & cost savings (`cctx usage`)

Monitor real multi-session telemetry—modeled AI Credit (`@ $0.001/AIC`) spend with versus without `cctx`, avoided tokens across turns/cache/BPE, warm cache hit rate, per-model breakdown, and actionable optimization advisories—directly from your terminal or the VS Code status bar:

```sh
cctx usage --hours 24
# Full 4-driver cost anatomy & session advisories:
cctx usage --hours 24 --details
# Machine-readable JSON output:
cctx usage --hours 24 --json
```

Example `24h` multi-session summary (`159` sessions · `7,578` turns):

```text
⚡ CCTX USAGE · Last 24 hours

💰 $48.88 saved · 26.6% less (estimated)
   Without cctx ~$183.78 → With cctx $134.90
   USD equivalent @ $0.001/AIC; not a bill.

EFFICIENCY
  Tokens avoided ~1.58B
  Warm hits 95.5% · Source adoption 90.3%

💡 OPTIMIZATION ADVISORIES
  No optimization issues detected.

MODELS · USD equivalent
┌───────────────────────┬─────────┬───────────┐
│ Model                 │    Used │ Est.saved │
├───────────────────────┼─────────┼───────────┤
│ Claude Opus (high)    │  $78.40 │    $29.10 │
│ GPT-6.1 (medium)      │  $41.25 │    $14.85 │
│ Claude Sonnet (med)   │  $15.25 │     $4.93 │
│ Total                 │ $134.90 │    $48.88 │
└───────────────────────┴─────────┴───────────┘

Full report: cctx usage --hours 24 --details
```

Running `--details` decomposes **why** net dollar spend drops—showing that **54.0%** (`851.8M tok`) comes from **avoided LLM round-trip turns** and **44.5%** (`701.8M tok`, `98.7×`) comes from **avoided prompt-cache re-reads**, rather than single-output byte trimming (`0.5%`). See [Benchmarks & Net LLM Cost Analysis](../benchmarks/#5-net-llm-cost-analysis-24h-production-trace-replay) for the full cost equation.

Continue to [Native VS Code & MCP Tools](../tools/) for full parameter schemas and rich document anchor syntax.
