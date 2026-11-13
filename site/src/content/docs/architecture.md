---
title: "Architecture, Adaptive Memory & Privacy"
description: "Inside the Code Context (cctx) Rust engine: case-folded trigram posting index, HIR regex planner, incremental file watcher reconciliation, cgroup v2 adaptive memory, and local security boundaries."
category: architecture
verified: 2026-11-14
sidebar:
  order: 6
---

Code Context is engineered as a **single-binary Rust daemon** (`cctx`) that keeps hot repository metadata and trigram postings in bounded local RAM while respecting operating-system memory pressure and workspace access rules.

## System architecture

```text
  VS Code 1.140+ Extension              MCP Clients (Claude, Cursor, CLI)
 (#cctxNativeContext, Read, ...)        (cctx shared-mcp -> code_context, ...)
               \                                       /
                \--- Persistent CCTXJ1 Framing RPC ---/
                (Unix Domain Socket or Authenticated Win32 Loopback)
                                   |
                   +---------------v---------------+
                   |     Shared Rust Daemon        |
                   |  - Workspace Catalog & Policy |
                   |  - Case-Folded Trigram Index  |
                   |  - HIR Regex Query Planner    |
                   |  - Polyglot AST Block Clamper |
                   |  - Incremental File Watcher   |
                   |  - Adaptive Memory Controller |
                   +-------------------------------+
```

---

## 1. Case-folded trigram posting index & HIR planner

- **Resident posting index (`src/resident.rs`):** Indexes eligible text source files into compact 3-byte ASCII/UTF-8 case-folded trigrams. Both exact and `ignore_case: true` queries intersect sorted posting lists in microseconds before touching file bytes.
- **HIR regex planner (`src/planner.rs`, `src/query.rs`):** Parses regular expressions with `regex-syntax` High-Level Intermediate Representation (HIR) to extract mandatory literals, prefixes, suffixes, and alternations (`a|b`). Only candidate files surviving trigram intersection are scanned with `grep-regex`.
- **Zero-allocation wire serialization:** Hot responses use borrowed snippet slices, `RawJson` framing, and persistent `CCTXJ1` socket connections to hold warm RPC latency at ~`0.232 ms`.

---

## 2. Incremental file watcher reconciliation

Saved edits never require manual re-indexing:

- `notify`-backed filesystem watchers (`src/watched.rs`, `src/incremental.rs`) detect file creates, edits, renames, and deletions in real time and patch only affected posting entries.
- If an agent calls `code_read` microseconds after editing a file—before the OS watcher event lands—`code_read` verifies file metadata/hashes and recovers transparently from live disk so stale lines are never returned.

---

## 3. Adaptive memory controller & eviction

A resident cache on a developer workstation or WSL2 VM must never starve compilers, browsers, or Docker containers:

- **Linux & WSL2 cgroup v2 awareness (`src/memory.rs`):** Continuously inspects host RAM reserve and parent cgroup v2 limits, shrinking index budgets automatically under memory pressure.
- **Windows physical-memory reporting:** Native clients report fresh host RAM availability; on WSL without a fresh host report, the daemon conservatively caps itself at a `4 GiB` ceiling unless configured otherwise.
- **TTL & LRU eviction:** Idle roots evict automatically after `15` minutes (`TTL`) or under Least-Recently-Used (`LRU`) pressure when capacity is reached (up to `256` registered workspace roots).

---

## 4. Security, exclusions & 100% local privacy

- **Zero network egress for indexing:** Source code, trigram indexes, continuity journals, and usage telemetry stay strictly on your local filesystem (`~/.local/state/code-context` on Linux/WSL or `%LOCALAPPDATA%\code-context` on Windows).
- **Strict exclusion boundaries (`src/exclusion_policy.rs`, `src/policy.rs`):** Automatically respects `.gitignore`, build outputs (`target/`, `node_modules/`, `.venv*/`, `dist/`), secret files (`.env`, `.env.*`), and sensitive agent/editor configuration directories (`.github`, `.vscode`).
- **No policy bypass across transports:** Neither Native tools, MCP, nor shared CLI will serve files excluded by workspace trust or repository ignore policies.
- **Authenticated Windows loopback:** On native Windows, local TCP loopback RPC requires a per-user secret token stored with restricted user permissions under `%LOCALAPPDATA%\code-context`.

Have more questions? Check the [FAQ](../faq/).
