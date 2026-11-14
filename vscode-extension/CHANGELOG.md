# Changelog

## 0.26.0

- Eliminate inventory-exceeded `CACHE_LOAD_FAILED` cliffs and host-tool fallback
  storms on large polyglot repositories:
  - Add **3-tier priority inventory & graceful budget capping** (core source
    code first, configs/docs second, bulk data/rich assets third) across cold
    loads, incremental directory watcher scans, and 60s background maintenance,
    with automatic self-healing promotion back to full residency when memory
    headroom recovers.
  - Keep `#cctxNativeFiles` (`code_files`) at 100% permitted-path coverage and
    serve tier-capped files (including Markdown `#toc` / `#outline` / `#section`
    anchors) transparently on demand via `#cctxNativeRead` (`code_read`) without
    falling back to built-in `view` or `glob`, while exposing
    `inventory_complete`, `inventory_capped_files`, and `unindexed_extra_files`
    in `code_sync`, `code_status`, and `freshness` metadata.
- Harden adaptive memory admission and WSL2 host coordination:
  - Add a soft-landing reserve curve on high-RAM hosts, include Linux
    `SReclaimable` slab caches in WSL2 reclaimable memory accounting, debounce
    transient single-sample allocation spikes, recover immediately below `4 GiB`,
    extend WSL2 Windows host report TTL to `600s`, restore LRU eviction floors
    for unhinted cold loads, match `budget_for_resident` proportionally to small
    footprints, and shorten transient admission retry cooldowns from `60s` to
    `5s`.
  - Raise default `max_source_bytes` to `512 MiB` (upgrading legacy `256 MiB`
    registrations on `cctx migrate`), expand file-discovery capture to `32 MiB`
    with immunity to transient `ENOENT` build/test races, isolate corrupt or
    non-UTF8 non-code documents per-file (`skipped_unreadable_files`), and
    detect nested monorepo manifests up to two directory levels under `.`.

## 0.25.9

- Restore Native-first priority (`#cctxNativeContext`, `#cctxNativeRead`,
  `#cctxNativeGrep`, `#cctxNativeFiles` FIRST; never choose MCP when the
  matching Native tool is callable) and fix the multi-profile Windows + WSL
  extension mismatch that caused `"No client with the advertised tool
  definition was connected to run cctxNative*"`: `install-vscode-extension.sh`
  and `configure_windows_profile.py` now install the VSIX into both the base
  Windows profile and WSL-associated custom profiles (`associatedRemoteProfiles`),
  configure `"remote.extensionKind": { "dotnetpower.code-context-native": ["ui"] }`
  plus `codeContextNative.wslDistribution` and `codeContextNative.wslExecutablePath`
  across both profiles, and clean stale extension directories so all open
  windows advertise identical tool definitions.
- Prevent unsolicited post-compaction and `tool_search_tool` drift to MCP:
  mark `cctx shared-mcp` tool descriptions explicitly as MCP fallbacks for
  sessions where `cctxNative*` is unavailable, bound `tool_search_tool`
  discovery queries within the 5-tool result cap, scope dispatch-error MCP
  fallback to the current user turn (including unattended autopilot
  continuations before the next user message), and restore Native-first on the
  next foreground user turn without pinning MCP in `cctx journal` or mutating
  MCP config files automatically.

## 0.25.8

- Eliminate first-call UI client-relay failures (`"No client with the advertised
  tool definition was connected to run cctxNative*"`, `"No matching client
  connected"`, `"skipped from another client"`) in VS Code Agent Host, Copilot
  SDK, subagent, and autopilot sessions: prioritize direct server-side backend
  MCP (`code-context-code_*` / `mcp__code-context__*`) whenever callable
  alongside client-relayed `cctxNative*` tools while keeping `#cctxNative*`
  primary in Editor Chat and VSIX-only setups.
- Make client-relay fallback sticky for the entire session across autopilot
  continuations and context compactions (`Recovery` instructions, BPE prompt
  compiler surfaces, and deduplicated pinned `PostToolUse` continuity entries in
  `cctx journal`), classify `"advertised tool definition"` errors under
  `client_disconnected` in `cctx usage` and adoption audits, and auto-provision
  `code-context` (`cctx shared-mcp`) in Agent Host / VS Code MCP configs when
  `cctx` is installed locally.

## 0.25.7

- Accelerate warm daemon IPC and resident lookups (`14.92x -> 17.22x` on the
  8,960-pair AKS Store Demo mixed retrieval suite, `0.284 ms -> 0.232 ms` mean,
  `0.501 ms -> 0.355 ms` p95): serialize `code_files`, `code_grep`,
  `code_context`, and `code_read` directly to `WirePayload::RawJson` frames
  without intermediate `serde_json::Value` trees or defensive argument cloning,
  add a zero-allocation borrowed snippet fast path for warm `code_read` calls
  (`0.234 ms -> 0.190 ms`), fast-path O(1) warm-root dispatch, and decode
  single-chunk IPC frames directly in `daemonClient.ts`.
- Cut cold source index sync latency by 31% (`28.675 ms -> 19.735 ms` median,
  `5.01x` faster than v0.14): parallelize `Resident::load_with_inventory` across
  scoped `O_NOFOLLOW` + `fstat` workers, wake the daemon event loop immediately
  on background root preparation/load completion instead of polling every 5 ms,
  reuse path buffers in `check_search_paths`, and skip 4-thread scope spawns
  when single-thread `lstat` stamps match.

## 0.25.6

- Harden cross-model and subagent Code Context adoption across Gemini, Claude
  (Opus/Sonnet/Haiku), and GPT sessions: enforce turn-0 `tool_search_tool`
  discovery with separate foreground/MCP queries (avoiding the 5-tool search
  cap), require single-call `ranges` batching before multi-edits, add the
  `claude-md` (`CLAUDE.md`) prompt target, and auto-patch standalone Copilot CLI
  caches and Windows VS Code SDK subagent bundles (`explore`, `research`,
  `rubber-duck`, `code-review`, `security-review`, `task`).
- Emit proactive `PARTIAL_READ` symbol outlines on initial top-of-file single
  reads (`>120` lines) to prevent `SEQUENTIAL_SLICE` turns before they happen,
  raise `MAX_GLOBS` (`128 -> 160`) with `*.dog` default source support, and
  deduplicate unregistered-root worktree diagnostics.
- Improve `cctx usage` cross-model accuracy: ingest Claude session JSONLs
  (`~/.claude/projects`), recognize `mcp__code-context__*` MCP calls, exclude
  `tool_search_tool`/`web_search`/`/dev/shm`/`.gitignore` targets and batched
  same-file pre-edit `view` calls from fallback penalties, and report
  warm-adjusted model efficiency alongside `[cold_cache_spike]` advisories.

## 0.25.5

- Support VS Code Agents window `vscode-agent-host://` workspace, editor, and
  tab URIs (`hex-` encoded host authorities and `_ah` query metadata), matching
  WSL session roots only to the configured WSL distribution and local session
  roots only to `local` agent hosts while rejecting non-file inner schemes.
- Skip non-`file:` workspace folders during automatic registration, missing-root
  worktree verification, and manual folder selection so virtual or remote
  Agent Host session switches never attempt local `realpath` resolution, and
  document `extensions.supportAgentsWindow` setup for `file:`-only LSP servers.

## 0.25.4

- Eliminate Linux cgroup v2 `memory.high` reclaim stalls (`16s–25s` tail latency)
  and transient warm-root purges: evict and trim userspace memory before
  lowering kernel cgroup limits, debounce single-sample external memory dips,
  recover immediately from `<2 GiB` emergency floors, and clear admission
  `CACHE_LOAD_FAILED` cooldowns as soon as free RAM recovers.
- Raise the systemd service I/O read/write guard (`SERVICE_READ_BPS`:
  `32 MB/s -> 256 MB/s`, `SERVICE_WRITE_BPS`: `1 MB/s -> 16 MB/s`) for 8x faster
  NVMe cold index loads, and add an 8x (2-hour) idle lease tier for busy roots
  with `500+` lookups.
- Fix false-positive `SEQUENTIAL_SLICE` notices on single-call multi-range
  batches and targeted distant line jumps, skip individual oversized
  (`> max_file_bytes`) files during `cctx plan`/`register`/`migrate`/`coverage`
  so large JSON/data fixtures no longer block workspace registration, extend the
  native workspace registry cache TTL (`15s -> 120s`), and add cross-repo target
  attribution, oversized host-only filtering, and the `unregistered_workspace`
  advisory to `cctx usage`.

## 0.25.3

- Replay `cctx usage` token and AIC cost savings per `(session, agent)` stream
  using each turn's `copilotUsage.tokenDetails` rates, pre-compaction history
  re-read horizons, and warm-turn context sizes instead of fixed `35x`,
  `0.00055 AIC/tok`, and `4.20 AIC/turn` constants.
- Price standing BPE formula prompt compression at each turn's actual cold/warm
  prefix rate (preventing `35x` double-counting on zero-CCTX sessions), assign
  zero avoided-turn credit to `SEQUENTIAL_SLICE` reads, add the
  `sequential_slicing` optimization advisory, and sort the top-sessions table
  by actual LLM spend.
- Evict older idle sibling roots in the shared daemon before restricting an
  active root's admission budget on foreground lookups so incremental updates
  on large workspaces do not stall with `staged update exceeds resident
  admission peak`.

## 0.25.2

- Isolate oversized (`> max_file_bytes`) and binary files during initial and
  incremental watcher updates so a single large or binary artifact never blocks
  unrelated `code_read`/`code_grep`/`code_context` lookups in the workspace.
- Support partial batch success in compact `code_read` (both plain and
  `page=true` batches), allow `start=1` reads on 0-byte empty files, auto-clamp
  single-file past-EOF reads to `valid_tail` with `EOF_CLAMPED`, and broaden
  `did_you_mean` suggestions to sibling files in the same directory.
- Auto-truncate oversized `code_grep` responses at the 256 KiB budget boundary
  (`truncated=true`) instead of failing with `RESPONSE_BUDGET_EXCEEDED`.
- Emit `EOF`, `OVERLAP_READ`, and `SEQUENTIAL_SLICE` unread-symbol outline
  notices (`unread_outline=[L...:fn/def/class ...]`) on sequential single-file
  reads, elide unchanged middle lines on exact duplicate MCP reads, and add the
  per-workspace `low_context_share` advisory to `cctx usage`.
- Piggyback `_cctx_prev_trace` telemetry across both `cctx shared-mcp` and
  the VS Code WSL native bridge (`usesWslBridge`), eliminating separate
  per-call `code_trace` round-trips, and handle `"skipped from another client"`
  client-tool routing stalls across instructions and diagnostics.

## 0.25.1

- Limit native client-disconnection fallback to the current user turn. On the
  next foreground user turn, rediscover native tools and use one needed source
  lookup to restore native-first selection; keep backend MCP for unattended
  sessions without native dispatch.
- Apply the recovery contract to contributed instructions and generated
  instruction/MCP prompts in every style, with regression tests.
- Document Agents window client-disconnection diagnostics and distinguish
  them from normal cold-cache responses without restarting the shared daemon.

## 0.25.0

- Extend `cctx usage` with failure-cause, P95/P99 latency and offload tables,
  built-in fallback and per-workspace adoption reports, and optimization
  advisories; export the savings-model assumptions in `--json`; stop counting
  `web_search`/`web_fetch` as built-in source lookups.
- Add native stall diagnostics: `CCTX_TRACE` now carries `started_unix_ms` and
  `total_ms`, calls over 5 s log a warning and append a `CCTX_SLOW` transport
  hint, and `cctx usage` flags shared stalls and host-dispatch delay.
- Keep MCP `code_files`/`code_grep`/`code_context`/`code_read` responses under
  the 7.4 KiB inline budget with explicit `next_ranges` continuation. A plain
  `code_read` array is trimmed into the paged shape; an end-to-end `shared-mcp`
  test covers the real response shapes.
- State the expected load time in `CACHE_MISS` (per-root history, else recent
  median) instead of a fixed "~1-2s" so agents stop retrying early.
- Add a built-in-to-CCTX replacement map to the agent instructions and make the
  "model without CCTX" advisory report built-in lookup counts.
- Give busy roots a longer idle lease (2x at 50, 4x at 200 source lookups) to
  reduce avoidable `CACHE_MISS` reloads.
- Convert the auto-loaded agent instructions (`prompts/code-context.instructions.md`)
  and subagent prompt blocks (`scripts/configure_subagents.py --style formula`)
  to a BPE-aligned symbolic formula specification (9,127 B / 2,048 `o200k_base`
  tokens -> 2,994 B / 756 tokens, **-63.1% instruction tokens** with 100% 31/31
  edge-case recognition),cap them at 6,000 bytes in `scripts/verify_vsix.py`,
  and add `cctx prompt` (`--style formula|hybrid|prose`, `--target`,
  `--compress`) and `CCTX_PROMPT_STYLE` with bilingual Korean core-noun
  preservation (`[ko] :: ...`) and Unicode math / backtick BPE normalization.

## 0.24.0

- Immediately unwatch deleted or moved directories and ancestor trees
  (`prune_deleted_watches`, `unwatch_prefix`, `record_affected_directory`),
  drain watcher events during idle 1-second maintenance ticks, and evict deleted
  workspace/worktree roots immediately (`reason="root_missing"`) instead of
  waiting for the 15-minute idle lease.
- Add subagent tool configuration (`scripts/configure_subagents.py`, `cctx
  doctor` diagnostics, and subagent role/model attribution in
  `audit_agent_tool_adoption.py`) so Copilot `explore` and `research` subagents
  use Native and MCP Code Context tools automatically.
- Harden agent query ergonomics across Native and MCP surfaces: auto-repair
  malformed LLM regex patterns (unbalanced `(`, non-quantifier `{identifier}`
  braces), suggest `regex: true` on zero-match literal regexes, filter excluded
  entries from multi-path search arrays, suggest `did_you_mean` indexed files on
  missing reads, auto-narrow oversized compact `code_grep`/`code_context`
  responses, and disambiguate multi-root workspaces from relative target paths.

## 0.23.0

- Add live LLM model, token, cache-hit, and AIC/USD cost-savings telemetry
  across `cctx usage --hours 24` (`--json`), `audit_agent_tool_adoption.py
  --pretty`, and the VS Code status bar badge, `MarkdownString` hover tooltip,
  and `Code Context: Show Native Tool Status` dashboard.
- Fix WSL2 adaptive memory ceiling calculation so reclaimable Linux page cache
  (`Cached` + `Buffers`) is not double-subtracted against Windows host free
  memory, restoring up to 24 concurrent resident roots (`34.4 GiB` target).
- Complete 55 performance and stability optimizations across the Rust backend
  and VS Code extension: 6-slot inline `PostingList`, single-inventory cold
  load (`full_inventory_scans: 2 -> 1`, **29.115 ms** cold sync median — 3.39×
  faster than v0.14), cached directory `lstat` verification (**-79.2%**
  syscalls), single-pass SHA-256 chaining (**-50%** hash work), adaptive 200 µs
  `rg` subprocess polling (**13.1×** faster shallow scans), 9-bit $O(1)$
  DEFLATE Huffman lookup tables (**5.4×** faster `.pptx`/`.docx` extraction),
  cached `w:tbl` scanning (**8.5×** faster DOCX parsing), and $O(1)$ multi-chunk
  IPC buffer assembly.

## 0.22.0

- Eliminate false-positive watcher directory and full-workspace scans on `.py`
  profile markers, atomic saves, and excluded/temp files, and make
  `Resident::update` $O(|\text{changed}|)$ with incremental byte tracking so
  multi-million-line workspace reads execute in `< 0.1 ms`.
- Enforce an inline `< 8 KiB` budget across all Native tool formatters and
  compact `code_status` / `code_workspaces` payloads so responses stay inline
  without triggering `/tmp` host offloading.
- Add `#toc` / `#outline` virtual Table-of-Contents expansion, `#slide-N-M`
  multi-slide range anchors, inline DOCX `{#bookmark}` resolution, PPTX banner /
  footer / `sldNum` deduplication, series-compressed charts with IEEE 754 float
  cleanup and slide attribution, empty-column table pruning, and `w:delText` /
  duplicate-URL compaction.
- Sync WSL and Windows `.vscode/extensions` installations automatically in
  `scripts/install-vscode-extension.sh` and preserve `#anchor` fragments when
  `start: 1` is passed to `#cctxNativeRead`.

## 0.21.0

- Boost exact whole-word identifier matches (`has_whole_word_match`) above
  substring definition matches under `max_matches_per_file` and demote
  `@generated` / `DO NOT EDIT` files so symbol lookups resolve directly to
  human-authored definitions.
- Fix block clamping for the first, adjacent, and last methods inside `impl`,
  `trait`, `class`, `interface`, and `mod` blocks where no blank line separates
  the method from `{` or `}`, and fix index-0 definition header indentation
  tracking.
- Reclaim heap memory in `PostingList::remove_sorted` when lists shrink to
  `<= 5` entries, surface `[Cell Visual Output: image/png]` markers in Jupyter
  notebooks, bound 3+ range default read spans to prevent `>8 KiB` host
  offloading, and report estimated lookup tokens saved in the VS Code status bar
  tooltip.

## 0.20.0

- Expand `PostingList::Inline` capacity to 5 document IDs without increasing
  bucket size (`24 B`), lower multi-threaded trigram indexing threshold to
  `512 KiB`, add CJK UTF-8 syllable boundary selectivity scoring and 16-window
  long-literal sampling, bypass trigram lookups when `<=2` files are scoped,
  and partition parallel regex workers by byte size.
- Add 2-class small character set planning (`[gs]et_user_[iI]d`) and redundant
  subslice literal elimination in the HIR planner, negated glob filters
  (`*.rs, !*.test.rs`), case-insensitive extension matching across all source
  files, and multi-token subsequence filename fallback in `code_files`.
- Add Git merge conflict (`<<<<<<<` .. `>>>>>>>`), TOML section (`[table]`),
  and YAML OpenAPI/workflow block clamping, cap leading doc-comment rewind to
  12 lines, extend multi-line `impl`/`trait`/`class`/`interface` headers, and
  recognize CSS custom properties (`--var:`), `#[proc_macro*]`, Mermaid diagrams,
  and PlantUML blocks.
- Compact `#cctxNativeStatus` and `#cctxNativeWorkspaces` JSON payloads by >93%
  so responses stay inline (`<3 KB`) instead of being offloaded to `/tmp`,
  support `.ipynb#cell-N` / `#slide-N` / `#page-N` / `#record-N` anchors,
  GitHub/GitLab blob URLs, and `rg` `path:line: text` inputs, and preserve
  `.tsbuildinfo` and `.eslintcache` across extension builds.

## 0.19.0

- Accelerate coding, build, and test cycles by configuring `line-tables-only`
  debug symbols and `opt-level = 2` dependency compilation in `dev`/`test`
  profiles, cutting `cargo test` wall-clock runtime by >65%.
- Reduce daemon active background polling latency (`25 ms → 5 ms`) and watcher
  atomic-save read recovery delay (`25 ms → 8 ms`), and skip post-limit line
  galloping with a `memchr` newline jump fast path in resident and query scans.
- Extend signature-complete clamping to Rust `where` clauses, Python class
  methods immediately following `class` headers, C++ `template<...>` prefixes,
  C# `[Attribute]` annotations, and Go receiver method definitions (`func (s
  *Server)`), and enable incremental TypeScript builds (`incremental: true`).

## 0.18.0

- Prevent VS Code agent edit retries (`No match found` / `Multiple matches found`)
  and compile-fix rewrite loops by preserving multi-line function signatures,
  `where` clauses, return types, multi-line `#[derive(...)]` / `/** ... */`
  attributes, full `struct` / `interface` / `enum` / `type` field blocks, and
  TypeScript/Rust overload + implementation pairs in `code_context`.
- Track outer definition indentation during block clamping so inner `if`/`match`
  closing braces (`}`) never truncate enclosing functions, and recognize chained
  error-propagation closures (`})?`, `})?;`, `}).`).
- Guarantee surrounding context lines for short/non-unique call-site hits, add
  test-attribute (`#[test]`, `#[tokio::test]`, `#[rstest]`) definition boosting,
  and demote debug print macros (`println!`, `eprintln!`, `dbg!`) under
  `max_matches_per_file`.
- Eliminate tool-invocation retries by auto-normalizing `**.ext` and `.ext`
  shorthand globs, splitting comma-separated `paths`, converting Windows
  backslash relative paths, handling `start=0` / reversed `start > end` /
  `view_range: [start, -1]` in MCP and Native reads, reporting `valid_tail`
  bounds on `READ_RANGE_OUT_OF_BOUNDS`, and surfacing a `regex=true` hint when
  a literal search with regex syntax returns zero matches.

## 0.17.0

- Expand polyglot symbol-definition ranking and smart block-clamped context
  assembly across Go, Kotlin, Scala, Swift, Elixir, Ruby, PHP, Lua, Dart, SQL
  DDL, Protobuf/gRPC, GraphQL, Terraform/HCL/Bicep, Makefile/Justfile recipes,
  Dockerfile multi-stage builds, Python 3.12+ `type` aliases and multi-line
  decorators, React 19 / TanStack / Next.js / Svelte 5 Runes / Vue 3.5+ /
  Angular Signals, and comment-only hit demotion with post-clamping snippet
  coalescing.
- Accelerate UTF-8 / CJK / Korean + ASCII `ignore_case` regex queries and
  grouped small character classes (`\b(?:prefix)[-_]suffix\b`) in the trigram
  query planner, and add cross-convention (`camelCase` / `snake_case` /
  `kebab-case`) filename stem fallback in `code_files`.
- Add JSON Lines (`.jsonl`, `.ndjson`, `.jsonlines`) record extraction and
  structure inspection, XLIFF 2.0 `<unit>`/`<note>` support, WebVTT/SRT cue
  extraction and clamping, Jupyter `In[N]` execution counts and ANSI-cleaned
  error tracebacks, SVG structure inspection, inline code/HTML stripping in
  Markdown `#slug` anchors, and 2026 polyglot source/exclusion profiles.
- Support `around_line` / `center_line` + `radius` centered reads, Python
  traceback (`File "path", line N`) and Rust/Go compiler diagnostic (`-->
  path:line:col`) target parsing, `#subpath` package aliases, `cursor://` /
  `windsurf://` / `vscode-insiders://` URIs, MSYS paths, and consecutive line-run
  compression (`20-22`) in search output.

## 0.16.0

- Add native zero-dependency document indexing and smart section/slide/sheet/table
  clamping for Office (`docx`, `pptx`, `xlsx`), OpenDocument (`odt`, `odp`, `ods`),
  `epub`, `pdf`, `rtf`, Jupyter `ipynb`, `csv`/`tsv`, LaTeX/BibTeX, Typst, AsciiDoc,
  Org, XLIFF/gettext, and WebVTT/SRT.
- Enhance Markdown (`.md`, `.mdx`, `.markdown`, `.qmd`, `.rmd`, and agent
  instructions) with fence-aware ATX/Setext heading detection, GFM alerts,
  `:::` admonitions, `<details>`, pipeless GFM tables, `$$` math blocks, and
  `path.md#heading-slug` section range reads.
- Improve React, Vite, TypeScript/TSX, Vue/Svelte/Astro SFC, and CSS/Tailwind
  definition ranking, barrel re-export filtering, dynamic-route globs
  (`[id].tsx`), compound extension fast paths (`*.test.tsx`, `*.module.css`),
  Vite `?raw`/`?react` query stripping, `@/` alias resolution, and lockfile/bundle
  exclusions.
- Expand VS Code GitHub Copilot compatibility with built-in tool parameter
  aliases (`includePattern`, `symbol`, `isRegexp`, `caseInsensitive`,
  `maxResults`, `contextLines`, `filePath`, `view_range`), Markdown link and
  backtick path unwrapping, and automatic `>500`-line read chunking.

## 0.15.0

- Eliminate multi-project VS Code startup contention with registry seeding,
  coalesced `code_workspaces` discovery, jittered service auto-start, lock-free
  registered-root preparation, and small-root background queue prioritization.
- Prioritize native VS Code Language Model Tools (`#cctxNative*`) first and add
  smart multi-root workspace resolution across visible editors, tab groups,
  relative path existence checks, folder basenames, file URIs, and `:line` /
  `#Lline` suffixes.
- Accelerate resident trigram lookups (`15.01×` mixed warm speedup, `0.260 ms`
  mean across 8,960 pairs; `20.50×` literal search) with inline `PostingList`
  storage, SplitMix64 trigram maps, galloping line search, zero-copy `CCTXJ1` /
  `CCTXB1` IPC framing, and adaptive definition vs. call-site snippet clamping.
- Update the four-panel benchmark chart to compare `CCTX off`, `v0.14`, and
  current warm IPC side-by-side with explicit latency, speedup, and token labels.

## 0.14.0

- Support native Windows (`cctx.exe`) workspaces without WSL alongside existing
  Linux, WSL, and SSH hosts, with hidden background daemon auto-start and
  drive-letter path normalization.
- Attach contributed chat instructions automatically (`applyTo: "**"`) and
  guide deferred `tool_search` discovery so new installations use Code Context
  out of the box.
- Accelerate `ignore_case` and scoped searches via ASCII case-folded trigram
  planning and selectivity-first filtering; recover transparently from stale
  index snapshots on search/context calls.
- Add `max_matches_per_file` with symbol-definition priority, smart block/Python
  boundary snippet clamping, and compact trace/notice output to cut payload and
  schema tokens.
- Publish the four-panel retrieval speedup, Rust/Python token savings, and
  end-to-end coding agent acceleration benchmark figure.

## 0.13.2

- Render native Output traces, status, self-test, and registration as readable
  text; use real log levels and preserve machine-facing JSON contracts.
- Emit one call summary with timings and correlation; record root-resolution
  failures and escape control characters in error text.
- Treat Windows Agents' explicit-root requirement as callable readiness instead
  of an unregistered error, and skip unrelated session-folder readiness probes.
- Avoid registry enumeration for explicit WSL roots while retaining backend
  authorization; let Windows status choose a registered Linux root.

## 0.13.1

- Qualify the Marketplace display name as **Code Context by dotnetpower** after
  the unqualified display name was rejected as already taken.
- Preserve the publisher, extension ID, settings, and seven native references.
- Verify that both the packaged manifest and VSIX metadata use the corrected
  display name.

## 0.13.0

- Adopt `dotnetpower.code-context-native` for the first Marketplace publication.
- Resolve walkthroughs, settings, and self-test metadata from the running
  extension identity instead of a previous publisher.
- Keep all seven native tool references, settings, and the shared backend stable.
- Refuse duplicate registration while the previous publisher's extension is
  present; document profile/host migration and safe-boundary reloads.
- Validate both VSIX and package identities and allow the public publisher brand
  in docs while continuing to reject machine-specific paths.

## 0.12.3

- Put Resources links and the complete benchmark table before installation in
  extension details, retaining results even when images cannot load.
- Explain the Marketplace-only native Resources sidebar and manual VSIX updates.
- Validate the packaged README, image, and gallery-ready resource properties.

## 0.12.2

- Simplify installation READMEs and move detailed operation notes to dedicated guides.
- Replace local machine/project examples with portable placeholders.
- Publish a correctness-checked AKS Store Demo on/off retrieval benchmark, with
  session-bootstrap confidence intervals, cold behavior, and slower known reads.
- Include the benchmark figure in the VSIX; preserve the existing extension identity.

## 0.12.1

- Supply required, packaged walkthrough media for all four setup steps.
- Verify those media entries against the built VSIX as well as the manifest.
- Keep the first installable GitHub distribution on this corrected patch version.

## 0.12.0

### Added
- Original icon and gallery artwork, a concise extension landing page, and
  dedicated setup and publishing guides.
- A four-step Getting Started walkthrough and connection/help commands.
- Reproducible VSIX validation and GitHub release packaging.

### Changed
- Marketplace-facing display name is **Code Context**; the extension ID and all
  seven native tool IDs/references remain unchanged.
- Stable tool contributions replace the proposed tool-set API. Installation no
  longer edits VS Code runtime arguments or needs proposed API approval.
- Tool grouping is now managed by VS Code. Review tool enablement after upgrade;
  per-tool IDs/references remain unchanged.
- Normal Linux/WSL/SSH and Windows Agent Sessions connection paths are preserved.

## 0.11.1

- Negotiated outcome and Windows memory trace fields for older daemon compatibility.

## 0.11.0

- Event-driven native usage status and separate success/readiness/refusal/error outcomes.
- Adaptive host/cgroup-aware backend memory, including Windows physical headroom.
- More precise source refusal diagnostics and directory-only build-output exclusions.

## 0.10.5

- Windows Agents activation and native WSL IPC support.
- Explicit trust enforcement without relying on a mismatched UI context key.
