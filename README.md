# Code Context

> **Reviewer**: Copilot SDK assistant (GitHub Copilot)

Shared, local Rust source-search acceleration for coding agents. A single
per-user daemon holds RAM source/indexes; short CLI clients and stdio MCP proxies
reuse it across projects and sessions. No model, network search, embeddings,
database cluster or editor extension is required.

## Implemented scope

- Case-sensitive literals: intersect file-level byte-trigram postings, then
  verify the original bytes. One/two-byte patterns scan RAM.
- Regex and Unicode case-insensitive search: bounded, line-aware ripgrep Rust
  matchers plus a sound HIR query planner. Mandatory literals reuse the trigram
  index; the original matcher always verifies candidates. Text anchors retain
  rg-compatible line-buffer behavior.
- A bounded 8-entry compiled matcher/plan cache. Large scans without a sound
  literal condition use up to eight deterministic workers; a targeted line
  prefilter handles leading unbounded character-class repetition.
  No PCRE lookaround/backreferences or multiline queries.
- Optional relative file/directory prefixes and filename/path glob filtering.
- Search, multiple range reads and combined search/surrounding-context retrieval.
- Transparent `code_read` recovery: a snapshot miss or live hash race triggers
  bounded strict reconciliation and one short atomic-save retry. Callers do not
  need to issue `code_sync` or retry manually. Persistently absent, ignored,
  hidden, unsupported, or out-of-scope files remain rejected.
- Watcher-driven, staged incremental updates of changed files and posting lists.
  Create/delete/rename/ignore changes reconcile permitted file membership.
- One daemon, explicit multi-root registry, bounded IPC/notification queues,
  duplicate-process exclusion, idle/LRU root eviction, and systemd lifecycle.
- Seven MCP tools: `code_files`, `code_workspaces`, `code_grep`, `code_read`, `code_context`,
  `code_status`, `code_sync`.

Non-goals: semantic/vector search, answering questions itself, call graphs,
editor-buffer access, remote indexing, registering roots outside explicitly
opened trusted workspaces, or replacing the host's built-in `rg` implementation.

## Install and register

Linux, Cargo/Rust, installed `rg`, and a systemd user manager are required for
the supplied install script. The daemon itself only requires Linux/Unix sockets.

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
bash scripts/install.sh

cctx --root /path/to/project --scope src --scope tests --glob '*.rs' plan
cctx --root /path/to/project --scope src --scope tests --glob '*.rs' register
cctx --root /path/to/another --scope api --glob '*.py' register
cctx workspaces
cctx ping
```

Registration requires explicit directory scopes and positive source globs.
Use repeatable `--exclude 'vendor/**'` options for generated/vendor trees.
`plan` enumerates the proposed source set and reports file, path and source-byte
totals, effective excludes and watch-directory count without changing the
registry. Duplicate and nested scopes are collapsed.
By default, 54 high-confidence dependency/build/cache directory names and eight
glob families are excluded even when a repository has no ignore file. Examples:
`node_modules`, `jspm_packages`, `target`, `CMakeFiles`, `_build`, `zig-out`,
`nimcache`, `TestResults`, `DerivedDataCache`, Python environments/caches,
`Pods` and `DerivedData`.
Marker files activate 27 scoped ecosystem profiles: Node, Python, CMake, .NET,
Swift, BEAM, Haskell, Ruby/Rails, PHP/Composer, Go, Perl, Unity, Unreal, Dart,
Julia, R, Android, ROS, OCaml, Clojure, Rust, JVM, Scala, Godot, Terraform, Zig
and Nim. Conditional names such as `vendor`, `deps`, `bin`, `obj`, `Library`,
`Intermediate`, `tmp` and `logs` are excluded only below the root/scope where
the matching marker was found, so a profile cannot hide an unrelated monorepo
subtree. `plan` and `code_status` report detected profiles.
Use `--no-default-excludes` only when generated/dependency source is
intentionally required; the choice is persisted in the workspace registry.
Never register roots or files excluded by your organization's content-access
policy: this standalone program cannot discover a host's private exclusion rules.
The shared backend is an optional, nonblocking **root cache**. Registry entries
do not cause startup loading. The first ordinary Files/Grep/Context/Read lookup
on a cold root returns `CACHE_MISS` immediately and queues only that root for
background loading. Continue that lookup with permitted host search/read;
do not poll, repeat it through another CCTX transport, or force `code_sync`.
A later independent lookup can hit the cache. A miss is not zero matches or
proof of absence. `code_sync` is still an explicit, waiting full-source barrier
and may deliberately clear a prior load-error cooldown to attempt repair.
Clients never do that automatically merely to wait for warmup.
The legacy `--prewarm`/registry preference is retained for compatibility but
does not load roots automatically, bypass LRU, or pin them against idle expiry.
`--preserve-registration-settings` updates only source scopes/globs for an exact
existing root while retaining its excludes, prewarm choice, and memory policy.

Install locations:

- `~/.local/bin/cctx`
- `~/.config/systemd/user/code-context.service`
- `~/.local/state/code-context/` (private registry, lock and socket)

`CCTX_HOME` overrides the state directory for isolated tests. It must be absolute
and mode 0700, owned by the current user. Installed service uses the default path.
The install script does not change the MCP configuration; register it separately:

```sh
copilot mcp add code-context --timeout 30000 -- "$HOME/.local/bin/cctx" shared-mcp
```

This creates a distinct user entry, not a replacement for existing servers.
Reload the host's MCP tools/start a new session if needed. Registration does not
prove that a model will automatically select these tools.

### Native VS Code tools

MCP remains supported. For VS Code 1.140+ on a Linux workspace extension host
(including WSL/SSH), install the native Language Model Tool adapter:

```sh
bash scripts/install-vscode-extension.sh

# Equivalent after the VSIX has been built:
code --install-extension \
  /home/moonchoi/dev/code-context/vscode-extension/dist/code-context-native.vsix \
  --force
```

The installer enables the local `contribLanguageModelToolSets` proposal in
the desktop user's persistent `~/.vscode/argv.json`. Fully quit all VS Code processes and
reopen VS Code after installation; **Developer: Reload Window is not enough**
for runtime arguments. The extension contributes
seven native tools:

- `#cctxNativeFiles`
- `#cctxNativeGrep`
- `#cctxNativeContext`
- `#cctxNativeRead`
- `#cctxNativeStatus`
- `#cctxNativeSync`
- `#cctxNativeWorkspaces`

The `Native` suffix intentionally avoids collisions with MCP-generated
references such as `#cctxContext`. MCP remains available under its existing
tool names.

The seven native tools are grouped into the `code-context-native` Language Model
Tool Set. VS Code Agent Host forwards extension tools through enabled tool sets;
ensure **Code Context Native** is enabled in the Agent tools picker.
The manifest uses the real trust context key `isWorkspaceTrusted`; an earlier
`workspaceTrust` condition silently filtered every tool from Agent Host while
leaving them visible in `vscode.lm.tools`.

The extension runs in the workspace host, so WSL paths and the private Unix
socket stay on Linux. It resolves the active editor/workspace against registered
roots, keeps one persistent length-prefixed JSON connection, reconnects once,
honors cancellation/timeouts, and can start `code-context.service`. Tools are
unavailable until the workspace is trusted. The startup probe checks that all
seven names appear in `vscode.lm.tools`, invokes `codeWorkspaces` through
`vscode.lm.invokeTool`, and resolves the current root without loading a
source index. Use **Code Context: Run Native Tool Self-Test**
to call `codeStatus` through `vscode.lm.invokeTool`.

Trusted unregistered workspaces are prepared asynchronously after the first
source lookup returns a cache miss, not during window startup/reload:
use two bounded root/nested source-existence probes, include every supported
source glob (plus common uppercase aliases), run bounded `cctx plan`, then
register only if every admission/exclusion check succeeds. No click or input is
needed. Automatic registrations use scope `.` so a later nested project,
sibling source directory, or newly introduced supported language is not
permanently outside the watcher; ignore rules, ecosystem excludes, and
admission limits keep that root scope bounded. When preparation is needed, an existing
automatic plan is widened when needed through
`--preserve-registration-settings`, so custom excludes, prewarm, and memory
policy are retained.
Registered folders do not need source-existence searches to infer their plan.
Their scope/glob plans are computed from registry metadata instead.
Hidden source paths (for example `.fdai/` or `src/.generated/`) are never
indexed because the Rust admission contract intentionally rejects hidden
relative paths.

Automatic coverage includes Markdown/text documentation and JSON/JSONC,
YAML, TOML, INI, XML, and common web component/style files, not just code.
Hidden files remain excluded, including `.github`, `.env`, and private tool
state. Default exclusions also reject `secrets.*`, `credentials.*`, and private
key/certificate containers. These filename rules are not a secret scanner;
keep sensitive data ignored/excluded and honor host content-access policies.
Existing manually managed registrations remain unchanged.

Set `codeContextNative.autoRegisterWorkspace` to `false` to restore manual
registration. The status bar then shows `+ CCTX Register`; clicking it runs the
same inferred plan and asks for confirmation, with optional **Customize** inputs.
The same flow is available from **Code Context: Register Current Workspace**.
Disable automatic registration before maintaining intentionally narrow custom
scopes.

Automatic drift repair is provenance-safe: the extension keeps a validated,
64-entry bounded fingerprint of plans it created. It updates a root only while
the live registration still equals that fingerprint. A CLI or Customize change
releases the root from automatic management rather than being overwritten.

Extension client tools are available to direct VS Code chats only when the
**Code Context Native** tool set is enabled. Delegated/headless sessions and
Copilot CLI can omit client tools by design; the MCP surface remains the
cross-session fallback and uses the same daemon/index.

Missing native roots can be initialized on demand only if Git lists them as
worktrees of a currently open trusted repository. The same bounded plan and
registration checks apply; unrelated paths are rejected. Newly added workspace
folders are checked without restarting. Disable `autoRegisterWorkspace` to
require explicit registration. Since 0.6, native, MCP, and CLI share the same
worktree preparation path. A local operator must explicitly authorize a
registered Git parent; a model-provided path alone never grants access.

`CCTX Available` and the self-test prove extension-side registration/callability,
not model selection or chat picker enablement. Enable **Code Context Native**
in the Agent tools picker. If extension tools are not forwarded to a delegated
session, discover and use the MCP equivalents.

Since 0.7.1, extension activation is scheduled at `onStartupFinished` and
readiness probes are coalesced during event bursts. Reload checks only native
tool registration, the daemon workspace registry, and registered-root resolution:
no `findFiles`, automatic plan/register, worktree preparation or source lookup.
An unregistered workspace stays visibly deferred until its first source lookup
or an explicit registration command.

`code_status` is observational: it never cold-loads an index, prepares a new
worktree, drains watcher events, hashes source, or refreshes its idle lease.
Unloaded roots report `index_state: "unloaded"` and `stats: null`; loaded roots
report the last in-memory status, not an implicit freshness barrier. Native
status/self-test commands are therefore safe before a large root's first search.
Use a source lookup to load source intentionally or `code_sync` for strict
verification, not status polling.

For other MCP hosts, use a local stdio server whose command is the absolute
`cctx` executable and whose arguments are `["shared-mcp"]`. No secrets are needed.
The daemon must already be running.

#### VS Code on Windows with a WSL remote

For native tools in the separate **Windows Agent Sessions window**, install the
native extension locally as well as in WSL and configure its WSL native-IPC
bridge. See [Windows native setup](vscode-extension/README.md#windows-agent-sessions-with-wsl).
A WSL workspace-host installation alone cannot register tools in that Windows
window. This is separate from the MCP setup below.

Use VS Code's **remote user MCP configuration** for the WSL distribution, not
the Windows user configuration. Set both the executable and `cwd` to existing,
absolute Linux paths:

```json
{
  "servers": {
    "code-context": {
      "type": "stdio",
      "command": "/home/you/.local/bin/cctx",
      "args": ["shared-mcp"],
      "cwd": "/home/you"
    }
  }
}
```

Replace `/home/you` with the WSL home directory. The standard remote user file
is `~/.vscode-server/data/User/mcp.json`; use VS Code's remote configuration
command if the server data directory is customized.

Some VS Code builds import `~/.copilot/mcp-config.json` with a remote home
directory converted to Windows separators. That invalid working directory can
produce `spawn /home/you/.local/bin/cctx ENOENT` even when the executable exists.
If this occurs, use the explicit remote configuration above and disable only
Copilot CLI discovery in **remote settings**:

```json
{
  "chat.mcp.discovery.enabled": {
    "copilot": false
  }
}
```

This disables discovery of all CLI-configured servers in that remote. Migrate
any other required CLI server definitions to remote MCP configuration first.
It does not remove the CLI configuration or disable other discovery sources.
Keep the normal MCP trust/start approval; the prompt itself is not a daemon
failure. Restart only the affected MCP connection after changing its definition.

### Diagnose and migrate existing sessions

```sh
cctx --root /project doctor
cctx --root /project migrate
# Explicitly approve a scope expansion after reviewing the plan:
cctx --root /project migrate --add-scope . --apply
# Exclude a specific generated/oversized artifact without raising budgets:
cctx --root /project migrate --add-exclude 'tools/visualizer/src/generated/**'
# Authorize only Git-listed worktrees of this registered parent:
cctx --root /project authorize-worktrees
cctx --root /linked/worktree prepare
cctx --root /project authorize-worktrees --revoke
cctx prune-worktrees
cctx prune-worktrees --apply
```

`doctor` reports client/daemon executable fingerprints, capability schema,
registered coverage, and stale running MCP binaries. It is read-only: it does
not terminate processes, restart a host, or claim an exact session-to-process
mapping. Updating the executable or restarting the daemon does not update
already-running MCP clients or a chat's cached tool catalog. Finish in-flight
requests, reconnect the host's code-context MCP server, and refresh the affected
chat tools at a safe boundary. Native self-tests reject outdated daemon
capabilities, while chat picker enablement remains explicitly unverified.

`migrate` previews before/after inventories and only applies with `--apply`.
It adds supported text/config globs but preserves scopes unless `--add-scope`
is explicitly supplied. Existing exclusions, prewarm, default-exclude settings,
and memory limits remain intact; `--add-exclude` only adds reviewed exclusions.
Oversized artifacts fail explicitly instead of silently raising budgets.
Application additionally validates UTF-8, binary-content, live byte and line
limits before committing the migration. Format errors name the offending paths
(up to sixteen) and leave the registration unchanged.

Read diagnostics identify the actual failing target rather than returning
partial batches as success. `READ_RANGE_OUT_OF_BOUNDS` includes the root/path,
requested bounds and indexed line count; a snapshot EOF receives one local
refresh first so a just-grown saved file is not falsely rejected.
`READ_UNAVAILABLE` distinguishes missing disk paths from present but non-admitted
paths and unstable versions. Hidden/excluded targets require permitted host
tools, not repeated CCTX retries or automatic scope expansion.

Worktrees inherit their parent's scopes, globs, exclusions and memory policy,
never prewarm or permission to authorize more roots. Parent changes invalidate
cached worktree snapshots; revocation blocks subsequent worktree queries.
Preparation is serialized and admission-checked across CLI/MCP/native callers,
including persistent MCP sessions created before authorization. Always use the
actual worktree root, not the main repository. Registration metadata is limited
to256roots and1MiB, independently of the eight-root source residency limit:
at capacity, admission reclaims only nonexistent managed registrations before
retrying the limit check. Existing worktrees and manual registrations, including
missing manual roots, are never evicted. This bounded metadata check runs only
at admission capacity, not on each lookup or startup, and never enumerates Git
worktrees. Explicit pruning remains available for nonexistent managed roots.
Git verification targets only the actual requested worktree and its authorized
repository's common Git directory/backlink, not a listing of sibling worktrees.
Only explicit coverage diagnostics enumerate candidates. Lazy preparation/load
failures are logged, exposed as `CACHE_LOAD_FAILED` and cooled down for60seconds;
another demand after the cooldown can retry. Admission retries are bounded.
If authorization changes while a failed first-use preparation is in flight,
the stale rejection is retried within the same bounded queue instead of
poisoning the newly authorized root's cooldown. Unrelated managed registrations
do not reset that policy decision.

Search patterns are literal substrings by default, not semantic queries or
lists of keywords. Set `regex: true` explicitly for `a|b` alternation; without
it, the pipe is searched literally. Use `code_files` for filenames and paths.
A zero-match result is distinct from `CACHE_MISS` and does not prove disk absence.
MCP and native argument descriptions expose this distinction.

## Use

```sh
cd /path/to/registered/project
cctx files '*Tools.ts'
cctx files '*.md' --path docs
cctx grep 'get_user' --limit 20
cctx grep '^async def ' --regex --filter '*.py'
cctx grep 'user' --ignore-case --path src/auth
cctx context 'get_user' --radius 5 --limit 8
cctx read src/auth.py 10 50
cctx sync
cctx status
```

CLI resolves the closest registered ancestor of `--root` (default current
directory). It searches that registered workspace, not just the current subfolder;
use `--path` to narrow. MCP source calls accept an explicit registered
absolute `root` or resolve the current registered ancestor when omitted; use
`code_workspaces` rather than guessing another root.

`code_files` accepts `{"glob":"*Tools.ts","paths":["src"],"limit":200}` and
returns `{"paths":[...],"total_matches":N,"truncated":false}`. It searches
filenames/paths, not contents, and only returns admitted indexed files.
`code_grep` and `code_context` search contents; `glob`/`paths` only filter those
contents. An empty result does not prove a file is absent outside the registered
coverage. Check scopes/globs and use policy-aware host tools for excluded targets.

`code_read` accepts `{"root": "...", "ranges": [{"path": "src/a.rs", "start": 1,
"end": 30}, ...]}` and verifies the selected files' live SHA-256 before returning.
Ranges are 1-based inclusive. `code_context` merges overlapping ranges and
returns source hashes. Hashes describe bytes read, not a guarantee that a
subsequent edit still addresses those bytes; revalidate before writing.
Context radius is bounded to0..100 with1..64matching lines. Shared batch reads
default to `split_ranges: true`: broad ranges become explicitly labeled snippets
of at most500lines, at most64segments per response, clamped to snapshot EOF.
`split_ranges: false` retains strict500line input ranges. Response/memory caps
are unchanged. Non-shared experimental backends retain strict range semantics.

For large shared reads use `code_read` with `page: true`, or CLI
`cctx read path start end --page`. It returns an explicit object with `snippets`,
`complete` and `next_ranges` under256KiB. Continue with `page: true` and the
returned `next_ranges`; do not treat an incomplete page as a complete batch.
Compare whole-file hashes across pages and restart if they change. A single
line that cannot fit still fails explicitly; no partial lines are returned.
Default `page: false` preserves the atomic snippet-array result shape.

### Search freshness and recent disk edits

Shared `code_grep`, `code_context`, and `code_files` results include a
`freshness` object, also visible as a `NOTICE` in native text results. It records
the snapshot generation, returned-file check (`metadata` for grep/files,
`sha256` for context), and `full_disk_verified: false`. This is deliberately not
a claim of globally current or atomic results, including when no match is found.

Returned grep/file paths and explicit known file targets are checked against
live metadata, without scanning the corpus. Context snippets are live-hash
verified even on context-cache hits. A checked change/removal produces an
explicit `STALE_INDEX` error containing the root/path and live-disk guidance,
not a successful stale result. Use `code_read` to refresh known saved ranges or
the host's policy-aware live file read, then rerun the search. Policy exclusions
are never bypassed. A newly present explicit path with no indexed entry returns
`TARGET_NOT_INDEXED`, rather than silently treating it as an empty file.

Unscoped searches can still miss new/changed files that are not returned as
candidates. Metadata checks also cannot detect changes preserving the checked
metadata. For recent edits, verify known saved files before acting; use
`code_sync` before exhaustive absence claims. No operation freezes concurrent
filesystem changes after verification, and unsaved editor buffers remain outside
the contract. These checks do not trigger a whole-project reload.

### Measuring real use

```sh
python scripts/audit_agent_tool_adoption.py --hours 1 --output /tmp/cctx-adoption.json
cctx coverage > /tmp/cctx-coverage.json
python scripts/audit_agent_tool_adoption.py --hours 1 \
  --coverage /tmp/cctx-coverage.json --session SESSION_ID --sample-limit 20 \
  --output /tmp/cctx-eligible-adoption.json
# Optional compatibility input from host protocol traces:
python scripts/audit_agent_tool_adoption.py --source wire --hours 1 --output /tmp/cctx-wire.json
```

Schema 5 defaults to actual Copilot session records and separates tool selection,
successful calls, recognized nonempty results, empty results, and unmeasured
successes. Tool discovery is not a source lookup. Catalog name mentions are not
verified model exposure; unavailable/truncated payloads are never counted as
hits. A nonempty result measures retrieval, not relevance or task correctness.
Per-session reports classify structured lookups as eligible, legitimate
fallback, or unknown using a permitted filename inventory no older than ten
minutes. Hidden, unregistered and non-admitted targets remain legitimate host
fallbacks. Built-in searches with an unproven corpus remain unknown rather than
inflating eligible-use percentages; arbitrary shell text is never interpreted.
The first twenty eligible calls per session are reported separately. Replaying
old events against current coverage is labeled as such: it does not prove that
the same registration or host access policy existed at the time. A post-update
sample with twenty actual eligible requests is required before claiming 90%
selection; empty/incomplete/unknown samples do not pass that criterion.
Git-listed worktrees of authorized parents that are not yet prepared are
reported separately as unknown/requiring preparation, not counted as either
successful eligible calls or inevitable fallbacks. Candidate diagnostics are
bounded to 128 roots and never mass-register a repository's worktrees.

Output is JSON, diagnostics stderr. Invalid/failed CLI requests exit 2.
MCP failures use `isError: true`; a missing search returns an explicit count of
zero. Truncation is explicit, totals count matching lines rather than occurrences.
Results sort by relative path then line number.

## Freshness contract

Saved files only; no API here can promise an atomic view of a changing filesystem.

1. A bounded inventory determines permitted source and its parent directories.
   Only those directories plus configured scope ancestors are watched
   non-recursively; dependency/ignored subtrees do not consume recursive
   inotify watches. Membership discovered after inventory and relevant
   invalidations captured during loading are reconciled before readiness.
2. Existing-file writes use the changed-file path. Create/delete/atomic-save
   events reconcile only their parent directory; directory create/delete/rename
   reconciles its affected subtree. Scans retain the original root, registered
   scopes and ripgrep ignore/exclusion rules. Unrelated source is not reopened.
   An unpaired rename of indexed content can move into an unwatched parent, so
   it conservatively reconciles the full metadata inventory to find its destination.
   Ignore/profile changes and watcher overflow also reconcile the full inventory.
3. Every sixty seconds, a bounded background metadata/membership safety scan
   recovers missed events without reading unchanged source contents. Watcher
   events are still processed before queries; the slower interval changes only
   the missed-event fallback (up to sixty seconds plus scan scheduling), not
   ordinary saved-file updates.
4. `code_sync` explicitly hashes all permitted files and applies only changed
   postings. Use it after edits when exact freshness matters and before an
   exhaustive "not present" claim. It is more expensive than a normal query.
5. All changed bytes, hashes and budget checks are staged before mutation.
   Synchronization errors block source responses until successful repair/resync;
   an old generation is not returned as a successful current result.
6. Read recovery re-enumerates only requested parent directories and refreshes
   requested/changed files. Hidden, out-of-scope, unsupported and explicitly
   excluded paths fail before recovery; ignored existing paths fail after local
   membership checks. Neither case triggers a corpus-wide hash pass. Atomic-save
   gaps retain bounded retries. Strict sync, overflow/rescan and blocked-state
   repair remain full verification barriers.

Policy revocation is not instant/linearizable: stop/unregister the root when an
external access policy changes. The local daemon is not an authorization sandbox
against another malicious process running as your own user.

## Resource and format bounds

Version0.9.1 caches one compiled read-path policy per resident root, keyed by
the current scopes/globs/effective exclusions. It caches matchers, not per-file
permission decisions: current profiles, ignore reconciliation, safe paths and
live file hashes still apply. Policy changes replace the cache; memory-pressure
cleanup releases it. `io_profile` reports compilation/cache-hit counters.
Unchanged background metadata results skip index/accounting reconstruction;
the60second metadata safety interval and explicit full-source sync are unchanged.
The daemon sleeps up to1second when idle and polls at25ms only while root work
or explicit requests are pending; incoming requests wake it immediately.
Lost/replaced runtime directories, sockets or lock files stop the daemon instead
of leaving an unreachable background process polling indefinitely.

- Up to256metadata-registered roots; the resident source-root limit scales with
  the cache budget (one per384MiB, bounded to1..24). Evict the least recently used
  root at capacity. Source lookups extend a15minute idle lease; observation-only
  status requests do not. Legacy prewarm roots expire too. Existing/manual registrations are not automatically
  deleted to make room. Idle expiry still does not override memory limits.
  The adaptive resident-cache target also evicts other roots; a single
  large root is still subject to its per-root budgets and the service memory cap.
- Default admission policy per root: 100,000 files, 256 MiB UTF-8 source,
  1 MiB per file, 10M lines, 16 MiB total relative-path bytes, 250,000 unique
  trigrams per file, 2M unique trigrams per root, 64M file postings, 10,000
  changed files, 128 MiB staged bytes per update and 50,000 watch directories.
  Registration stores the policy, and `plan` checks file count/path/source/watch
  metadata before persistence.
- Each resident root reserves 160 MiB inside the current cache target for
  watcher metadata, bounded matcher/result/context caches and runtime overhead.
  Binary/NUL/non-UTF8/oversized source causes an explicit load/update error.
- Exclude hidden paths and symlinks; reject absolute/traversal source paths.
  Ignore rules are preserved; positive source globs do not unignore files.
- Up to 1,000 returned matching lines, 64 read ranges, 500 lines per range,
  256 KiB source payload. Narrow oversized requests; nothing is silently omitted.
- IPC: 2 MiB messages, 32 queued requests, eight incomplete input connections.
  Incomplete input times out after two seconds. Warm source operations remain
  serialized. Ordinary cold/loading/syncing source lookups return an explicit
  `CACHE_MISS` without waiting for initialization; only explicit sync/profile
  requests may wait. A bounded queue holds at most8deduplicated root demands,
  and one worker performs requested-root preparation/loading or strict sync.
  It never speculatively loads sibling roots. A full queue returns
  `background_load=false`, not implicit success. Deferred explicit work is
  limited to32requests with a28second deadline. Background work reserves cache budget;
  adaptive memory reservations still share one total cache budget.
- Socket mode 0600 inside a 0700 directory. The user service applies
  restart-rate limiting and a verified adaptive cgroup memory guard. Automatic
  memory mode uses Linux/WSL `MemAvailable`, this service's anonymous memory,
  and ancestor cgroup headroom. It leaves `max(2GiB,20% of MemTotal)` for the
  system; ancestor groups retain20% of their own cap after sibling usage.
  Of the resulting ceiling,75% is the cache target and87.5% is `memory.high`.
  `memory.max` is set and read back in the dedicated service cgroup, not in an
  inherited shared cgroup. A64MiB runtime floor keeps metadata service alive;
  tiny budgets cannot admit source roots and parent kernel limits still apply.
  Metadata-only samples run at most every5seconds. Shrink follows pressure;
  growth requires a material change and a30second cooldown. Before decreasing
  the kernel hard limit, the daemon waits for active builders and for usage to
  fall below the new high watermark; cache reservations shrink first.
  `CCTX_MEMORY_MAX_MIB=4096` restores a4GiB manual ceiling while still respecting
  available memory; `CCTX_MEMORY_RESERVE_MIB` overrides system reserve.
  Set these through a service environment drop-in and restart at a safe boundary.
  `code_status.process_memory.adaptive` reports mode, target, ceiling and errors;
  raw cgroup values show the actual kernel limits, including deferred shrink.
  No source is loaded merely because more RAM is available.
  On WSL, native Windows clients report outer-PC free/total memory in the
  existing asynchronous trace message (no new polling or RPC). A fresh report
  further limits the budget; reports expire after120seconds. Without one, WSL
  conservatively bootstraps at4GiB unless a manual ceiling is configured.
  This avoids treating a VM's potential memory as free Windows physical RAM.
  The optional cache has lower scheduling weights (`CPUWeight=20`, `IOWeight=10`)
  and physical I/O caps of32,000,000read bytes/s and1,000,000write bytes/s on the
  filesystem backing the home directory (default state and source location).
  The path-based device lookup survives
  WSL device-letter changes and works before the state directory is created;
  other source/state filesystems need their own limits.
  These are decimal MB/s, not MiB/s. `IOAccounting=yes` is deliberately enabled
  for attribution; status includes raw cgroup I/O stats/limits/pressure when
  available. These caps do not throttle unrelated processes or promise an
  end-to-end tool latency bound.
  `CCTX_IO_GUARD=1` is enabled only by the dedicated service. At startup the
  daemon verifies/applies its own `io.max` home-device limits before accepting
  requests, preserving any stricter limits and existing IOPS rules. This handles
  managers that expose configured bandwidth properties without realizing them
  after restart. It refuses to run this guard outside `code-context.service`
  or start unbounded when verification fails; ordinary standalone/test daemons
  do not modify an inherited cgroup.
  RPC/maintenance/regex/pipe workers use bounded stacks; the unit also sets
  `TasksMax=96`, `LimitNOFILE=16384`, a ten-second stop timeout and mixed kill
  mode. External inventory commands have bounded output and a 30-second timeout.
  `code_status` reports resident category estimates, process RSS, cgroup v2
  current/high/max/events values, and transparent read recovery success/failure
  counters. Estimates remain conservative rather than allocator-exact; process
  RSS/cgroup current are the observed metrics.
  `io_profile` counts full/directory inventories, full-source verification,
  source files/bytes reopened for incremental index updates since initial load,
  and read-policy rejections. These are application-level counters, not physical
  disk bytes or every live read-hash check. `daemon_admission.root_loads` counts
  complete root loads since daemon startup, allowing reload churn to be measured.
  Recent root-size hints allow eviction before a known reload would exceed
  the cache target and prevent speculative background prewarm attempts.
  `performance` includes daemon-local request IDs and bounded recent queue,
  execution, serialization and socket-write times, plus background load/sync
  durations and eviction reasons.
  A failed cold build reports its reserved and required bytes. The source
  inventory's whole-corpus byte/path/document floor is checked before reading
  source. Each admission failure reports a lower bound, not a complete build
  peak. Up to six admission retries grow the reservation within the current cache
  target: the first adds staging reserve (at least128MiB), later retries grow
  geometrically. They stop at the global ceiling, including when no other
  roots remain. Retries evict only enough LRU source roots for that reservation,
  never unconditionally purge the entire cache. Managed worktrees can reuse their
  parent's size hint before building. A root that cannot fit even alone fails
  without evicting working roots. Automatic prewarm is disabled; only requested
  roots enter the bounded background preparation/load queue.
  Queue time includes initialization wait; this is not the model/host's entire
  tool duration. Samples retain64requests
  and32background/eviction entries, not an unbounded per-call log.
  `outcome_counts` and recent timing samples distinguish `success`,
  `cache_not_ready`, `policy_denied`, `invalid_request`, and `system_error`.
  Cache misses remain explicit non-results, never empty searches or disguised
  successful reads. Legacy protocol-failure counters are retained for compatibility.
  Root failures share one bounded32-entry store. Unrelated managed registration
  changes preserve their cooldown; a retry after cooldown includes the previous
  failure cause while still returning immediately. Completed or late replies
  for expired explicit requests are discarded without cancelling useful cache
  preparation. Private runtime lock permissions are normalized to0600 after
  acquiring sole ownership and checked alongside socket/directory identity.
  Clients negotiate `request_tracing` before adding a private trace argument;
  existing JSON and binary request/response DTOs and untraced result shapes are
  unchanged. New MCP/native tool results retain their original first content
  item and add a `CCTX_TRACE` diagnostic item with the same ID as the daemon
  sample. MCP results also expose `_meta.cctx_trace`. This joins a host's
  tool-completion event to daemon queue/execution without guessing by timestamps.
  `started_unix_ms` and `completed_unix_ms` mark the client handler in the
  host's wall-clock domain; phase durations remain monotonic. Comparing these
  wall-clock fields with host events separates dispatch delay from handler
  execution without subtracting elapsed times from different clocks.
  `recent_clients` retains64 bounded4KiB client samples with IPC, preparation,
  serialization and output-write times. MCP output is encoded into one buffer
  and written as one JSON line, not streamed as thousands of escaped fragments.
  MCP's post-write sample is reported separately; the result footer necessarily
  precedes that write. Native write/host-queue time beyond the tool handler is
  not observable and is not presented as measured. Native telemetry is queued
  without delaying the source result. Diagnostic report failures are explicit.
  A new executable does not hot-patch running MCP clients: refresh them at a
  safe boundary to activate new client-side tracing and batching.
  `cache` reports ready/unloaded/preparing/loading/failed states, hits/misses,
  queued roots, the queue limit and last root load error. `CACHE_MISS` is tracked
  as a cache miss rather than a protocol failure; request timing still marks
  it as no source result. Status does not queue a root. Folder granularity is
  the registered project/worktree root, not independent subdirectory segments.
  Native callbacks also depend on VS Code host dispatch. In a known
  unattended/background session without callable native tools, use backend
  MCP; if neither client surface can dispatch, a usable backend terminal may
  invoke `cctx --backend shared --root /actual/root` against the same index.
  Never duplicate a pending lookup, use full sync to repair host dispatch, or
  switch transport to bypass content exclusions, trust or access decisions.
  These routing instructions do not automatically restart hosts or guarantee
  timeout-driven failover.
  MCP application/input errors are not reconnected/re-executed as transport
  failures. Framed protocol errors retain their negotiated framing.

## Lifecycle

```sh
systemctl --user status code-context.service
systemctl --user restart code-context.service
cctx --root /path/to/project unregister
systemctl --user disable --now code-context.service
copilot mcp remove code-context
```

Unregister revokes requests and drops the resident root on the next daemon call.
A removed root can be unregistered using its saved absolute path. A stale socket after termination is
replaced only after acquiring the process lock; regular files are never removed.
The registry survives restart; indexes reconstruct lazily and are not persisted.

## Validation and measurement

See [DESIGN.md](DESIGN.md) and [REPORT.md](REPORT.md) for architecture, measured
latencies, hardening rounds and remaining constraints.

The report also includes a natural five-pair Copilot CLI A/B on synthetic code.
With secure current-workspace inference, complete correctness stayed 5/5, tool
calls fell 64.7%, model calls fell 34.6% and mean wall time fell 18.3%. A proposed
batch-context tool was measured, made agents slower, and was removed. The final
sample remains small/noisy and is not a universal speed claim.

```sh
python3 scripts/measure_shared.py --root /path/to/agent-san \
  --queries fixtures/queries.json --rounds 30 \
  --output logs/YYYY-MM-DD/shared-original.json
```

The shared benchmark uses a private frozen copy, checks raw `rg` parity and
live/shared context/hash parity, rotates operation order after warmup, measures
actual Unix RPC/stdio MCP/CLI paths, and records twenty source-write-to-visible
update trials. It never edits the source project or flushes system caches.
The harness is currently scoped to the documented Agent-San source workload;
the Rust tool itself is project-independent.

Regex optimization uses two fixed 32-query files:
`fixtures/regex-tune.json` chooses the cost gate; the untouched
`fixtures/regex-holdout.json` evaluates generalization. `scripts/measure_regex.py`
checks every query against current `rg`, compares the planner with an identical
parallel full-RAM scan, and records compile/filter/plan/scan/finalize phases.
See [REPORT.md](REPORT.md) for the measured result and block-index decision.

Historical snapshot experiments remain available with `--backend live|ram|index`
and `bench` / `bench-resident` (explicit scopes/globs required). Those modes are
for comparison, not the shared production path. In particular, one-shot `index`
commands rebuild on every execution and do not measure shared startup amortization.

Results are warm-cache measurements, not SSD-vs-RAM hardware bandwidth tests.
Kernel physical-I/O counters were unavailable in the original environment and
are not represented as zero. No whole-agent acceleration claim follows from
engine timing alone.

The final five-block distribution reaches **46.86x by mean, 49.14x by median and
31.95x at p95** for warm exact search. Search plus versioned context measured
**76.44x mean** in the earlier full-path run. A
private-copy `POSIX_FADV_DONTNEED` cold-hint experiment measured about 1,001x
mean search speedup, with explicit advisory/virtual-disk caveats. Small known
files are different: direct open/read/hash is faster than IPC-based verified
reading. See the final section of [REPORT.md](REPORT.md).

The stdio MCP resolves its current registered root once and keeps a persistent
Unix connection to the daemon. The proxy reconnects once after a daemon restart.
Periodic fallback reconciliation runs during daemon idle windows rather than on
the first query after each interval; watcher invalidations are still drained
before every source request.

On the measured Agent-San root, prewarm holds about 218MiB. A fresh MCP process
completed initialize + tools/list in 1.74ms; its first exact-search tool call was
0.91ms and the second 0.43ms. After memory hardening, three isolated cold loads
measured 1.599s median end-to-end, versus 2.217s for the previously installed
binary on the same corpus. The source-derived watch plan reduced inotify watches
from 13,009 to 430.
Prewarming moves that cost to daemon startup instead of the first agent query.

After startup reconciliation and asynchronous metadata maintenance, a fresh
ready service measured 0.94ms for MCP initialize/list and 3.67ms for its first
search. Three further rounds removed production profiling/sorting overhead and
compacted before daemon serialization; five 600-search blocks measured a
0.344ms median block mean and 0.577ms median block p95. Context payload compaction
reduced the measured response by 25.7% without removing source snippets or hashes.

Ten further measured rounds added an exact-literal fast path, tuned posting depth,
generation-aware result/context caches, structural response bounds, thin LTO and
binary persistent daemon framing. That optimization baseline reached:

- repeated cached exact queries: **105.38x mean / 112.73x median / 75.69x p95**
- cache-miss exact queries: **62.48x mean / 63.21x median / 53.37x p95**

The repeated and cache-miss contracts are reported separately to avoid presenting
cache hits as universal latency. Eight concurrent MCP clients sustained 18,568
calls/s over 48,000 calls with zero errors and 1.371ms p99. Two model-facing
experiments—line-oriented output and a reduced tool set—were slower in paired
agent trials and were rolled back. See [REPORT.md](REPORT.md).

After ecosystem-profile hardening, the final repeated cached exact rerun
measured 0.153ms mean, 0.144ms median and 0.240ms p95, equivalent to
**105.22x / 109.09x / 76.88x** against the same warm-rg references and well
above the 30x target.

The final native adapter A/B used the same five blocks of 600 repeated searches:

| Path | Mean | Median | p95 |
|---|---:|---:|---:|
| Native VS Code framed adapter | **0.185ms** | **0.180ms** | **0.272ms** |
| MCP stdio + binary daemon IPC | 0.221ms | 0.215ms | 0.321ms |

Native was 16.2%/16.7%/15.3% faster by mean/median/p95. MCP remains installed
for Copilot CLI and other hosts.

### Agent adoption

The installer also adds concise cctx-first guidance to the Copilot user's
always-on instructions. Registered saved source should use
`cctxNativeContext`/`cctxNativeGrep`/`cctxNativeRead` before built-in `rg` and
file reads, with explicit fallback conditions.

The extension activates with `*` rather than `onStartupFinished`, so native
tools register before most Agent sessions build their initial tool catalog.
Native search/context/read results are returned as direct model-readable
`path:line` and numbered source text instead of raw JSON, avoiding the extra
temporary-file plus `jq` summarization step.

Existing long-lived Agent sessions retain their original instruction/tool
snapshot. Start a new Agent session to pick up newly installed tool metadata or
changed user/project instructions.

In the final read-only adoption probe, a fresh Copilot CLI session selected
`code_context` twice and `code_read` once without cctx being named in the user
prompt; all three calls succeeded.
