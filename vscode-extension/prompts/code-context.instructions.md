---
description: "Use when: finding files by name or path, searching code, or reading saved source, documentation, and configuration in a registered workspace"
applyTo: "**"
---

# Code Context (cctx) — Formula Spec
CCTX is an optional accelerator (nonblocking root cache), not a prerequisite.
Priority: AgentHost / Copilot SDK (`Copilot SDK in VS Code`, `ahp-session`, `get_current_session`/`list_sessions`, autopilot/bg, subagents, CLI) with MCP callable => prefer backend MCP (`code-context-code_*` | `code_*`) FIRST (avoids Reload Window `'Running on another client... Skip?'` stalls & client-disconnect failures) > `#cctxNative*` > CLI(`cctx --backend shared --root R`) > Host(rg,view,read_file,glob). Foreground Editor Chat without backend MCP => `#cctxNativeContext` / `#cctxNative*{Read,Grep,Files}` (`code-context-native_*`) FIRST; never start with Host search/read for saved source.
Dispatch:
- symbol|literal+context => code_context / #cctxNativeContext (root=R, radius:0..100, limit:1..64; 1 call > grep+read & !(1..150->151..300 slicing))
- filename|glob(!content) => code_files / #cctxNativeFiles (glob=G) ; content(a|b=>regex=true) => code_grep / #cctxNativeGrep (max_matches_per_file:1..64)
- range|anchor(path:line,path#Lstart-Lend,doc.md#slug,@/) => code_read / #cctxNativeRead (<=150L,<8KiB; >150=>page=true->next_ranges->complete=true)
- Swap: view|read_file=>code_read/#cctxNativeRead ; rg|grep=>code_grep/#cctxNativeGrep ; glob|find=>code_files/#cctxNativeFiles ; 한글 요청 => 동일 디스패치 & 한글 답변
- Shell: bash|run_in_terminal grep|rg|sed -n|cat|head|tail|find|ls on saved source => same Swap (bash is NOT exempt; 1 code_context > grep+sed -n chain) ; bash only for build|test|git|non-source
- Probe & Continuity: API/STAC/tiles | JSON/JSONL/GeoJSON//tmp offload | SQLite/Parquet/CSV | PNG/WebP/SVG | large logs/CLI batch | compaction journal => pctx {http|json|table|image|log|batch|journal} (cctx probe / cctx journal) & !(python3 -c | raw curl/jq)
Guards:
- known(R) => root=R ; before(edit) => Context(radius>=8)|Read(around_line) ; multi-edit => 1 Read(ranges=[...]) (!parallel view)
- deferred (ONLY when CCTX tools are absent/hidden from active declarations, even if rg/view/glob visible on turn 0; skip if already declared, and invoke whichever of tool_search_tool / tool_search is declared) => <=2 names/call in 2 parallel calls (4 names drop code_read/code_files: result cap 5 incl. GitHub-* noise): AgentHost/SDK/subagent "code-context-code_context code-context-code_grep" + "code-context-code_read code-context-code_files" | Editor Chat "cctxNativeContext cctxNativeGrep" + "cctxNativeRead cctxNativeFiles" ; all 4 loaded before 1st lookup, else re-search the missing name
- subagent(explore,general-purpose,code-review,rubber-duck,research,security-review,task) => pass root=R & require code-context-code_* | #cctxNative* (via tool_search_tool if hidden) first
Transitions:
- CACHE_MISS != (0 matches|missing) -> 1 Host read -> resume CCTX next turn & !(poll|repeat|switch transport|code_sync) ; CACHE_LOAD_FAILED => Host ; STALE_INDEX => Read|Host ; TARGET_NOT_INDEXED != absent
- READ_RANGE_OUT_OF_BOUNDS(N) => clamp(1..N) ; code_sync <=> exhaustive absence claims only ; TTL>=15m|LRU => evict. Do not enumerate or prepare sibling worktrees speculatively (authorize-worktrees is operator-only).
- .github,.vscode,ignored,secrets,/tmp => Host policy-aware tools only. Never use another transport to bypass an exclusion, trust decision or access denial.
- Error("No client with the advertised tool definition"|"No matching client connected"|disconnected|"skipped from another client"|"not available on this client") => code-context-code_* MCP | cctx --backend shared --root /actual/root
- Session(AgentHost|CopilotSDK|unattended|background) => prefer backend MCP (code-context-code_* | code_*). Do not duplicate a still-pending lookup or poll Status; safe boundary => cctx --root /project doctor.
- Recovery: AgentHost/CopilotSDK/background/unattended sessions stay on backend MCP so Reload Window never stalls on another client or fails mid-turn. In Editor Chat without backend MCP, dispatch-error MCP fallback lasts only for the current user turn (including unattended autopilot continuations before the next user message); on the next foreground user turn, rediscover native tools and try one needed source lookup: success => Native-first; dispatch error => MCP for that turn. Never duplicate a pending call or poll Status; background/unattended sessions without native dispatch stay on MCP.
