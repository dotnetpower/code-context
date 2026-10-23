# Focused shared-search implementation

## Purpose and fixed scope

Speed up repeated source search, reading and surrounding-context retrieval for
coding agents, globally reusable across explicitly registered local workspaces.
Linux user-space Rust, saved UTF-8 source files, existing permission/ignore policy.
No vector search, LLM inference, call graph, remote indexing service, automatic
repository discovery, or modifications to the searched repository.

## Architecture decision

Alternatives: per-session snapshots duplicate memory/build time; a full
OpenSearch service adds unrelated operational overhead; a single local indexed
process shares the measured fast core while keeping integration thin.
Choose one Unix-socket process, explicit root registry, stdio MCP proxy, native
VS Code Language Model Tool adapter and CLI clients. Source file IDs and posting
lists are updated transactionally.

1. A private 0700 runtime/state directory contains the registry, lock and socket.
   The socket and owned process lock are mode 0600. Runtime identity and
   permissions are rechecked; the process lock excludes duplicate daemons.
2. At most256roots can be metadata-registered within the1MiB registry;
   source residency scales at one root per384MiB of cache budget, within1..24.
   A15minute source-activity timer
   expires all idle roots even without later requests. Source, posting,
   path, watch and transaction-peak budgets remain bounded. If a new root cannot
   fit in the remaining resident budget, the daemon evicts the oldest
   root before retrying; legacy prewarm metadata does not pin residency.
   It does not require a failed load before
   LRU replacement can occur.
   The memory ceiling adapts to host/cgroup headroom, preserving system reserve.
   Cache target/high watermark are75%/87.5% of the ceiling; growth is hysteretic
   and shrink drains caches before reducing kernel `memory.max`. A manual MiB
   ceiling is supported. Known root-size hints avoid
   starting reloads with a clearly insufficient remaining budget.
3. Inventory uses ripgrep's ignore decision plus universal exact/glob excludes
   and marker-driven ecosystem profiles. Ambiguous names are profile-scoped to
   the marker's root or registered scope rather than globally excluded. Only
   permitted source parents and scope ancestors receive
   non-recursive watches, avoiding ignored-tree inotify amplification. Reconcile
   pre-watch membership, drain bounded notifications before each request, and
   recover missed membership/metadata through periodic inventory.
   Ordinary membership events enumerate affected parent directories/subtrees,
   not the whole root. A sixty-second full metadata safety scan retains
   missed-event recovery. Ignore/profile changes and overflow remain full scans.
   Unpaired moves of indexed content also rescan metadata to discover destinations
   outside the installed watch set; they do not hash unchanged file contents.
4. Stage all changed bytes, hashes, postings and budget checks before applying.
   Update only modified file postings. A failed synchronization blocks responses
   until successful reconciliation; never disguise an old result as current.
5. Case-sensitive literals intersect byte-trigram postings then verify source.
   Short patterns, regex and case folding scan RAM, with bounded regex compilation.
   Optional relative path prefixes and globs only narrow registered scope.
   Regex HIR supplies only sound mandatory-literal AND/OR candidates; weak plans
   fall back through an empirically tuned cost gate. Compiled plans are bounded.
   Wide scans use deterministic path-ordered workers. A separate block index is
   rejected unless post-planner profiles show candidate-byte scanning still
   dominates a selective workload.
6. `code_sync` is an explicit full-content barrier. Watcher freshness is eventual,
   not linearizable; the filesystem can change during or after a request.
   `code_read` checks selected files' live hashes. Snapshot misses, atomic-save
   gaps, and live hash races receive bounded requested-directory reconciliation and a 25ms
   second attempt before failure; policy exclusions are never bypassed.
   Excluded reads do not cause whole-corpus hashing. Application I/O counters
   and complete-load counts distinguish local updates from real reloads.
7. `code_context` shares the same search/read assembly and byte budget.
   `code_workspaces` prevents root guessing; every source request specifies root.
8. Persistent MCP proxies use bounded binary frames internally; one-shot CLI
   clients retain newline JSON. Exact/context LRU entries are generation-keyed
   and strictly memory/entry bounded. Production requests skip profiling work;
   explicit benchmark operations retain phase instrumentation.
9. RPC opening frames, child stdout/stderr, command duration, worker count and
   stack sizes are bounded. Connection ownership is RAII-counted. Pressure
   eviction drops caches/roots and asks glibc to release free arenas.
10. The native VS Code extension is a workspace-host adapter, not a second
    indexer. It contributes seven stable tools with workspace-trust gating,
    resolves registered ancestors, and uses a persistent length-prefixed JSON
    protocol. MCP retains its binary protocol and remains available. Automatic
    registrations use a root scope with every supported positive source glob
    and bounded admission, preventing later nested projects or languages from
    falling outside a sampled plan. Two bounded existence probes replace broad
    startup enumeration.
    Coverage includes safe text documentation/configuration; hidden paths remain
    rejected and default secret/key exclusions apply. `code_files` searches
    admitted filenames/paths, separately from content matching. Missing native
    roots use the shared registry preparation path only after Git verifies
    membership in an explicitly operator-authorized registered parent.
    Registry updates are serialized and remain bounded. Parent coverage and
    revocation apply to CLI, MCP and native tools; inherited worktrees cannot
    grant further access or silently fall back to a main repository.
11. Native client tools and MCP are complementary rather than mutually
    exclusive. Direct VS Code chats can invoke client extension tools; delegated
    and headless sessions may receive only server tools. Both surfaces terminate
    at the same daemon. Startup verifies Native execution through
    `vscode.lm.invokeTool(codeWorkspaces)`, not catalog presence alone.
12. Automatic registration drift repair stores a validated, bounded fingerprint
    of extension-created plans. Any external/manual change revokes automatic
    ownership, preventing zero-click maintenance from overwriting intentional
    narrow scopes.
13. Registration migrations require explicit application and explicit scope
    additions; existing excludes, prewarm and admission policies are preserved.
    Explicit managed worktree pruning removes only nonexistent roots. At
    registration capacity, the same bounded metadata check reclaims nonexistent
    managed roots automatically; existing/manual roots remain protected.
    Recovery is logged only after atomic registry commit. If no room remains,
    fail with actionable guidance. Runtime root LRU eviction is a separate
    memory policy.
14. Diagnostics distinguish installed, client and daemon build fingerprints,
    capability schemas, current coverage and unverified chat exposure. They
    never terminate active sessions. Per-session adoption samples exclude
    legitimate fallback and unknown-corpus requests from eligible-use rates.
15. Shared search freshness is explicit, including zero-result responses.
    Returned grep/file paths and explicit file targets receive live metadata
    checks; context snippets receive live SHA-256 checks even when cached.
    A known mismatch returns STALE_INDEX with the root/path and policy-aware
    live-read guidance, not stale success. Candidate checks cannot prove global
    absence; the response never claims an atomic/full-disk search snapshot.
16. Window reload is registry-only readiness, after normal startup and with
    overlapping probes coalesced. Source discovery, automatic registration and
    worktree preparation occur on first source lookup or explicit registration,
    not readiness. Shared code_status is an observational snapshot, including
    unloaded roots; it never cold-loads source, reconciles events or extends
    the root's idle lease. Capability checks do not imply a warmed source index.
17. CCTX is an optional nonblocking root cache. Four ordinary source operations
    return CACHE_MISS immediately when cold/loading/syncing and queue only the
    actual requested root. Copilot continues with a permitted host search/read,
    not another CCTX transport or readiness polling. Misses are not empty source
    results. At most8deduplicated root demands and one preparation/load/sync
    worker are allowed; explicit sync/profile requests alone may defer (32max,
    28second deadline). Registration/Git membership is verified in the worker
    before publication; the main loop rechecks registered parent policy.
    Only explicit coverage diagnostics list sibling worktrees. Background memory
    is reserved from the same cache budget. Automatic prewarm and idle pinning
    are disabled. Typed application/miss errors do not trigger reconnect/retry.
18. Recent timing/eviction histories are bounded. Shared broad reads can split
    into labeled500line snippets; opt-in pages have explicit completion and
    continuation ranges, never silent partial success or weakened exclusions.

19. Trace capability negotiation and a bounded private argument correlate client
    and daemon requests without changing binary DTO layouts. Only opted-in
    responses carry a trace envelope; clients unwrap it before source formatting.
    The first MCP/native content item keeps its old shape; a diagnostic content
    item exposes the correlation ID to host event logs. Client histories are
    bounded to64 records of at most4KiB and contain no source bodies. MCP JSON
    lines are serialized once before writing. Telemetry must not make native
    source results wait behind later source requests.
20. Cold admission failures carry typed required-byte measurements. Retry
    admission evicts the minimum LRU prefix for the measured lower bound and
    bounded staging reserve; an impossible build does not purge working roots.
    Parent size hints seed actual requested worktree builds, never sibling loads.
    Whole-inventory source floors reject insufficient reservations before source
    reads. Reported admission requirements are lower bounds; after an initial
    staging-reserve increase, retries grow reservations geometrically, up to six
    retries and the adaptive total target. Even a last remaining root may
    retry within that bound. The error-history limit remains32 independently of
    the metadata registry. Actual failures are logged and exposed
    with a60second demand-driven retry cooldown. Native first-use registration
    is also bounded/coalesced, asynchronous, and returns a miss before scanning.

## Delivery sequence

CPU/I/O follow-up: compiled read-path matchers are a single root-local cache
keyed by effective policy, never cached admission decisions. Unchanged metadata
results avoid resident reconstruction/accounting. Idle loop ticks are1second,
active root/explicit work ticks25ms, and arrivals wake the receiver immediately.
Periodic runtime identity verification terminates missing/replaced endpoints.
The service verifies adaptive memory bounds while lowering CPU/I/O scheduling
weights and applying home-filesystem physical read/write bandwidth guardrails.

1. Core: stable file IDs, incremental posting mutations, query planner and limits.
2. Lifecycle: private registry/socket, shared process, watchers, error recovery.
3. Surfaces: ergonomic CLI, seven named MCP tools and seven native VS Code tools
   with honest freshness labels.
4. Verification: existing regression suites plus update/rename/ignore/race,
   malformed input, startup/restart, parallel client and scope-isolation tests.
5. Performance: same frozen corpus and query contracts as previous runs; measure
   actual Unix RPC and stdio proxy, build/RSS and single-file update separately.
6. At least ten review criteria per hardening round; repair every Medium+ finding
   and repeat tests until no known Medium+ findings remain.
7. Install release binary/user service, retain the distinct MCP entry, and
   install the native VSIX without replacing either path. Verify registration,
   activation, actual shared requests and restart.

## Acceptance and interpretation

All returned matches/counts/ranges must agree with the baseline contract.
Report warm search and IPC latency separately from indexing/update costs.
Do not call whole-agent execution faster without an agent workload measurement.
Do not infer cold SSD I/O from warmed tests or unavailable kernel counters.
Document actual residual constraints instead of declaring universal perfection.
Extension callability is not chat exposure or selection. Adoption reports
separate lookup selection, call success, recognized nonempty/empty results and
unknown payloads; catalog name mentions are explicitly not verified exposure.
