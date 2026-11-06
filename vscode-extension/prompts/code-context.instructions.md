---
description: "Use when: finding files by name or path, searching code, or reading saved source, documentation, and configuration in a registered workspace"
applyTo: "**"
---

# Code Context (cctx) — Formula Spec
CCTX is an optional accelerator (nonblocking root cache), not a prerequisite.
Priority: `#cctxNativeContext` / `#cctxNative*{Read,Grep,Files}` (`code-context-native_*`) > MCP (`code-context-code_*` / `code_*`) > CLI (`cctx --backend shared --root R`) > Host (`rg`, `view`, `read_file`, `grep_search`, `glob`). Never start with Host search/read for saved source.
Dispatch:
- `symbol | literal + context` => `#cctxNativeContext` / `code_context` (`root=R`, `radius: 0..100`, `limit: 1..64`; 1 call > grep + read)
- `filename | glob (!content)` => `#cctxNativeFiles` / `code_files` (`root=R`, `glob=G`)
- `content (a|b => regex=true)` => `#cctxNativeGrep` / `code_grep` (`root=R`, `max_matches_per_file: 1..64`)
- `range | anchor (path:line:col, path#Lstart-Lend, doc.md#slug, @/)` => `#cctxNativeRead` / `code_read` (`root=R`, `lines <= 150`, `< 8KiB`; `> 150 => page=true -> next_ranges -> complete=true`)
- `Swap`: `view | read_file => #cctxNativeRead` ; `rg | grep => #cctxNativeGrep` ; `glob | find => #cctxNativeFiles`
- `Bilingual`: Korean/multilingual requests (`함수/심볼+주변코드 => #cctxNativeContext`, `파일명/경로 => #cctxNativeFiles`, `문자열/정규식 => #cctxNativeGrep`, `줄범위 => #cctxNativeRead`) follow identical dispatch & preserve reply language (`한글 요청 => 한글 답변`)
Guards:
- `known(R) => pass explicit absolute root=R` ; `before(edit) => #cctxNativeContext(radius >= 8) | #cctxNativeRead(around_line, radius)`
- `deferred => tool_search("cctxNativeContext cctxNativeRead cctxNativeGrep cctxNativeFiles code-context-code_context code-context-code_read code-context-code_grep code-context-code_files")`
- `subagent(explore, general-purpose, code-review, rubber-duck, security-review) => pass root=R & require #cctxNative* | code-context-code_* first`
Transitions & Fallback:
- `CACHE_MISS => !(0 matches | missing file) -> Host read (1 turn only) -> resume CCTX next turn & !(poll | repeat | switch transport | code_sync)`
- `CACHE_LOAD_FAILED => Host read (cooldown)` ; `STALE_INDEX => #cctxNativeRead | Host live read` ; `TARGET_NOT_INDEXED != absent`
- `READ_RANGE_OUT_OF_BOUNDS(N) => clamp(1..N) & do not repeat` ; `code_sync <=> exhaustive absence claims only (never per edit)`
- `target in {.github, .vscode, ignored, secrets, /tmp} => Host policy-aware tools only & do not retry CCTX`. Never use another transport to bypass an exclusion, trust decision or access denial.
- `TTL: idle >= 15m | LRU => evict`. Do not enumerate or prepare sibling worktrees speculatively (`cctx --root /parent authorize-worktrees` is operator-only).
- `Error("No matching client connected" | disconnected) => code-context-code_* MCP | cctx --backend shared --root /actual/root`
- `Session(unattended | background) & !native_dispatch => prefer backend MCP (code_*)`. Do not duplicate a still-pending lookup or poll Status; safe boundary => `cctx --root /project doctor`.
- Recovery: dispatch-error MCP fallback lasts only for the current user turn. On the next foreground user turn, rediscover native tools and try one needed source lookup: success => Native-first; dispatch error => MCP for that turn. Never duplicate a pending call or poll Status; background/unattended sessions without native dispatch stay on MCP.
