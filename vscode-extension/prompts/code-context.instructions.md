---
description: "Use when: finding files by name or path, searching code, or reading saved source, documentation, and configuration in a registered workspace"
applyTo: "**"
---

# Code Context source lookup

CCTX is an optional accelerator, not a prerequisite for Copilot to continue.
It is a root-level memory cache: startup and registry/status inspection do not
prewarm source. A first cold lookup returns `CACHE_MISS` immediately and schedules
only the requested root for bounded background loading. Loading or syncing roots
also return a miss instead of making ordinary lookups wait. Treat this as a
cache miss, never as an empty search, missing disk file, or proof of absence.
For that lookup continue with a permitted host search/read under the same policy.
Do not poll/repeat the request, try MCP/CLI against the same cold index, change
roots, or run `code_sync` just to wait. A later independent lookup may hit the
cache. `CACHE_LOAD_FAILED` exposes the actual load failure and retry cooldown.
Only an explicitly needed `code_sync` is a waiting full-source barrier.
Inactive roots, including legacy prewarm registrations, may be evicted after
15 minutes or earlier under LRU/memory pressure; a later miss lazily loads them.
Do not enumerate or prepare sibling worktrees speculatively.

Use `#cctxNativeFiles` (`code-context-native_codeFiles`) first for filename/path discovery with a glob; a content
pattern does not search filenames. Use `#cctxNativeContext` (`code-context-native_codeContext`) for a symbol or literal plus surrounding saved
source. Use `#cctxNativeGrep` (`code-context-native_codeGrep`) for exact or regex search and
`#cctxNativeRead` (`code-context-native_codeRead`) for known ranges. Prefer one context call over separate search
and read calls. Do not start with built-in `read_file`, `view`, `grep_search`, or `rg` for registered saved source, even when file paths are already known. If Code Context tools are deferred or not yet visible in the active tool list, discover them first with `tool_search` (for example query `"code-context-native codeContext codeRead codeGrep codeFiles"`) before reading or searching files.

Context radius is 0..100 and limit is 1..64. Broad shared reads are split into
explicit snippets of at most500lines (max64segments total); split_ranges=false
retains strict input ranges. For large reads use page=true and continue with
next_ranges until complete=true; compare whole-file hashes across pages and
restart if they differ. Do not treat a page as complete or bypass256KiB limits.
For oversized context reduce limit/radius or select one file.
Content search is literal unless `regex: true`.
An empty result is not proof that a file does not exist: check the registered
globs/scopes. Hidden paths (including .github), ignored files, and secret/key
files remain excluded; use the host's policy-aware tools for permitted excluded
targets, without retrying Code Context or widening access automatically.
Do not send hidden paths (including .github/.vscode) or known excluded datasets
to CCTX. For unknown paths/lines, locate them with Files/Context before batched
reads. `READ_RANGE_OUT_OF_BOUNDS` reports the actual indexed line count: choose
a valid range instead of repeating the failed request. A missing member fails
the whole batch; correct only the reported path, not the workspace root.

Search/file results include a snapshot freshness notice. `STALE_INDEX` means a
checked disk file differs from the memory index: do not rely on old matches or
line numbers. Use `code_read`/`#cctxNativeRead` for those known saved ranges or a
host policy-aware live disk read. `TARGET_NOT_INDEXED` is not proof of absence
and can indicate a new or policy-excluded file. Do not change roots or bypass
exclusions to silence it. For recent edits and exhaustive no-match conclusions,
use an explicit synchronization barrier; candidate checks alone cannot verify
the whole corpus or editor buffers.
For known changed files prefer Read/Context or explicit file paths rather than
a full-root code_sync. Full sync is for exhaustive corpus absence/freshness
barriers, not routine status checking or every edit.

Use MCP `code_files`, `code_context`, `code_grep`, or `code_read` when the native tools are
not callable in the current session, even if their names appear in a tool list.
In an unattended/background session known to depend on foreground VS Code
external-tool dispatch, prefer backend MCP instead of enqueueing native calls
that cannot start. A native timeout or repeated host-dispatch delay is not proof
that the shared index is slow or stale: do not run code_sync to repair it.
Do not duplicate a still-pending lookup or repeatedly poll native Status; use
host cancellation for the pending call when available, then use backend MCP
for subsequent lookups.

If both client surfaces are unavailable or blocked on host dispatch but a backend
terminal is usable, use the installed `cctx --backend shared --root /actual/root`
CLI before raw source search/read. This is the same Code Context daemon, not a
standalone index, and cannot repair `CACHE_MISS`. For example, append `files '*.ts' --limit 20` or
`read src/example.ts 1 80` with actual discovered paths. Keep the same authorized
root, scopes, exclusions and host policy. Never use another transport to bypass
a content exclusion, trust decision or explicit access denial.

Fall back to built-in search/read only
when neither Code Context surface is available, the workspace is unregistered,
the target is unsaved or excluded, or Code Context returns an error.

MCP-only worktree calls should pass the actual absolute worktree `root`; never
substitute its main repository. Shared preparation inherits scope/exclusion
policy only after a local operator has authorized that registered parent with
`cctx --root /parent authorize-worktrees`. This is not permission for an agent
to authorize unrelated paths. Do not retry a denied root or persistently
excluded target. Report the coverage limitation and use a permitted host tool.
For an already operator-authorized Git parent, absence of a worktree from
`code_workspaces` alone is not a reason to skip CCTX: its first lookup performs
bounded shared preparation. Use the actual root of the files being accessed,
which can differ from the session's initial workspace.

Installed versions are not session readiness. At a safe boundary use
`cctx --root /project doctor` to detect stale MCP processes; refresh the host's
MCP tool list or native extension host without killing active sessions.
