# Shared Rust code-context delivery report

> **Reviewer**: Copilot SDK assistant (GitHub Copilot)

## Scope

The implemented delivery is a source-search/read acceleration tool, not a new
assistant. See [DESIGN.md](DESIGN.md) for the bounded plan and
[README.md](README.md) for the supported interfaces and freshness contract.
No embeddings, external inference service, semantic solver or editor extension
was added. Agent-San is only a read-only benchmark/registered source root.

## Hardening record

Review covers the full implementation, not only the initial snapshot prototype.
Severity refers to defects within the stated trusted-local-user/saved-file
contract. It is not a claim of a formal security audit or mathematical absence
of all bugs.

### Round 1 — state correctness, boundary and lifecycle review

| # | Critique | Severity before repair | Repair / regression evidence |
|---|---|---|---|
| 1 | Own source access events can fill the bounded watcher queue, causing repeated full verification | High | Filter access events before queue insertion; 1,600-file startup regression |
| 2 | Explicit roots outside Git may omit root ignore rules | High | Apply root `.ignore`/`.gitignore` on discovery; root policy remove/restore regression |
| 3 | Lost notifications must not certify stale state as fresh | High | Overflow/rescan triggers full-source verification; errors block successful source replies |
| 4 | A single incomplete socket message can block all clients | High | Bounded parallel input readers with 2s deadline; incomplete-client regression |
| 5 | Applying half a changed-file batch could corrupt index/source agreement | High | Stage all fallible reads/parsing/budget checks before mutation; failed-update repair test |
| 6 | File-ID reuse can reorder results or retain old postings | Medium | Sorted posting mutation, exact source verification, path-order and repeated-update differential tests |
| 7 | A corpus hash computed at initial load becomes misleading after updates | Medium | Clear load-time digest on changed generation; explicit hash regression |
| 8 | Dropping all roots at cache capacity defeats reuse | Medium | Evict only least-recently-used root; idle expiry bounded to request activity |
| 9 | Positive globs must not unignore excluded files | High | Source type filters, explicit hidden exclusion and ignored-read regressions |
| 10 | Invalid regex/path/range inputs must not become empty successes | Medium | Explicit errors, bounded regex compilation, traversal/regex/range tests |
| 11 | Malformed/oversized wire input and duplicate daemon ownership need explicit failure | Medium | Bounded framing/queues, process lock, restart and malformed-protocol tests |
| 12 | Read-only selected source may change after indexed search | Medium | `code_read` hashes live selected files and refuses mismatches; edit-time revalidation documented |

### Round 2 — integration and measured-performance review

| # | Critique | Severity before repair | Repair / regression evidence |
|---|---|---|---|
| 1 | Re-enumerating every file for an ordinary existing-file write wastes the measured advantage | Medium | Existing-file event path updates cached metadata and only changed postings |
| 2 | Unrelated generated extensions can flood invalidations in a live repository | Medium | Filter source globs/policy/directory events before queue insertion |
| 3 | Making shared mode default breaks historical CLI benchmark subprocesses | Medium | Historical benchmark explicitly selects live backend; full legacy benchmark integration test |
| 4 | CLI used from a project subdirectory must locate registered ancestor | Medium | Closest registered ancestor resolution; subdirectory regression |
| 5 | A deleted root otherwise cannot be unregistered | Medium | Saved absolute path may revoke a missing root |
| 6 | Registry size must be checked before unbounded allocation | Medium | Bounded 1MiB read and sixteen-root registration cap |
| 7 | Repeated ranges from one file should not reread/hash that file repeatedly | Medium | Deduplicate live verification by source path |
| 8 | Benchmark child cleanup must survive proxy-close failure | Medium | Nested `finally` always terminates/waits for owned daemon |
| 9 | Comparing fewer hits or different snippets can exaggerate speedup | Medium | Compare full counts, sorted first-eight payloads, ranges and hashes; remove only consistency metadata |
| 10 | A snapshot-only result does not establish shared IPC performance | Medium | Benchmark actual Unix RPC, persistent MCP proxy and short-lived CLI |
| 11 | Protocol success alone does not prove a model can discover/select the tools | Medium | Fresh Copilot CLI synthetic-code trial actually selected `code_workspaces` then `code_context` |
| 12 | Warm-cache measurements must not be described as SSD-vs-RAM bandwidth or whole-agent speedup | Medium | Explicitly label cache/contract and report all unmeasured claims |

### Round 3 — performance-tail and semantic hardening

The same twelve criteria below were reapplied. Additional issues were repaired:

| # | Critique | Severity before repair | Repair / evidence |
|---|---|---|---|
| 1 | Per-line regex/case dispatch was slower than native rg | Medium | Reuse line-aware `grep-regex` matcher across RAM documents; measure all feature modes again |
| 2 | Whole-document optimization can change `\A`/`\z` behavior | High | Inspect regex HIR, retain LF-inclusive line-buffer behavior for text anchors; 13-pattern rg differential test |
| 3 | Staged allocations need a budget before final transaction validation | Medium | Bound staged source bytes and posting counts during preparation |
| 4 | Multiple large roots need a process-wide cache target as well as per-root limits | Medium | Approximate 1GiB cache target evicts other LRU roots; service remains capped at 4GiB |
| 5 | A read benchmark can accidentally select an empty first source file | Medium | Select an explicitly nonempty fixture file, verify ranges/hashes before timing |
| 6 | JSON-RPC identifier objects should not be reflected as valid IDs | Medium | Validate scalar identifier shape; dedicated shared-MCP protocol regressions |

The other six criteria found no new Medium+ defect. Tests were rerun after each
repair; the final optimized measurements, not the slower intermediate runs, are
the authoritative delivery numbers.

### Round 4 — ignore precedence recheck

A final policy-combination check found a Medium issue: when both root policy
files exist, the explicit ignore-file order must preserve `.ignore` priority
over `.gitignore`. The order was corrected in both discovery and measurement.
A two-direction include/exclude regression now covers the interaction. The
same twelve criteria were revisited; no other new Medium+ issue was found.

### Round 5 — final convergence criteria

The final pass rechecks these twelve dimensions after repairs:

1. Literal and Unicode match/count parity.
2. Regex/case/path/glob semantics and invalid input.
3. Create/delete/modify/rename membership and ID reuse.
4. Ignore-policy changes, source-root isolation and root revocation.
5. Failure recovery without partial generation mutation.
6. Watcher self-feedback, queue overflow and periodic fallback contract.
7. Protocol framing, errors, connection/read limits and duplicate ownership.
8. Shared memory reuse, daemon restart and concurrent clients.
9. Live selected-file hash verification and explicit sync barriers.
10. Stable CLI/MCP interfaces and compatibility with historical benchmarks.
11. Frozen-corpus parity, fair operation counts, cleanup and raw-report retention.
12. Installed binary/service/MCP readiness and honest result interpretation.

Final review outcome: **no known Critical, High or Medium findings remain within
the documented contract**. This is a bounded engineering review, not a guarantee
that undiscovered defects cannot exist. Remaining Low-level operational caveats:
latency varies with shared-host scheduling; the current session may need an MCP
reload; one synthetic adoption trial is not a selection-rate study.

Known contract constraints are not silently reclassified as implementation bugs:
Linux-only delivery, no unsaved editor buffers, no atomic filesystem transaction,
serial index operations and no PCRE lookaround/backreferences are intentional.
See the README for explicit synchronization and policy-revocation guidance.

## Authoritative release measurement

Environment: Linux, Rust 1.96.0 release build, ripgrep 13.0.0.
Both final datasets used the same frozen 8,513-file / 80,866,577-byte corpus:

`69cdbd4fbb4411e34cf5c7dc0966c4129b810671035f117f7c7f3a994c31945e`

The original source repository changed in other sessions since the earlier
8,511-file snapshot. It was not edited by this implementation. Each final
workload used eight queries x thirty rounds x six modes = 1,440 observations,
plus four feature groups x two modes x twenty = 160 feature observations, and
twenty real update trials. Two workloads total **3,200 timed operation
observations plus forty update trials**, excluding warmup.

### Full-path search, milliseconds

| Workload / path | Mean | p50 | p95 | Max |
|---|---:|---:|---:|---:|
| Original, warm rg with normalized result | 15.734 | 15.448 | 18.558 | 31.591 |
| Original, shared MCP | 0.776 | 0.524 | 0.848 | 51.482 |
| Original, shared CLI | 1.939 | 1.693 | 2.341 | 49.292 |
| Holdout, warm rg with normalized result | 18.154 | 17.261 | 25.310 | 45.343 |
| Holdout, shared MCP | 2.652 | 1.427 | 10.907 | 48.807 |
| Holdout, shared CLI | 3.842 | 2.546 | 12.614 | 44.978 |

Mean shared-MCP search speedup: **20.3x original, 6.85x holdout**.
The shared implementation includes actual socket/MCP overhead, registered-root
validation and notification processing. These numbers are therefore smaller
than the earlier snapshot-engine-only ~51x; that is expected and not hidden.
The two-byte holdout pattern falls back to full RAM scan.

Periodic metadata reconciliation explains the occasional ~45--55ms spike.
Do not interpret sub-millisecond p95 as a universal maximum latency guarantee.
Raw Unix RPC samples and every per-query observation are retained in the reports.

### Context and additional features

| Operation | Baseline mean ms | Shared mean ms | Comparison |
|---|---:|---:|---|
| Original search + versioned context | 45.722 | 0.814 | Live CLI vs shared MCP, equal normalized ranges/hashes |
| Holdout search + versioned context | 48.460 | 2.939 | Same contract |
| Regex `^async def `, holdout feature trial | 17.013 | 10.724 | Warm rg vs shared MCP |
| Case folding `GET_AZURE`, holdout feature trial | 15.212 | 15.278 | Approximately equal; no meaningful acceleration claim |
| Path/glob constrained literal, holdout feature trial | 4.724 | 0.593 | Warm rg vs shared MCP |
| Known versioned range read, original feature trial | 34.281 | 1.808 | Live policy-checking CLI vs shared MCP |

The known-read baseline is **not the host's built-in view tool or a bare file
read**: it includes the historical live CLI's enumeration/policy check. No claim
is made that RAM can make every ordinary filesystem read 19x faster. Similarly,
context speedup is relative to that defined live workflow, not a claim that
native `rg -C` has equivalent whole-file hash behavior.

The first regex implementation was measured at ~30--40ms for broad regex/case
search, worse than rg. It was not accepted as complete: line-aware matching
reduced the final regex feature to ~10.7ms. Case folding remains a full Unicode
RAM scan and approximately ties the baseline. Literal indexing is the main win.

### Loading, updates and memory

| Metric | Original run | Holdout run |
|---|---:|---:|
| Lazy root load/index, including first RPC | 1,789ms | 1,712ms |
| Steady daemon RSS | 240,076 KiB | 240,128 KiB |
| Full explicit synchronization barrier | 238.8ms | 210.0ms |
| Write-to-query-visible mean | 4.36ms | 3.89ms |
| Write-to-query-visible p95 | 5.35ms | 4.33ms |
| Write-to-query-visible max | 5.40ms | 4.35ms |
| Changed-posting update core mean | 1.93ms | 1.69ms |

Existing-file updates no longer rebuild the ~1.5s whole index or duplicate the
~227MiB generation. New/delete/rename/policy changes may require membership
enumeration, so the ordinary write numbers are not a claim about every kind of
filesystem event. Explicit sync hashes all source but mutates only changed
postings. At steady long-query latency, roughly 120 searches amortize the initial
~1.8s load relative to the measured warm-rg workload; shared sessions reuse it.

## Real host integration

The release binary is installed at `~/.local/bin/cctx`, with enabled user service
`code-context.service` and a distinct user MCP entry `code-context`. Registered
roots are Agent-San's explicit source scopes and this Rust tool's source/tests.
No searched repository source or unrelated MCP entry was modified.

In an isolated fresh Copilot CLI session with only synthetic non-private Rust
source, no custom instructions and built-in MCPs disabled, a natural read-only
question caused the model to choose:

1. `code-context-code_workspaces`
2. `code-context-code_context` with a regex and surrounding context

It correctly located `stock_after_sale` and explained `checked_sub` / `None`.
The observed session took 12.77s and two tool calls. This proves one actual
discovery/selection/execution path, **not** a 90% selection rate or a measured
whole-task speedup; there was no corresponding baseline model run.
The already-running VS Code conversation did not discover the newly registered
server in its original tool list; reload MCP tools/start a new session to use it.

### Paired natural agent A/B

A follow-up directly compared how a fresh Copilot CLI agent works. The corpus
was synthetic Rust only (6,004 files including four targets), never private
source. Five paired repetitions alternated ordering:

- Built-in arm: no MCP configured; normal `glob`, `rg` and `view` remained.
- MCP arm: the `code-context` MCP was additionally available; normal tools were
  not hidden, so model selection was natural rather than forced.
- Same exact four-question prompt, `gpt-6.1-sol`, medium reasoning, fresh session,
  warmed shared daemon/index, no custom instructions or built-in remote MCPs.

| Metric | Built-in search/read | MCP available | Change |
|---|---:|---:|---:|
| Correct complete answers | 5/5 | 5/5 | Equal |
| Mean wall time | 20.30s | 17.07s | **15.9% lower** |
| Median wall time | 18.52s | 15.60s | **15.7% lower** |
| Mean tool calls | 6.2 | 3.4 | **45.2% lower** |
| Mean measured tool execution | 208.4ms | 120.2ms | **42.3% lower** |
| Mean model calls | 4.6 | 4.4 | 4.3% lower |
| Mean model dispatch time | 13.25s | 12.09s | 8.7% lower |

Observed built-in tools: 16 `view`, 8 `glob`, 7 `rg` calls.
Observed MCP tools: 9 `code_context`, 5 `code_workspaces`, 3 `code_grep` calls.
The combined context tool usually replaced a search followed by several reads.

Paired wall-time differences were noisy: MCP was faster in three pairs and
slower in two; differences (built-in minus MCP) were -3.05s, +2.05s, -4.06s,
+9.12s and +12.13s. The paired median advantage was 2.05s. With only five pairs,
this is evidence of reduced work and a promising end-to-end effect, **not a
stable population estimate or proof that every task is 16% faster**. Model
dispatch dominated wall time; direct tool acceleration is therefore necessarily
diluted at the whole-agent level.

#### Model-roundtrip follow-up

The first MCP design required `code_workspaces` in every fresh session. A secure
optimization now lets the stdio MCP infer only an explicitly registered ancestor
of its current working directory; explicit roots and `code_workspaces` remain
available for other registered projects.

A batch-context tool was also tested, but natural agents called it 13 times over
five trials and processed excessive context. That experiment made the MCP arm
17.4% slower than its paired built-in arm. The tool was removed rather than
shipping an engine microbenchmark win that harmed agent behavior.

The same five-pair design was rerun with current-root inference and no batch tool:

| Metric | Built-in search/read | MCP available | Change |
|---|---:|---:|---:|
| Correct complete answers | 5/5 | 5/5 | Equal |
| Mean wall time | 18.36s | 15.00s | **18.3% lower** |
| Median wall time | 18.33s | 13.72s | **25.2% lower** |
| Mean tool calls | 6.8 | 2.4 | **64.7% lower** |
| Mean measured tool execution | 188.4ms | 124.4ms | **34.0% lower** |
| Mean model calls | 5.2 | 3.4 | **34.6% lower** |
| Mean model dispatch time | 14.34s | 11.72s | **18.2% lower** |

Built-in trials made 10 `glob`, 8 `rg` and 16 `view` calls (34 total).
MCP trials made 5 `code_grep` and 7 `code_context` calls (12 total), with zero
workspace-discovery calls. Pair differences (built-in minus MCP) were +4.01s,
+5.53s, -0.49s, +2.98s and +4.75s: MCP was faster in four of five, paired median
advantage 4.01s. The sample remains small and inference variance remains visible;
18.3% is the observed mean, not a universal guarantee.

## Reproducibility and artifacts

Historical delivery validation at this stage: **52 tests passed**, strict all-target clippy passed with
warnings denied, release build passed, Python critical-name checks passed,
installer shell syntax and installed systemd unit verification passed.
The final round contains no known Medium+ findings in the stated contract.

- [Original final raw report](logs/2026-10-17/shared-original-optimized.json)
- [Holdout final raw report](logs/2026-10-17/shared-holdout-optimized.json)
- [Synthetic host event report](logs/2026-10-17/agent-smoke.json)
- [Paired natural agent A/B](logs/2026-10-17/agent-ab-v2.json)
- [Current-root A/B](logs/2026-10-17/agent-ab-v4.json)
- `agent-ab-v3.json`: intentionally retained negative batch-tool experiment
- [Final test/build validation](logs/2026-10-17/validation-final.txt)
- `shared-*-release.json`: intermediate pre-regex-optimization comparisons
- Earlier `resident-*.json`: snapshot-only engine experiments, not delivery IPC

Source is never sent to another service by the search implementation or the
performance harness. The host adoption probe uses synthetic code only.
Cache flushing, SSD cold-read bandwidth, broad model-choice statistics and
full-agent task-time improvement remain deliberately unclaimed.

## Final storage-path and request-tail follow-up

The earlier warm exact-search average was below the requested 30x target because
periodic source reconciliation ran on the first request after each two-second
interval, the MCP proxy reopened its daemon socket for every tool call, and an
unscoped posting query still walked every file to recompute eligible bytes.

Repairs:

1. Notification-driven synchronization remains on each request, but fallback
   metadata reconciliation runs during daemon idle maintenance instead of the
   source request path. Maintenance failure marks the root blocked so a stale
   success is not returned.
2. A stdio MCP proxy reuses one bounded Unix connection for successive calls.
3. The current registered root is resolved once per MCP process. The daemon still
   checks the current registry for each request, so unregistering revokes access.
4. Unscoped indexed queries use resident corpus statistics and direct posting
   candidates instead of scanning 8,515 IDs just to build the cost estimate.
5. The MCP proxy keeps one daemon connection across successive calls. A failed
   request reconnects once; a daemon-restart integration test verifies recovery.

Pre-three-round warm full-path measurement, 80 samples over the original
eight-query set:

| Operation | Baseline mean | RAM mean | Mean speedup | p50 speedup | p95 speedup |
|---|---:|---:|---:|---:|---:|
| Exact search: warm `rg` vs persistent MCP | 16.110ms | 0.537ms | **30.02x** | **31.44x** | 20.31x |
| Search + versioned context | 46.313ms | 0.606ms | **76.44x** | **99.54x** | 51.32x |

At this stage the 30x goal was met for mean and median exact search, not p95. Update visibility
remained 4.97ms mean / 7.47ms p95 in this run. Raw report:
`logs/2026-10-17/shared-query-fastpath.json`.

### Private-copy cold-hint experiment

To avoid globally dropping caches, a private 8,515-file / 80,903,213-byte copy
was used. Before each `rg` search, each private file received
`POSIX_FADV_DONTNEED`; this is an advisory eviction hint, not proof of physical
NVMe/SSD access. Directory metadata may remain cached, and the environment
reports the ext4 virtual disk as rotational.

| Search path | Mean | Median | p95 |
|---|---:|---:|---:|
| cold-hint `rg` | 1,265.49ms | 1,275.08ms | 1,435.00ms |
| RAM daemon RPC | 1.264ms | 0.852ms | 1.325ms |
| Ratio | **1,001x** | **1,496x** | **1,083x** |

This establishes that repository-wide cold-ish storage search easily exceeds
30x, while retaining the caveats above. Raw report:
`logs/2026-10-17/cold-hint-final.json`.

### Why a known-file raw read is not 30x faster

For one already-known file, direct open/read/SHA-256 has little work and no IPC
or JSON. In the same private-copy experiment it beat the live-verified daemon
read:

| File size | cold-hint direct read+hash | daemon verified read | Result |
|---:|---:|---:|---:|
| 4KiB | 0.179ms | 1.483ms | direct is 8.3x faster |
| 32KiB | 0.355ms | 1.791ms | direct is 5.0x faster |
| 104KiB | 0.528ms | 2.028ms | direct is 3.8x faster |
| 198KiB | 1.101ms | 2.462ms | direct is 2.2x faster |

Therefore “minimum 30x” is a valid repository-search target, but not a valid
universal target for raw reads of small known files. `code_read` deliberately
reopens and hashes the live file to detect edit races. `code_context` is the
fast RAM path for read-only investigation; live verification is retained where
correctness before editing matters.

### Copilot first-use readiness

Frequently used registered roots can now opt into `prewarm`. Agent-San is
configured this way; other registered roots retain lazy loading and five-minute
idle eviction.

| Fresh MCP operation after daemon prewarm | Time |
|---|---:|
| Process start + initialize + tools/list | 1.74ms |
| First exact-search call | 0.91ms |
| Second exact-search call | 0.43ms |

Without prewarm, the first root request previously included about 1.6--2.2s of
source loading/index construction. Prewarm moves that cost into daemon startup
work, and registered-root tests verify loading without any source query.
Agent-San's steady service memory is about 250MiB.

The final readiness contract was tightened further: explicit prewarm and its
startup event reconciliation now complete before the daemon accepts source
requests. Service install-to-ready took 2.67s in the final run, but the cost no
longer leaks into the first agent search.

Over 600 successive exact MCP searches after a fresh ready service:

| Metric | Final latency |
|---|---:|
| MCP initialize + tools/list | 0.94ms |
| First search | 3.67ms |
| Steady mean | 0.422ms |
| Median | 0.396ms |
| p95 | 0.659ms |
| p99 | 0.788ms |

Using the previously measured warm-rg distribution as reference, this is about
38.2x by mean, 39.7x by median and 28.0x by p95. Metadata enumeration now occurs
in a background worker; an epoch prevents a stale result from overwriting a
newer watcher generation. No-change reconciliation returns before recomputing
the full posting-memory estimate.

The MCP-facing context projection also removes the root wrapper, duplicate
consistency strings and match text already present in snippets. For the measured
eight-hit context response, payload size fell from 5,224 to 3,880 bytes
(**25.7%**), while CLI/daemon diagnostics retain the full metadata.

### Three additional optimization rounds

#### Round 1 — production query hot path

Normal `code_grep`/`code_context` no longer construct phase timing, plan strings
or profiling metadata. Explicit internal profile operations retain the old
instrumented path. Unscoped full scans also preserve existing BTree path order
instead of sorting all file IDs again.

| 600-search metric | Before | Round 1 | Reduction |
|---|---:|---:|---:|
| Mean | 0.422ms | 0.339ms | 19.7% |
| Median | 0.396ms | 0.295ms | 25.5% |
| p95 | 0.659ms | 0.593ms | 9.9% |

#### Round 2 — compact before daemon serialization

The daemon now projects MCP responses before JSON serialization/Unix transfer,
instead of serializing the full diagnostic object and compacting it in the proxy.
An alternating same-daemon controlled test (1,200 samples per arm) measured:

| Wire path | Mean | Median | p95 |
|---|---:|---:|---:|
| Proxy-side compaction | 0.314ms | 0.291ms | 0.558ms |
| Daemon-side compaction | **0.297ms** | **0.283ms** | **0.531ms** |

The controlled improvement was 5.3% mean, 2.8% median and 4.9% p95.

#### Round 3 — Copilot context-first selection

A stronger tool description eliminated `code_grep` in three natural agent
trials, but models repeated `code_context`: MCP tool calls rose from the previous
2.4 to 2.67 per task and model calls from 3.4 to 3.67. All three answers remained
correct and the MCP arm still beat its paired built-in arm by 22.2%, but the new
guidance did not improve over the prior MCP behavior. It was rolled back.
`logs/2026-10-17/agent-ab-v6.json` is retained as negative evidence.

#### Robust final distribution

Five consecutive blocks of 600 exact MCP searches (3,000 total) produced:

| Final metric | Median across blocks | Warm-rg reference | Speedup |
|---|---:|---:|---:|
| Block mean | 0.344ms | 16.110ms | **46.86x** |
| Block median | 0.320ms | 15.733ms | **49.14x** |
| Block p95 | 0.577ms | 18.448ms | **31.95x** |

Relative to the pre-round steady distribution, these are 18.5%, 19.1% and 12.4%
reductions respectively. Raw artifacts:
`logs/2026-10-17/three-rounds-final.json`,
`logs/2026-10-17/three-rounds-5blocks.json`, and
`logs/2026-10-17/agent-ab-v6.json`.

## Regex and case-insensitive optimization

### Implemented algorithm

1. Parse once into HIR and compile the same line-aware matcher used for final
   verification.
2. Recursively derive sound mandatory conditions:
   concatenation becomes AND, fully covered alternation becomes OR, positive
   repetition keeps its child condition, optional/unknown branches weaken to
   full scan. Bounded prefix/suffix extraction supplements inner literals.
3. Evaluate conditions over existing file trigram postings. Intersect with
   path/glob scope. Never treat posting membership as a regex match.
4. Use the plan only when tuning showed candidate bytes at or below 75% of the
   eligible scope and modeled posting cost is lower than scanning the scope.
5. Cache up to 8 compiled matcher/plan entries (256KiB pattern budget).
6. For a leading unbounded character-class repetition such as
   `\\w+_handler`, test the same mandatory condition per line before invoking
   the expensive matcher.
7. If no safe/selective condition exists and at least 8MiB/128 files remain,
   scan path-ordered chunks with up to eight workers; merge counts and first
   results deterministically.

The tuning threshold came only from `regex-tune.json`. The holdout was not used
to pick it. Case-insensitive planning comes from case-expanded HIR literals;
when Unicode equivalence cannot be represented safely and compactly, it falls
back. `K`/Kelvin-sign correctness has a dedicated regression.

### Workload and correctness

- 32 fixed tuning queries and 32 untouched holdout queries.
- Anchors, selections, optional/repeated branches, internal/suffix literals,
  broad/no-literal expressions, path/glob scopes, Korean and Unicode case fold.
- Twenty timed repetitions per path, after warmup.
- Every run first compares full count, sorted lines and truncation with current
  `rg`, then compares planned execution with the identical matcher in forced
  full-scan mode.
- Final automated suite: 63 tests, including generated substring checks,
  regex boundary parity, candidate-plan soundness, cache eviction, Unicode
  equivalence and large parallel-scan ordering.

The source corpus changed slightly during other concurrent sessions, so old and
new wall-clock reports are not bit-identical corpora. Exact planner/full-scan
ratios below are same-run. Cross-run before/after numbers are shown separately
and not presented as a controlled hardware comparison.

### Result

| Metric | Tuning 32 | Holdout 32 |
|---|---:|---:|
| Queries using indexed candidate plan | 19 | 17 |
| Queries using targeted line prefilter | 1 | 0 |
| Queries using large-scan parallelism | 23 | 24 |
| Median internal matcher speedup, all queries | 1.42x | 1.25x |
| Median internal matcher speedup, indexed subset | 2.80x | 1.98x |
| Median internal matcher ratio, fallback subset | 0.98x | 1.00x |
| Mean pre-optimization shared latency | 137.04ms | 58.24ms |
| Mean final shared latency | 28.17ms | 20.44ms |
| Approximate cross-run ratio of means | 4.87x | 2.85x |
| Median per-query cross-run improvement | 2.06x | 2.13x |
| Per-query cross-run regressions over 10% | 0 | 0 |

Representative final same-run values:

| Query | Final mean | Forced full-scan mean | Improvement |
|---|---:|---:|---:|
| `\\w+_handler\\b` | 20.75ms | 266.31ms | 12.83x |
| case-insensitive `GET_AZURE` | 2.23ms | 6.56ms | 2.94x |
| case-insensitive provider literal | 1.26ms | 4.91ms | 3.91x |
| main-guard regex | 2.31ms | 8.80ms | 3.81x |
| no-literal camel-case regex | 166.39ms | 166.50ms | 1.00x |

The original single-thread shared baseline for `\\w+_handler\\b` was about
1,501ms; the combined planner/prefilter/parallel release is about 20.75ms.
Broad patterns such as `.*` remain dominated by enumerating matching lines; no
index can avoid producing and counting those matches under the current contract.

Cold matcher/plan compilation was about 0.49ms median and 3.0--3.8ms p95.
Repeated queries report zero compile time through the cache. With 64 distinct
queries loaded through a 32-entry cap, service memory was 264,388,608 bytes,
versus 237,862,912 bytes observed before this optimization: about 11% higher,
within the planned 20% budget. The cache retained 32 entries / 734 pattern bytes.

### Block-index decision

A line/block trigram index was **not added**:

- After line prefiltering, the worst selective matcher scan was only a few
  milliseconds and indexed-subset median matcher speedup already reached
  2.80x/1.98x.
- Remaining dominant queries (`[A-Z]...`, `\\w+`, `.*`, Unicode word classes)
  have no sound fixed literal. A block trigram index cannot exclude their
  blocks; it would duplicate postings and update work without reducing scan.
- Large full scans now use bounded parallel workers and preserve deterministic
  result order.

This is the planned measurement gate working as intended, not an unfinished
feature. A future character-class or structural index would be a different
scope and must independently justify memory, update and correctness costs.

Raw artifacts:

- `logs/2026-10-17/regex-tune-baseline.json`
- `logs/2026-10-17/regex-holdout-baseline.json`
- `logs/2026-10-17/regex-tune-final.json`
- `logs/2026-10-17/regex-holdout-final.json`

## Ten additional focused optimization rounds

All low-level rounds used five blocks of 600 persistent-MCP exact searches unless
the round targeted a different contract. Only measured wins were retained.

| Round | Experiment | Decision | Measured outcome |
|---:|---|---|---|
| 1 | Dedicated unscoped case-sensitive literal path | Adopt | Mean -13.1%, median -12.6%, p95 -7.9% |
| 2 | Rarest posting depth 1/4/8/16/32 | Adopt 8 | Versus 32: mean -10.4%, median -13.2%, p95 -10.9% |
| 3 | Generation-aware result LRU, 32 entries / 4MiB | Adopt | Repeated: mean -17.5%, median -35.2%, p95 -12.8% |
| 4 | Structural response-size accounting | Adopt | Cache-miss hit responses: mean -4.4%, median -2.7%, p95 -9.3% |
| 5 | Thin LTO, one codegen unit, stripped symbols | Adopt | Three-run median: mean -4.1%, median -4.2%, p95 -4.3% |
| 6 | Binary persistent MCP-daemon framing; JSON CLI retained | Adopt | Mean -39.2%, median -24.8%, p95 -45.2% |
| 7 | Generation-aware context LRU, 16 entries / 4MiB | Adopt | Context: mean -3.1%, median -5.4%, p95 -2.7% |
| 8 | Line-oriented instead of JSON model payload | Roll back | Payload -7.3%, but paired agent wall time/calls increased |
| 9 | Hide management tools; expose core four only | Roll back | Both 5/5 correct, but core set was 2.6% slower with more calls |
| 10 | Eight concurrent MCP clients across maintenance | Validate | 48,000 calls, 0 errors, 18,568 calls/s, p99 1.371ms |

### Final exact-search distributions

| Path | Mean | Median | p95 | Warm-rg speedup |
|---|---:|---:|---:|---:|
| Repeated, cached query | 0.153ms | 0.140ms | 0.244ms | **105.38x / 112.73x / 75.69x** |
| Cache miss with result payload | 0.258ms | 0.249ms | 0.346ms | **62.48x / 63.21x / 53.37x** |

The repeated distribution uses six realistic symbols repeatedly and therefore
benefits from the generation-aware result cache. The cache-miss distribution
varies query limits from 1 through 600 and continually evicts the 32-entry LRU;
it demonstrates that the uncached path also remains far above the 30x target.

Relative to the baseline immediately before these ten rounds, repeated-search
mean fell 62.4%, median 64.1% and p95 65.0%.

### Round details and safety

- Exact cached results and contexts include the resident generation in their
  key; changed files cannot hit an old generation. Result cache is capped at
  32 entries/4MiB and context cache at 16 entries/4MiB.
- Posting depth experiments retain original-byte verification. One/four
  postings created too many false-positive candidate files; eight was fastest.
- Structural payload accounting is a conservative upper bound for JSON escaping,
  retaining the 256KiB response limit without serializing the same object twice.
- Binary framing carries a bounded bincode envelope and JSON payload; newline
  JSON remains supported for CLI/diagnostics. Persistent reconnect and malformed
  legacy protocol tests pass.
- Text output and reduced tool schemas were not shipped despite smaller prompts:
  natural model behavior outweighed micro-level savings.

Raw artifacts are under `logs/2026-10-18/`, especially
`ten-rounds-summary.json`, `round10-final-repeated.json`,
`round10-final-cache-miss.json`, `round8-json-vs-text.json`,
`round9-full-vs-core-tools.json`, and `round10-concurrency-long.json`.

### Regex optimization hardening

Three focused rounds used twelve criteria: candidate soundness, alternation and
optional semantics, Unicode case equivalence, anchor/empty-match semantics,
cost-gate regressions, line-prefilter safety, parallel ordering/counting,
matcher-cache keys/eviction/memory, long-query resource bounds, path/glob scope,
incremental-generation independence, and benchmark fairness.

| Round | Medium+ finding | Resolution |
|---|---|---|
| 1 | Applying a line prefilter to every selective regex slowed ordinary matcher-prefilter cases | Restrict it to a measured expensive shape: leading positive unbounded character-class repetition |
| 1 | `^`/text-anchor classification could change execution semantics | Preserve existing line-aware matcher path; differential `\\A`, `\\z`, LF/CRLF tests |
| 2 | No-literal regexes remained single-core and slower than rg | Deterministic path-chunk scan, max eight workers, exact ordered merge |
| 2 | A 75%+ candidate corpus produced unreliable gains | Tune-only 75% byte-selectivity gate; holdout untouched |
| 3 | 64 compiled matchers raised service memory beyond the planned 20% headroom | Reduce to 32 entries; 64-query fill measured about 264MB total service memory |
| 3 | A 16KiB exact literal could force thousands of posting intersections | Check at most the 32 rarest trigram postings, then verify original bytes |

Final focused review found no known Medium+ issue within the documented regex
contract. Low caveats remain: periodic reconciliation adds occasional tail
latency, parallel speed depends on host load, and patterns without mandatory
literals cannot gain index selectivity.

## Memory-admission hardening (12-round convergence)

The following rounds specifically address hostile or accidental source-set
shape, initialization peak memory, runtime pressure and preservation of the
search-speed objective. Every Medium-or-higher finding was repaired and its
targeted test rerun before the next round.

| Round | Critique | Severity | Resolution / evidence |
|---:|---|---|---|
| 1 | Source bytes alone do not bound tiny-file, path or line metadata | High | Persist limits for files, path bytes and total lines; inventory rejects before source loading |
| 2 | Per-file and aggregate trigram cardinality can dominate source size | High | Per-file/root unique-trigram and 64M posting caps with build-time admission checks |
| 3 | `String` paths, `usize` line offsets and hex digests duplicate memory | Medium | Shared `Arc<str>` paths, `u32` offsets and binary SHA-256 in resident documents |
| 4 | Broad/duplicate scopes and generated trees can expand silently | High | Canonical scope collapse, include/exclude count/byte caps, explicit exclude globs and side-effect-free `plan` |
| 5 | Initial construction could exceed the global target before admission | High | Remaining global budget is passed into resident construction; build peak checks fail before insertion |
| 6 | Incremental staging could hold old and new generations beyond budget | High | Changed-file, staged-byte, posting, line, trigram and transaction-peak checks precede mutation |
| 7 | Strict sync incorrectly treated every verified file as changed | High | Limit actual staged replacements, not verification targets; rollback/generation regression |
| 8 | Loader/excluded-tree events could fill the watcher queue | Medium | Filter access/access-time and explicit-exclude events; startup and 5,000-event flood regressions |
| 9 | Matcher/result/context caches were absent from admission estimates | High | Matcher LRU reduced to 8; conservative 2x automata estimate and 160MiB root runtime headroom |
| 10 | Failed prewarm and process pressure could produce retry/OOM loops | High | Exponential retry backoff, cache/root pressure eviction, `MemoryHigh=3G`, `MemoryMax=4G`, restart-rate limiting |
| 11 | RSS claims and registry/config reads could be misleading or unbounded | Medium | Bounded 1MiB registry read, 16-root cap, process RSS plus cgroup v2 current/high/max/events in status |
| 12 | Safety checks regressed cold index construction | Medium | Reordered root-cap lookup and adopted an admission-accounted 24-bit dense epoch index for large corpora |

### Adversarial and real-corpus results

| Measurement | Result |
|---|---:|
| Tiny-file stress | 100,001 zero-byte files |
| Default 100,000-file rejection | 0.31s, 11,320KiB peak RSS, explicit `max_files` error |
| Explicit 100,001-file inventory | 0.30s, 11,052KiB peak RSS |
| Inventory totals | 1,200,012 path bytes, 0 source bytes |
| Agent-San inventory | 8,515 files, 80,903,213 source bytes, 374,348 path bytes |
| Agent-San `plan` | 0.04s, 9,168KiB peak client RSS |
| Resident lines / postings / unique trigrams | 1,963,561 / 16,975,323 / 185,602 |
| Resident structural estimate | 235,835,615 bytes |
| Observed daemon RSS, three-run median | 237,764,608 bytes (226.75MiB) |
| Conservative root admission charge | 403,607,775 bytes (resident plus runtime headroom) |

The cold-load comparison used the same Agent-San checkout, scopes, globs and
three-restart harness for both binaries:

| Binary | Median wall | Median index build | Outcome |
|---|---:|---:|---:|
| Previously installed stable binary | 2.217s | 1.241s | baseline |
| Memory-hardened dense-index release | **1.944s** | **0.945s** | **12.3% / 23.9% faster** |

The dense table exists only while building a source set of at least 16MiB, uses
32MiB, and is counted against the in-progress admission budget before
allocation. Small corpora retain the bounded HashSet path. The final dedicated
large-corpus regression exercises the dense path, while ordinary exact-search
results continue to verify original source bytes.

The post-install persistent-MCP benchmark ran five blocks of 600 exact searches:

| Metric | Final | Previous run | Warm-rg reference | Final speedup |
|---|---:|---:|---:|---:|
| Median block mean | 0.162ms | 0.153ms | 16.110ms | **99.33x** |
| Median block median | 0.152ms | 0.140ms | 15.733ms | **103.20x** |
| Median block p95 | 0.242ms | 0.244ms | 18.448ms | **76.25x** |

The mean/median shift is within the overlapping five-block host distribution;
p95 improved slightly. All three remain far above the 30x target. Raw output:
`logs/2026-10-18/memory-hardening-final.json`.

### Final residual review

After twelve rounds, no known Critical, High or Medium memory-safety finding
remains within the documented local-user contract. Remaining Low caveats:
allocator RSS may not fall immediately after eviction; a cgroup shared with
unrelated processes is reported but used for pressure only when a finite managed
`memory.high` is present; explicit `--allow-root-scope` can intentionally admit a
large root, still subject to all persisted policy limits.

## Runtime leak and ignored-tree stability hardening

The next audit specifically tested the long-lived daemon rather than only
resident source size. The first synthetic repository proved that
`node_modules` was included when no ignore file existed. Merely adding a search
glob would not fix recursive inotify amplification, so discovery, persistence,
watch planning, lifecycle and service limits were hardened together.

### Implemented improvements (28)

| # | Risk / improvement | Resolution |
|---:|---|---|
| 1 | `node_modules` included without ignore files | Default directory exclusion |
| 2 | Rust/Java/JS build trees amplify inventory | Exclude `target`, `build`, `dist`, `out` |
| 3 | Python environments/caches amplify inventory | Exclude `venv`, `env`, `.venv`, tox/nox/pytest/mypy/ruff caches |
| 4 | Front-end/mobile generated dependencies | Exclude `.next`, `.nuxt`, `.gradle`, `.dart_tool`, `Pods`, `DerivedData` |
| 5 | Defaults could hide intentionally searched generated source | Persisted `--no-default-excludes` opt-out |
| 6 | Registration could not preview effective policy | `plan` reports effective excludes and watch count |
| 7 | Recursive watcher entered ignored dependency trees | Non-recursive source-parent watch plan |
| 8 | Manual watch traversal could diverge from `.gitignore/.ignore` | Derive watched directories from the exact rg-permitted file set |
| 9 | Directory count could exhaust inotify quota | Persisted `max_watch_dirs=50,000` admission |
| 10 | Long watch paths could amplify metadata | Reuse total path-byte budget; store relative watch paths |
| 11 | Watch updates could partially apply on failure | Addition/removal rollback before publishing desired set |
| 12 | New nested source directories still need prompt discovery | Watch scope ancestors and refresh membership on directory events |
| 13 | Initial inventory/watch race could miss a pre-watch file | Reconcile post-load stamp membership before readiness |
| 14 | New watch inventory duplicated root enumeration | Transfer the bounded inventory into Resident construction |
| 15 | Opening socket frame could grow until newline | 2MiB `take` bound before parsing |
| 16 | External rg stdout/stderr could grow or deadlock | Concurrent bounded pipe readers |
| 17 | External rg/FUSE stall could retain maintenance forever | 30-second child timeout and kill/wait |
| 18 | Thread creation failure could leak active state | Fallible named spawns with explicit rollback |
| 19 | Panicking/early-return RPC worker could leak connection count | RAII active-connection guard |
| 20 | A response waiter could remain forever | 35-second response-channel timeout |
| 21 | Thread stacks amplified 32 connections and parallel regex | 256/512KiB bounded worker stacks |
| 22 | Idle roots persisted forever when no later request arrived | Timer-driven idle eviction |
| 23 | Dropped roots/caches could remain in glibc arenas | `malloc_trim(0)` after eviction/pressure release |
| 24 | Pressure checks ran at 10Hz | Throttle to once per second |
| 25 | Cached query keys were absent from memory accounting | Include pattern/path/glob and vector structure |
| 26 | One read request cloned every indexed path | Membership check plus requested-path-only live read |
| 27 | Incremental delta allocated old/new union before checking | Sorted set-difference delta with pre-insert peak admission |
| 28 | Service lacked task/fd/stop containment | `TasksMax`, `LimitNOFILE`, `TimeoutStopSec`, `KillMode` |

### Twelve hardening rounds

| Round | Focus | Result |
|---:|---|---|
| 1 | Synthetic no-ignore dependency tree | Reproduced `node_modules` inclusion |
| 2 | Default policy and backward-compatible registry | Defaults/opt-out persisted and tested |
| 3 | Sparse watcher integration | Found and repaired root ignore-file watch regression |
| 4 | Socket/subprocess bounds | Oversized opening and command-timeout regressions pass |
| 5 | RPC/thread lifecycle | 500-connection thread recovery and RAII counter pass |
| 6 | Cache/read transient memory | Query-key accounting and full-path clone removal pass |
| 7 | Incremental transaction peak | Delta set-difference and pre-growth budget tests pass |
| 8 | Ignore-policy SSOT | `.gitignore` vendor tree consumes zero child watches |
| 9 | Initial-load race/performance | Inventory ownership reuse plus membership reconciliation |
| 10 | Full suite/lint/service validation | 89 tests, Clippy warnings 0, unit verification pass |
| 11 | Long-running memory soak | 20,000 regex + 1,000 connections converge to plateau |
| 12 | Installed service and performance | Final operational validation after install |

### Measured impact

| Measurement | Before | Hardened |
|---|---:|---:|
| Agent-San inotify watches | 13,009 | **430 (-96.7%)** |
| Cold-load median | 2.217s installed baseline | **1.599s (-27.9%)** |
| Cold-load RSS median | 237,764,608 bytes previous hardening | **228,786,176 bytes (-3.8%)** |
| Plan | — | 0.07s, 9,524KiB peak client RSS |

Post-install repeated exact-search validation measured 0.172ms mean, 0.166ms
median and 0.251ms p95 across five 600-query blocks. Against the frozen warm-rg
references this is **93.52x / 94.71x / 73.59x**. Raw output:
`logs/2026-10-18/stability-hardening-final.json`.

The final soak artifact (`logs/2026-10-18/stability-soak.json`) records fixed
cache limits throughout 20,000 regex searches. RSS growth by 5,000-query phase
was 3,616,768; 1,245,184; 397,312; then **20,480 bytes**, showing convergence.
The following 1,000 new socket connections added **0 bytes**, thread count
returned to four, and inotify watches remained exactly 430.

No known Critical, High or Medium leak/stability finding remains after round 12.
Low residuals: allocator behavior outside glibc is platform-dependent; a source
created inside a pre-existing empty unwatched deep directory may wait for the
ten-second reconciliation; opting out of defaults intentionally restores the
larger source/watch surface, still bounded by persisted limits.

## Cross-language generated-source exclusion profiles

The initial hardening covered 23 common directories. A comparison against the
GitHub standard gitignore templates found additional language-specific outputs,
but globally excluding names such as `vendor`, `deps`, `bin`, `lib`, `packages`,
`docs`, `tmp`, `logs` or `Library` would hide valid source in unrelated projects.

The final policy therefore has three layers:

1. **54 universal exact directory names** with low collision risk.
2. **8 universal glob patterns** for `build-*`, `cmake-build-*`, `bazel-*` and
   `mutants.out*` directories and descendants.
3. **27 marker-driven profiles**, scoped to the root or registered scope where
   the marker was detected.

| Profile | Marker examples | Conditional output examples |
|---|---|---|
| Node | `package.json` | `logs`, `pids` |
| Python | `pyproject.toml`, `setup.py`, direct `.py` | wheels, eggs, cover, instance, generated docs |
| CMake | `CMakeLists.txt` | universal CMake/build globs |
| .NET | `.sln`, `.csproj`, `.fsproj`, `.vbproj` | bin, obj, artifacts, publish, Generated_Code |
| Swift | `Package.swift` | Carthage Build/Checkouts, xcuserdata |
| BEAM | `mix.exs`, `rebar.config` | deps, cover, doc, `_build`, `_checkouts` |
| Haskell | `cabal.project`, `stack.yaml`, `.cabal` | `dist-*`, cabal-dev |
| Ruby/Rails | `Gemfile` | vendor/bundle, tmp/log/pkg, reports, public packs/assets |
| PHP | `composer.json` | vendor |
| Go | `go.mod` | vendor |
| Perl | `Makefile.PL`, `Build.PL`, `cpanfile` | blib, `_eumm`, local, cover_db |
| Unity | `ProjectSettings/ProjectVersion.txt` | Library, Temp, Obj, Builds, Logs, UserSettings |
| Unreal | `.uproject` | Binaries, Intermediate, Saved, DerivedDataCache |
| Dart | `pubspec.yaml` | doc/api |
| Julia | `Project.toml` | downloaded deps and generated docs |
| R | `DESCRIPTION` plus `NAMESPACE`/`.R` | cache, docs, rsconnect |
| Android | `AndroidManifest.xml` | captures |
| ROS | `package.xml` | devel, generated messages/services, isolated builds |
| OCaml | `dune-project`, `.opam` | `_opam` |
| Clojure | `project.clj`, `deps.edn` | classes, checkouts, lib |
| Rust/JVM/Scala/Godot/Terraform/Zig/Nim | canonical build marker | universal or hidden outputs |

NuGet `packages` is excluded only when `packages/repositories.config` exists.
Profiles in monorepos are prefixed to the marker's scope: for example,
`frontend/package.json` excludes `frontend/logs` but preserves `src/logs`.
Creating or removing a marker is a watcher policy event and updates membership
without re-registering. `--no-default-excludes` disables both universal and
profile rules while retaining all other source, memory and watch limits.

Validation added ten dedicated profile regressions covering every profile
family, universal exact/glob rules, ambiguous-name preservation, scope
isolation, NuGet gating, opt-out and live marker changes. The complete suite is
**99/99**, with Clippy warnings 0.

On Agent-San the policy detects `node` and `python`, preserves all 8,515 source
files and 430 watches, and reports 176 effective patterns. Three isolated cold
loads measured 1.471s median and 228,540,416 bytes median RSS. The final
post-install persistent MCP rerun measured 0.153ms mean, 0.144ms median and
0.240ms p95: **105.22x / 109.09x / 76.88x** versus the frozen warm-rg
references. Raw output:
`logs/2026-10-18/exclusion-profiles-final.json`.

## Native VS Code Language Model Tool integration

MCP was retained unchanged for Copilot CLI and non-VS Code hosts. A new
workspace extension provides the same six operations through the stable VS Code
Language Model Tool API (`contributes.languageModelTools` plus
`vscode.lm.registerTool`).

### Architecture

```text
VS Code Copilot / vscode.lm.invokeTool
  -> code-context-native workspace extension
     -> persistent CCTXJ1 length-prefixed JSON Unix socket
        -> existing Rust daemon and resident index

Copilot CLI / other hosts
  -> existing MCP stdio proxy
     -> existing CCTXB1 binary Unix socket
        -> same Rust daemon and resident index
```

The 19.25KiB VSIX contains no Rust binary or duplicate index. It:

- contributes `#cctxNativeGrep`, `#cctxNativeContext`, `#cctxNativeRead`,
  `#cctxNativeStatus`, `#cctxNativeSync`, and `#cctxNativeWorkspaces`;
- runs in the workspace extension host for WSL/SSH path correctness;
- disables tools and daemon startup in untrusted workspaces;
- resolves the active file/workspace to the longest registered ancestor;
- serializes one in-flight request on a persistent bounded connection;
- reconnects once, supports cancellation and configurable 40-second timeout;
- starts the user service when enabled and the socket is unavailable;
- resets connection and registry cache when settings change;
- checks all six names in `vscode.lm.tools` during startup without loading a
  non-prewarmed root;
- exposes a Command Palette self-test that invokes `codeStatus` through
  `vscode.lm.invokeTool`.
- turns an unregistered-workspace status click into an interactive safe setup:
  infer scope/globs automatically, execute bounded `plan`, display its totals,
  and register after confirmation; editable inputs are optional via Customize.
- defaults trusted unregistered workspaces to zero-click initialization after
  the same bounded plan; `codeContextNative.autoRegisterWorkspace=false`
  restores the interactive flow.

An Agent Host trace showed that `#cctxContext` collided with the MCP-generated
reference and executed contributor `kind=mcp`, tool
`code-context-code_context`. Native references were renamed to
`#cctxNative*`; the manifest regression asserts all six collision-free names.

A second trace showed that renamed references still did not reach Agent Host.
Inspection of VS Code 1.142's client-tool bridge found that Agent Host forwards
extension tools through enabled `languageModelToolSets`; standalone tools are
visible in `vscode.lm.tools` but omitted from the host catalog. Version 0.3.2
contributes all six tools in the `code-context-native` tool set and the manifest
test verifies exact membership.

Stable VS Code 1.142 gates that contribution behind the
`contribLanguageModelToolSets` proposed API. Renderer logs proved both failure
modes in sequence: proposal disabled, then tool names unresolved. Version 0.3.4
requests the proposal, uses `toolReferenceName` values in the set, and the
installer updates the desktop user's persistent `~/.vscode/argv.json` with
`enable-proposed-api: [\"moonchoi.code-context-native\"]`. A full VS Code
process restart is required after installation.

The final forwarding defect was a wrong `when` context key:
`workspaceTrust == true` is not defined, so every set member was filtered from
Agent Host. Version 0.3.7 uses `isWorkspaceTrusted`. A fresh Agent Host snapshot
then contained all six names: `cctxNativeGrep`, `cctxNativeContext`,
`cctxNativeRead`, `cctxNativeStatus`, `cctxNativeSync`, and
`cctxNativeWorkspaces`.

Version 0.3.5 additionally filters every hidden path component before automatic
scope inference. This repaired an FDAI registration failure where `.fdai` was
promoted to a scope and correctly rejected by the Rust path policy. FDAI now
validates as 2,855 files, 27,022,381 source bytes, 100 watches, and Node+Python
profiles.

Version 0.3.6 probes every non-hidden top-level directory for at least one
supported source file instead of relying only on the first 1,000 recursive
results. This prevents large monorepos from omitting later directories such as
FDAI `services/`. The FDAI registry was expanded to scope `.`, and verified at
9,480 files, 94,030,912 source bytes, and 566 watches; the previously absent
`services/core-control-plane/.../execution_backend.py` now supports both search
and verified range reads.

### Hardening findings repaired

1. Daemon application errors were initially retried as transport failures.
2. Replacing the client on configuration change left tools holding the old
   disposed client.
3. command cancellation sources were not disposed.
4. VS Code engine and `@types/vscode` versions differed.
5. VSCE allowlist and `.vscodeignore` were incorrectly combined.
6. Clean removed the VSIX output directory without recreating it.
7. Compiled tests were initially included in the VSIX.
8. A delayed close event from an old socket could tear down its replacement.
9. Startup status probing loaded every non-prewarmed workspace root.
10. Newline JSON framing was slower than the MCP binary route.
11. Untrusted workspaces could start the daemon before trust approval.
12. Query/root and manifest/runtime tool-name contracts needed explicit tests.

Framed JSON added one Rust daemon regression while preserving legacy newline
JSON and MCP bincode protocols. The complete Rust suite is **104/104**.
Extension validation is **22/22**, with tsc strict mode, ESLint warnings 0,
npm audit vulnerabilities 0, and VSCE packaging successful.

### Actual VS Code host evidence

VS Code 1.142 loaded `moonchoi.code-context-native@0.3.4` in the WSL workspace
extension host. Its output log recorded:

```text
NATIVE_READY tools=code-context-native_codeContext,
code-context-native_codeGrep,code-context-native_codeRead,
code-context-native_codeStatus,code-context-native_codeSync,
code-context-native_codeWorkspaces workspaces=2 root=/home/moonchoi/dev/code-context
```

The startup probe left `loaded_roots=1`, proving that registration/root
validation did not force-load the non-prewarmed Code Context root.

### Native versus MCP performance

Both paths ran five blocks of 600 identical exact searches on the same daemon:

| Path | Mean | Median | p95 |
|---|---:|---:|---:|
| Native framed adapter | **0.185ms** | **0.180ms** | **0.272ms** |
| MCP stdio proxy | 0.221ms | 0.215ms | 0.321ms |
| Native improvement | **16.2%** | **16.7%** | **15.3%** |

Raw artifacts: `logs/2026-10-18/native-vscode-final.json` and
`logs/2026-10-18/native-mcp-final.json`.

## Agent adoption hardening — ten measured rounds

The final audit focused on actual Agent Host use rather than registration or
microbenchmark speed. Existing sessions and their worktrees were kept
untouched; only read-only probe chats were created. The running daemon was not
restarted during the work.

Ten recurring critique criteria were applied per round: catalog presence,
instruction presence, tool-search ranking, direct selection, completion,
fallback correctness, startup timing, payload overhead, cross-session
observability, and active-session interference.

| Round | Evidence / critique | Improvement |
|---:|---|---|
| 1 | 4-hour baseline: 27 chats, 1,220 calls; `rg` 197, `view` 273, cctx 0 | Added exact callId-based audit script; rejected snapshot-count inflation |
| 2 | Native and MCP definitions were already present in Agent catalogs | Ruled out registration as the primary cause |
| 3 | User-wide Copilot instructions had no cctx routing rule | Added cctx-first always-on user instructions with bounded fallbacks |
| 4 | Read-only probes: agent-san used rg/view; FDAI/my-avatar searched for cctx | Isolated repository-instruction conflict from catalog discovery |
| 5 | AHP proved the new user instruction loaded in fresh prompts | Documented that existing sessions retain old snapshots |
| 6 | First tool search saw MCP only; later snapshot gained Native | Changed activation from `onStartupFinished` to eager `*` |
| 7 | Raw Native JSON encouraged temp-file + jq postprocessing | Added direct `path:line` and numbered-source result formatting |
| 8 | Usage required expensive distributed-log parsing | Added fixed-cardinality daemon `protocol × operation` counters |
| 9 | Second probes reached cctx search in 2/3; agent-san still forced grep | Updated agent-san routing rules to Native first, grep fallback |
| 10 | Same-session probe retained pre-change instructions; a fresh unnamed read-only lookup then selected cctx 3/3 times | Proved the future-session contract while avoiding disruption to live sessions |

Additional Medium+ findings repaired during the rounds:

1. Baseline audit double-counted snapshot copies as tool calls.
2. Rotating Agent Host logs could disappear between enumeration and open.
3. Native activation happened about 0.84s later under `onStartupFinished`.
4. Project instructions contradicted the user-level cctx-first rule.
5. Tool descriptions said “preferred” but did not state a first-path contract.
6. Raw context output required JSON parsing despite already bounded source.
7. No daemon-level distinction existed between Native, MCP, and CLI use.
8. Detached probe chats can pause for client-tool approval and are not valid
   success-rate measurements.
9. Existing top-level sessions cache instruction snapshots.
10. Restarting the shared daemon would interrupt unrelated active sessions.
11. A test-only audit initially failed on normal Agent Host log rotation.
12. Result formatting needed to preserve path, line numbers, and source hashes.
13. Protocol metrics recorded unvalidated operation names, so unique invalid
    names could grow the supposedly fixed-cardinality map; unknown names now
    share one bounded bucket with a 100-name regression test.

The accurate baseline artifact is
`logs/2026-10-18/adoption-round1-agent-trace.json`; catalog evidence is
`logs/2026-10-18/adoption-round2-catalog.json`. The first-cycle extension had 25
TypeScript tests, while the Rust suite had 101 tests. One residual Low
caveat remains: a currently running Agent session cannot be forced to replace
its cached instructions without starting a new session.

A final fresh Copilot CLI session did not mention cctx in its prompt. It
naturally selected and successfully completed two
`code-context-code_context` calls and one `code-context-code_read` call, with
no file edits. This changes the measured fresh-session lookup path from zero
cctx calls in the historical four-hour window to **3/3 successful cctx calls**.
Raw session events are in
`logs/2026-10-18/adoption-round10-copilot-cli.jsonl`.

## Agent adoption hardening — rounds 11 through 20

The second cycle tested measurement integrity, deferred discovery, malformed
payloads, registration drift, resident replacement, and repeated natural
selection rather than repeating the original catalog checks.

| Round | Evidence / critique | Improvement |
|---:|---|---|
| 11 | Trace files were selected by mtime, but old events inside active files polluted the requested time window; missing completion booleans were counted as failures | Parse ISO event timestamps, reject missing/stale events, preserve unknown outcomes as incomplete, validate positive windows, and write reports atomically |
| 12 | A bare callId could merge calls from different Agent Host channels | Key calls by `(channel, callId)` and add collision/stale-event regressions |
| 13 | Malformed Native payloads became `?`, `0`, or `snippets=0` success-shaped text | Validate every search/snippet field and range length; malformed payloads now fail explicitly |
| 14 | Registry refresh failures rethrew the stale pre-refresh resolution error | Resolve through the refreshed registry and surface its current error |
| 15 | Daemon metrics counted attempts only and were visible on ping, not ordinary status | Add bounded protocol failure counters and expose calls/failures through `code_status` |
| 16 | Native schemas allowed empty patterns/paths | Add `minLength: 1` and manifest regressions |
| 17 | `codeContext` claimed unsupported “concept lookup” | Restrict descriptions to symbol/literal/regex behavior |
| 18 | Deferred-tool sessions treated an unloaded cctx tool as unavailable and went directly to built-ins | Require tool-search discovery before fallback; align Native and MCP first-path instructions |
| 19 | Fixed top-level scopes missed later nested projects; this repository indexed 30 Rust files but no extension TypeScript | Automatic scope is now `.`, existing globs are merged, and CLI updates preserve excludes/prewarm/policy; the root now indexes 43 Rust/TypeScript files |
| 20 | A new root could fail admission before idle roots were evicted | Use typed admission errors and retry after oldest non-prewarmed eviction; isolated `code-context → FDAI → my-avatar` loading succeeded under the 1GiB target |

The corrected one-hour audit
`logs/2026-10-18/adoption-round20-windowed.json` observed 200 actual calls and
3 MCP context calls, all successful.
Five additional independent Copilot CLI sessions across agent-san,
code-context, and FDAI did not mention cctx; every session naturally selected
exactly one `code-context-code_context` call and all **5/5 succeeded**. Their
raw artifacts are `logs/2026-10-18/adoption-round20-*.jsonl`.

The final isolated replacement proof ended with three resident roots and
1,009,636,589 estimated bytes after successfully loading my-avatar. Twelve
live shared-MCP proxies used 51,520 KiB RSS, 119 file descriptors, and twelve
threads in total; they remain client-owned session processes rather than daemon
leaks.

Final second-cycle validation: **104/104 Rust tests**, Clippy with denied
warnings, **31/31 TypeScript tests**, strict TypeScript compilation, ESLint,
`npm audit` with zero vulnerabilities, and **3/3 Python audit tests** plus Ruff.
The installed Native extension is `0.4.1`.

## Native and lifecycle hardening — rounds 21 through 30

The third cycle separated direct VS Code client-tool behavior from delegated
and headless sessions, then hardened automatic-registration ownership and audit
integrity.

| Round | Evidence / critique | Improvement |
|---:|---|---|
| 21 | A 30-minute baseline had six successful MCP calls and zero Native calls despite thousands of Native catalog mentions | Separated catalog presence, Native invocation, and model selection as distinct evidence |
| 22 | Existing exact roots were treated as automatically managed, so a startup could widen manually customized scopes | Store the exact last automatic plan in extension state and update only while the live plan still matches |
| 23 | Fresh delegated VS Code chats in code-context and my-avatar both selected MCP after tool discovery | Established the runtime boundary: delegated/headless sessions can omit client extension tools and require MCP |
| 24 | Rotated trace files can deliver complete before start; channel-less calls can collide; malformed envelopes could stop the audit | Audit schema 3 merges events order-independently, falls back to trace identity, validates envelopes, and reports per-group sessions/latency |
| 25 | `NATIVE_READY` proved registration but not execution through the Language Model Tool API | Startup now invokes `codeWorkspaces` through `vscode.lm.invokeTool` before declaring Native ready |
| 26 | Native discovery depended on global instructions outside the extension | Bundle an on-demand saved-source chat instruction and add fast/saved-source/RAM-index discovery tags |
| 27 | Documentation implied all six tools were read-only and did not state the client-tool boundary | Correct sync semantics and document direct-UI Native versus delegated/headless MCP behavior |
| 28 | Automatic-plan fingerprints could grow without bound | Add a 64-entry recency-bounded registry |
| 29 | Corrupt extension state could crash startup or regain automatic ownership | Validate roots, arrays, entry sizes, and root identity; invalid state is ignored safely |
| 30 | Two fresh probe chats increased persistent proxies from 11 to 13 | Confirm each proxy is live-chat-owned, one thread/~4.3MiB/10 FDs; the transient `/dev/null` discovery proxy exits immediately |

The official
[VS Code Language Model Tool API](https://code.visualstudio.com/api/extension-guides/ai/tools)
confirms that extension tools can be enabled or disabled by users in the Chat
tools UI and distinguishes extension tools from MCP tools. The dual surface is
therefore intentional: direct UI chats can use Native tools, while
delegated/headless/CLI sessions retain the shared MCP path.

The two fresh delegated probes completed read-only symbol lookups successfully
through `tool_search → code_context`; neither fell back to shell search. The
final schema-3 one-hour artifact,
`logs/2026-10-19/adoption-round30-schema3.json`, contains two MCP context calls:
one deliberate invalid `radius=35` contract test failed explicitly, and the
corrected call succeeded.

Final third-cycle validation: **104/104 Rust tests**, denied-warning Clippy,
**34/34 TypeScript tests**, strict TypeScript compilation, ESLint, package
validation with the contributed instruction included, `npm audit` with zero
vulnerabilities, and **5/5 Python audit tests** plus Ruff. The installed Native
extension is `0.4.2`. Existing extension hosts retain their loaded 0.4.1 code
until a new host starts; no active window or shared daemon was restarted.

## Convergence hardening — Medium and above eliminated

Three additional critique rounds continued until a full round produced no new
Critical, High, or Medium finding.

### Round A — 15 critiques, five Medium findings repaired

| # | Critique | Before | Result |
|---:|---|---|---|
| 1 | Sampled current extensions miss a language added later in a nested project | Medium | Automatic plans include every supported source glob |
| 2 | Malformed `plan` output could look successful | Medium | Strict root/scope/glob/count/profile validation |
| 3 | Malformed `register` output could claim success after mutation | Medium | Require `registered=true` and a complete matching nested plan |
| 4 | Symlink workspace paths could fail exact-root ownership matching | Medium | Canonicalize workspace roots with `realpath` before planning/state lookup |
| 5 | Auto-registration could mutate state before checking all Native tools registered | Medium | Validate the six-tool registry first |
| 6 | Common uppercase `.R/.C/.H` sources were recognized but absent from future globs | Low | Add explicit uppercase aliases |
| 7 | Automatic globs could approach the Rust 64-glob contract silently | Low | Regression assertion locks the count at or below 64 |
| 8 | Manual scope/glob changes could be overwritten | Pass | Fingerprint mismatch releases automatic ownership |
| 9 | Corrupt extension state could regain ownership | Pass | Runtime normalization rejects malformed entries |
| 10 | Extension state could grow indefinitely | Pass | 64-entry recency bound |
| 11 | New-root admission could evict before proving fit | Pass | Full-budget construction precedes eviction |
| 12 | Saved-file freshness after edits | Pass | Watcher plus explicit sync/hash barriers |
| 13 | Delegated sessions without client tools | Pass | Shared MCP fallback |
| 14 | Per-chat MCP proxy memory | Low | One thread/~4.3MiB/10 FDs, owned by live chat |
| 15 | Existing extension hosts retain old code | Low | New-host activation requirement remains explicit |

The all-language plans were measured before adoption: code-context 56 files /
475,986 bytes / 8 watches; agent-san 10,469 files / 104,709,366 bytes / 638
watches; my-avatar 360 files / 9,076,327 bytes / 24 watches. All remain within
the existing admission policy.

### Round B — 12 critiques, two Medium findings repaired

The round rechecked startup cost, picker semantics, plan/register races,
multi-root behavior, symlinks, ignore boundaries, manual ownership, state
failure, watcher freshness, malformed payloads, observability, and proxy
lifecycle.

1. **Medium:** startup still enumerated up to 1,000 files and every top-level
   directory after all-language globs made that unnecessary. Replaced with two
   one-result root/nested source probes.
2. **Medium:** `NATIVE_READY` overstated Chat picker readiness, which the API
   cannot inspect. Status now says registered/callable and logs
   `agent_picker=unverified`.
3. The other ten checks produced only the already documented Low constraints.

### Round C — 12 critiques, convergence

The same twelve checkpoints were repeated after the repairs. No new Critical,
High, or Medium issue was found. Remaining Low constraints are:

- a new extension host is required to load an installed update;
- Chat picker enablement is user-controlled and not introspectable;
- live delegated chats retain one small MCP proxy each;
- uncommon uppercase extension conventions outside `.R/.C/.H` may require
  manual Customize input.

Final converged extension validation is **36/36 TypeScript tests** plus strict
compile and ESLint. Rust remains **104/104**, and the adoption audit remains
**5/5** with Ruff clean. The final one-hour audit artifact,
`logs/2026-10-19/adoption-convergence-final.json`, records four cctx MCP calls
across two sessions with **4/4 success**. The code-context registration now
contains 56 files / 474,976 source bytes / 8 watch directories and successfully
returns shell-script context. The converged release is Native `0.4.3`.

## Transparent snapshot-read recovery

The intermittent `file is absent from the permitted snapshot` error originated
in `Resident::read_many`: a path could be created, atomically replaced, or move
between search and read after the request's first watcher drain. The old
contract surfaced that transient membership miss and told the user to retry or
run status/sync.

The repaired path uses a typed snapshot-miss error and a bounded recovery loop:

1. normal watcher reconciliation and snapshot read;
2. on snapshot miss or live-hash instability, strict inventory/hash
   reconciliation;
3. if still unstable, wait 25ms for an atomic-save rename to settle and
   reconcile once more;
4. verify live hashes before returning source.

Successful and exhausted recoveries are exposed as
`transparent_read_recoveries` and `transparent_read_recovery_failures` in
`code_status`. Ignore, hidden-path, glob, scope, binary, symlink, and file-size
boundaries remain unchanged. Native 0.4.4 no longer asks the user to retry or
run status manually.

The final safety critique covered typed error routing, bounded attempts, delay
scope, batch atomicity, live-hash verification, policy boundaries, hidden and
symlink paths, synchronization failures, observability, Native UX, MCP
reconnection, and shared-daemon activation. No Critical/High/Medium issue
remained. Full validation is **107/107 Rust tests**, denied-warning Clippy,
**36/36 TypeScript tests**, ESLint, `npm audit` with zero vulnerabilities, and
**5/5 Python audit tests** with Ruff.
