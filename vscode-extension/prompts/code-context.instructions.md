---
description: "Use when: finding files by name or path, searching code, or reading saved source, documentation, and configuration in a registered workspace"
applyTo: "**"
---

# Code Context (cctx) — Formula Spec
CCTX is an optional accelerator (nonblocking root cache), not a prerequisite.
Priority: Agent Host/SDK/Subagent/CLI (code-context-code_* | mcp__code-context__* present) => prefer backend MCP code-context-code_{context,read,grep,files} FIRST (direct server IPC; avoids UI client-relay disconnects) ; Editor Chat / VSIX-only (!code-context-code_*) => #cctxNativeContext / #cctxNative*{Read,Grep,Files} (code-context-native_*) FIRST > code_* > CLI(cctx --backend shared --root R) > Host(rg,view,read_file,glob). Never start with Host search/read for saved source.
Dispatch:
- symbol|literal+context => code-context-code_context / #cctxNativeContext / code_context (root=R, radius:0..100, limit:1..64; 1 call > grep+read & !(1..150->151..300 slicing))
- filename|glob(!content) => code-context-code_files / #cctxNativeFiles / code_files (glob=G) ; content(a|b=>regex=true) => code-context-code_grep / #cctxNativeGrep / code_grep (max_matches_per_file:1..64)
- range|anchor(path:line,path#Lstart-Lend,doc.md#slug,@/) => code-context-code_read / #cctxNativeRead / code_read (<=150L,<8KiB; >150=>page=true->next_ranges->complete=true)
- Swap: view|read_file=>code-context-code_read|#cctxNativeRead ; rg|grep=>code-context-code_grep|#cctxNativeGrep ; glob|find=>code-context-code_files|#cctxNativeFiles ; 한글 요청 => 동일 디스패치 & 한글 답변
- Probe & Continuity: API/STAC/tiles | JSON/JSONL/GeoJSON//tmp offload | SQLite/Parquet/CSV | PNG/WebP/SVG | large logs/CLI batch | compaction journal => pctx {http|json|table|image|log|batch|journal} (cctx probe / cctx journal) & !(python3 -c | raw curl/jq)
Guards:
- known(R) => root=R ; before(edit) => Context(radius>=8)|Read(around_line) ; multi-edit => 1 Read(ranges=[...]) (!parallel view)
- deferred (even if rg/view/glob visible on turn 0) => tool_search_tool FIRST: AgentHost/SDK/MCP "code-context-code_context code-context-code_read code-context-code_grep code-context-code_files" | EditorChat "cctxNativeContext cctxNativeRead cctxNativeGrep cctxNativeFiles" (regex: "code-context|cctxNative")
- subagent(explore,general-purpose,code-review,rubber-duck,research,security-review,task) => pass root=R & require code-context-code_* | #cctxNative* (via tool_search_tool if hidden) first
Transitions:
- CACHE_MISS != (0 matches|missing) -> 1 Host read -> resume CCTX next turn & !(poll|repeat|switch transport|code_sync) ; CACHE_LOAD_FAILED => Host ; STALE_INDEX => Read|Host ; TARGET_NOT_INDEXED != absent
- READ_RANGE_OUT_OF_BOUNDS(N) => clamp(1..N) ; code_sync <=> exhaustive absence claims only ; TTL>=15m|LRU => evict. Do not enumerate or prepare sibling worktrees speculatively (authorize-worktrees is operator-only).
- .github,.vscode,ignored,secrets,/tmp => Host policy-aware tools only. Never use another transport to bypass an exclusion, trust decision or access denial.
- Error("No client with the advertised tool definition"|"No matching client connected"|disconnected|"skipped from another client"|"not available on this client") => code-context-code_* MCP | cctx --backend shared --root /actual/root
- Session(unattended|background|autopilot) & !native_dispatch => prefer backend MCP (code-context-code_* | code_*). Do not duplicate a still-pending lookup or poll Status; safe boundary => cctx --root /project doctor.
- Recovery: once a chat hits a client-relay dispatch error ("advertised tool definition", "No matching client connected", or "skipped from another client"), stay on code-context-code_* MCP for the rest of this chat (including across autopilot continuations and context compactions) — do not retry cctxNative* in the same session. Never duplicate a pending call or poll Status; background/unattended sessions without native dispatch stay on MCP.
