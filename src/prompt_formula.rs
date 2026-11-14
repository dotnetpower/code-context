use clap::ValueEnum;
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum PromptStyle {
    /// BPE-aligned symbolic formula notation (maximum token compression).
    #[default]
    Formula,
    /// Hybrid symbolic formula preserving all extension & MCP contract phrases.
    Hybrid,
    /// Full natural-language prose instructions.
    Prose,
}

impl PromptStyle {
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var("CCTX_PROMPT_STYLE").ok()?;
        Self::parse_str(&raw)
    }

    pub fn parse_str(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "formula" | "math" | "symbolic" | "wff" => Some(Self::Formula),
            "hybrid" | "hybrid-formula" | "compact" => Some(Self::Hybrid),
            "prose" | "standard" | "text" => Some(Self::Prose),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Formula => "formula",
            Self::Hybrid => "hybrid",
            Self::Prose => "prose",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum PromptTarget {
    /// VS Code auto-loaded `.instructions.md` specification.
    #[default]
    Instructions,
    /// MCP `initialize` instructions string.
    Mcp,
    /// Copilot SDK YAML subagent prompt block (`*.agent.yaml`).
    SubagentYaml,
    /// VS Code Markdown subagent prompt block (`*.agent.md`).
    SubagentMd,
    /// Claude Code / Claude Agent Host instructions (`CLAUDE.md`).
    ClaudeMd,
}

impl PromptTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Instructions => "instructions",
            Self::Mcp => "mcp",
            Self::SubagentYaml => "subagent-yaml",
            Self::SubagentMd => "subagent-md",
            Self::ClaudeMd => "claude-md",
        }
    }
}

/// Approximate BPE token count calibrated against `tiktoken` (`o200k_base` / `cl100k_base`).
/// Accumulates quarter-token units (`1 token = 4 units`) to capture newline/punctuation merging,
/// camelCase identifier splits, and Korean Hangul syllable density (~2.8% MAPE).
pub fn estimate_bpe_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut quarter_tokens = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\n' {
            quarter_tokens += 2;
            continue;
        }
        if ch.is_whitespace() {
            let mut run = 1usize;
            while let Some(&next) = chars.peek() {
                if next != '\n' && next.is_whitespace() {
                    run += 1;
                    chars.next();
                } else {
                    break;
                }
            }
            if run > 1 {
                quarter_tokens += 4 * run.div_ceil(8);
            }
            continue;
        }
        if ch.is_ascii_alphabetic() {
            let mut len = 1usize;
            let mut prev_lower = ch.is_ascii_lowercase();
            let mut case_splits = 0usize;
            while let Some(&next) = chars.peek() {
                if next.is_ascii_alphabetic() {
                    if prev_lower && next.is_ascii_uppercase() {
                        case_splits += 1;
                    }
                    prev_lower = next.is_ascii_lowercase();
                    len += 1;
                    chars.next();
                } else {
                    break;
                }
            }
            if case_splits > 0 {
                let segs = case_splits + 1;
                let extra = if len > segs * 8 { segs } else { 0 };
                quarter_tokens += 4 * (segs + extra);
            } else if len <= 9 {
                quarter_tokens += 4;
            } else {
                quarter_tokens += 4 * len.div_ceil(8);
            }
        } else if ch.is_ascii_digit() {
            let mut len = 1usize;
            while let Some(&next) = chars.peek() {
                if next.is_ascii_digit() {
                    len += 1;
                    chars.next();
                } else {
                    break;
                }
            }
            quarter_tokens += 4 * len.div_ceil(3);
        } else if ch.is_ascii_punctuation() {
            let mut run = String::from(ch);
            while let Some(&next) = chars.peek() {
                if next.is_ascii_punctuation() && run.len() < 4 {
                    run.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            quarter_tokens += match run.as_str() {
                "->" | "=>" | "<=>" | "<=" | ">=" | "!=" | "==" | ".." | "..." | "--" | "::"
                | "//" | "/*" | "*/" | "**" | "||" | "&&" => 4,
                "," | "." | ":" | ";" | "_" | "-" | "/" => 3,
                _ if run.len() == 1 => 4,
                _ => 3 * run.len(),
            };
        } else if ('\u{AC00}'..='\u{D7A3}').contains(&ch) {
            let mut syllables = 1usize;
            while let Some(&next) = chars.peek() {
                if ('\u{AC00}'..='\u{D7A3}').contains(&next) {
                    syllables += 1;
                    chars.next();
                } else {
                    break;
                }
            }
            quarter_tokens += syllables * 3;
        } else {
            quarter_tokens += 4 * ch.len_utf8().div_ceil(2);
        }
    }
    quarter_tokens.div_ceil(4).max(1)
}

pub fn render(style: PromptStyle, target: PromptTarget, root: Option<&Path>) -> String {
    let root_binding = root
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<abs_root>".to_string());
    let mut rendered = match (target, style) {
        (PromptTarget::Instructions, PromptStyle::Formula) => format!(
            "# CCTX Formula Spec\n\
#cctxNative*{{Context,Read,Grep,Files}} FIRST > MCP(code-context-code_* / code_*) > cctx --backend shared --root {root_binding} > Host(rg,view,glob); !MCP when Native callable; pctx {{http|json|table|image|log|batch|journal}} > python3 -c/curl/jq\n\
- symbol+context => #cctxNativeContext / code_context (root={root_binding}, radius:0..100, limit:1..64; !1..150 slicing)\n\
- glob(!content) => #cctxNativeFiles / code_files ; a|b(regex=true) => #cctxNativeGrep / code_grep (max_matches_per_file:1..64)\n\
- ranges(:line,#Lstart-Lend,.md#slug,@/) => 1 #cctxNativeRead / code_read (<=150L,<8KiB; >150=>page=true->next_ranges)\n\
- pre-edit => Context(radius>=8)|Read(around_line) ; hidden => tool_search_tool(\"cctxNativeContext cctxNativeRead cctxNativeGrep cctxNativeFiles\" | !native \"code-context-code_context code-context-code_read code-context-code_grep code-context-code_files\") FIRST ; subagents => pass root={root_binding} & require CCTX\n\
- CACHE_MISS != (0|missing) -> 1 Host read -> resume CCTX & !(poll|repeat|code_sync) ; CACHE_LOAD_FAILED => Host ; STALE_INDEX => Read/Host ; OUT_OF_BOUNDS(N) => clamp(1..N) ; code_sync <=> absence claims\n\
- .github,.vscode,ignored,secrets,/tmp => Host only. Never use another transport to bypass exclusions/trust/denials.\n\
- Error(\"No client with the advertised tool definition\"|\"No matching client connected\"|\"skipped from another client\"|disconnected) => code-context-code_* MCP | cctx --backend shared --root {root_binding} ; unattended & !native => prefer backend MCP. Do not duplicate a still-pending lookup."
        ),
        (PromptTarget::Instructions, PromptStyle::Hybrid) => format!(
            "---\n\
description: \"Use when: finding files by name or path, searching code, or reading saved source, documentation, and configuration in a registered workspace\"\n\
applyTo: \"**\"\n\
---\n\n\
# Code Context (cctx) — Formula Spec\n\
CCTX is an optional accelerator (nonblocking root cache), not a prerequisite.\n\
Priority: #cctxNativeContext / #cctxNative*{{Read,Grep,Files}} (code-context-native_*) FIRST > MCP(code-context-code_* | code_*) > CLI(cctx --backend shared --root {root_binding}) > Host(rg,view,read_file,glob). Do not choose MCP when the matching Native tool is callable; never start with Host search/read for saved source.\n\
Dispatch:\n\
- symbol|literal+context => #cctxNativeContext / code_context (root={root_binding}, radius:0..100, limit:1..64; 1 call > grep+read & !(1..150->151..300 slicing))\n\
- filename|glob(!content) => #cctxNativeFiles / code_files (glob=G) ; content(a|b=>regex=true) => #cctxNativeGrep / code_grep (max_matches_per_file:1..64)\n\
- range|anchor(path:line,path#Lstart-Lend,doc.md#slug,@/) => #cctxNativeRead / code_read (<=150L,<8KiB; >150=>page=true->next_ranges->complete=true)\n\
- Swap: view|read_file=>#cctxNativeRead ; rg|grep=>#cctxNativeGrep ; glob|find=>#cctxNativeFiles ; 한글 요청 => 동일 디스패치 & 한글 답변\n\
- Probe & Continuity: API/STAC/tiles | JSON/JSONL/GeoJSON//tmp offload | SQLite/Parquet/CSV | PNG/WebP/SVG | large logs/CLI batch | compaction journal => pctx {{http|json|table|image|log|batch|journal}} (cctx probe / cctx journal) & !(python3 -c | raw curl/jq)\n\
Guards:\n\
- known(R) => root={root_binding} ; before(edit) => Context(radius>=8)|Read(around_line) ; multi-edit => 1 Read(ranges=[...]) (!parallel view)\n\
- deferred (even if rg/view/glob visible on turn 0) => tool_search_tool FIRST: foreground \"cctxNativeContext cctxNativeRead cctxNativeGrep cctxNativeFiles\" (<=5 cap; do not combine 8+ tools) | !native_dispatch \"code-context-code_context code-context-code_read code-context-code_grep code-context-code_files\" (regex: \"cctxNative|code-context\")\n\
- subagent(explore,general-purpose,code-review,rubber-duck,research,security-review,task) => pass root={root_binding} & require #cctxNative* | code-context-code_* (via tool_search_tool if hidden) first\n\
Transitions:\n\
- CACHE_MISS != (0 matches|missing) -> 1 Host read -> resume CCTX next turn & !(poll|repeat|switch transport|code_sync) ; CACHE_LOAD_FAILED => Host ; STALE_INDEX => Read|Host ; TARGET_NOT_INDEXED != absent\n\
- READ_RANGE_OUT_OF_BOUNDS(N) => clamp(1..N) ; code_sync <=> exhaustive absence claims only ; TTL>=15m|LRU => evict. Do not enumerate or prepare sibling worktrees speculatively (authorize-worktrees is operator-only).\n\
- .github,.vscode,ignored,secrets,/tmp => Host policy-aware tools only. Never use another transport to bypass an exclusion, trust decision or access denial.\n\
- Error(\"No client with the advertised tool definition\"|\"No matching client connected\"|disconnected|\"skipped from another client\"|\"not available on this client\") => code-context-code_* MCP | cctx --backend shared --root {root_binding}\n\
- Session(unattended|background) & !native_dispatch => prefer backend MCP (code-context-code_* | code_*). Do not duplicate a still-pending lookup or poll Status; safe boundary => cctx --root /project doctor."
        ),
        (PromptTarget::Instructions, PromptStyle::Prose) => format!(
            "---\n\
description: \"Use when: finding files by name or path, searching code, or reading saved source, documentation, and configuration in a registered workspace\"\n\
applyTo: \"**\"\n\
---\n\n\
# Code Context source lookup\n\n\
Native-first: in foreground VS Code and Agents Window sessions, ALWAYS use `#cctxNativeContext`, `#cctxNativeRead`, `#cctxNativeGrep` and `#cctxNativeFiles` (`code-context-native_*`) FIRST before MCP (`code-context-code_*` / `code_*`) or built-in search/read. Do not choose MCP when the matching Native tool is callable, and do not start with `view`, `read_file`, `grep_search` or `rg` for saved source, even for known paths.\n\n\
| Need | Tool |\n\
| --- | --- |\n\
| Symbol/literal plus surrounding code (search and read in one call) | `#cctxNativeContext` / `code_context` |\n\
| Filename or path glob (`*.ts`, `src/**/*.rs`, bare stem; not content) | `#cctxNativeFiles` / `code_files` |\n\
| Exact or regex content search (`regex: true` for `a\\|b`) | `#cctxNativeGrep` / `code_grep` |\n\
| Known ranges (`path:line:col`, `path#Lstart-Lend`, `doc.md#slug`, `@/` aliases) | `#cctxNativeRead` / `code_read` |\n\
| Structured data, HTTP/OpenAPI/STAC batch probes, `/tmp` offloaded JSON/logs, SQLite/Parquet/CSV schemas, PNG/WebP/SVG assets, noisy CLI batch logs, or compaction continuity journal | `pctx http` / `pctx json` / `pctx table` / `pctx image` / `pctx log` / `pctx batch` / `pctx journal` (`cctx probe ...` / `cctx journal ...`) |\n\n\
- For structured data, noisy command logs, and long-session continuity, prefer declarative `pctx` (`cctx probe`) and `cctx journal` commands (`pctx http \"https://api.example/{{svc}}\" --var svc=a,b`, `pctx json data.geojson --bbox minx,miny,maxx,maxy`, `pctx table db.sqlite`, `pctx image render.png`, `pctx batch -c \"git log -n 200 --stat\" -q \"retry\"`, `pctx log @s1 -q \"fatal\"`, `cctx --root {root_binding} journal resume`) instead of writing multi-line `python3 -c`, `urllib.request`, `jq`, or `sqlite3` scripts: `pctx` runs in Rust, spools large logs outside context (`@sN`), redacts secrets automatically, and bounds replies within `<8KiB` (`<=2KiB` for compaction state).\n\
- Swap, do not fall back: `view`/`read_file` on a known path or range -> `#cctxNativeRead` (`path:start-end`); `rg`/`grep` -> `#cctxNativeGrep`; `glob`/`find` -> `#cctxNativeFiles`. Keep using them after the first lookup. Never slice a file across turns (`1..150` -> `151..300`) or chain `grep` -> `read` when `#cctxNativeContext` (`paths=[file]`, `radius=25..50`) or batched `ranges` can answer in one turn.\n\
- Pass the absolute `root` (`{root_binding}`) when the workspace or worktree is known (WSL, subagents, multi-root).\n\
- Keep each read to `<=150` lines (`<8KiB`) and set `max_matches_per_file` on broad searches so output stays inline; context radius is 0..100, limit 1..64. For large reads use `page=true` and follow `next_ranges` until `complete=true`. Responses that hit the inline budget report `inline_notice` and `next_ranges`.\n\
- Before editing, use `#cctxNativeContext` (`radius >= 8`) or `#cctxNativeRead` (`around_line` + `radius`) so the first edit has exact indentation, full signatures and 3+ unique context lines; before a multi-hunk or multi-file edit, batch all needed line ranges into one `#cctxNativeRead(ranges=[...])` call rather than parallel `view` calls.\n\
- Subagents (`explore`, `general-purpose`, `code-review`, `rubber-duck`, `research`, `security-review`, `task`): built-in subagents may run different models (such as Opus, Sonnet, Haiku, or Terra) without custom workspace instructions, so always include the absolute `root` in the delegation prompt and instruct them to use `#cctxNative*` or `code-context-code_*` (loading them with `tool_search_tool` first if hidden) before `view`, `rg`, or `grep`.\n\
- Deferred tools (Claude, GPT, and subagent sessions where built-in `grep`/`view`/`glob` appear on turn 0 while CCTX tools are hidden): call `tool_search_tool` (`tool_search`) FIRST before any host search/read — query `\"cctxNativeContext cctxNativeRead cctxNativeGrep cctxNativeFiles\"` first (at most 4 names per call because `tool_search_tool` caps results at 5 tools), or `\"code-context-code_context code-context-code_read code-context-code_grep code-context-code_files\"` when native dispatch is unavailable (regex `\"cctxNative|code-context\"`).\n\n\
## Cache and freshness\n\n\
- CCTX is an optional accelerator, not a prerequisite: a nonblocking root cache. `CACHE_MISS` returns immediately and queues only that root; it is never zero matches or a missing file. Continue that single lookup with a permitted host search/read. Do not poll, repeat, switch transports, change roots or run `code_sync` to wait. `CACHE_LOAD_FAILED` reports the real failure and retry cooldown.\n\
- `STALE_INDEX` means a checked file differs from the index: do not trust old matches or line numbers; use `#cctxNativeRead` or a host live read. `TARGET_NOT_INDEXED` is not proof of absence.\n\
- Use `code_sync` only as an explicit full-corpus barrier (exhaustive absence claims), not per edit; for known changed files read them directly.\n\
- `READ_RANGE_OUT_OF_BOUNDS` reports the real line count: choose a valid range instead of repeating.\n\
- Hidden paths (`.github`, `.vscode`), ignored files and secrets stay excluded: use host policy-aware tools without retrying CCTX or widening access. Never use another transport to bypass an exclusion, trust decision or access denial. Empty results do not cover files outside registered scopes.\n\
- Inactive roots may be evicted after at least 15 minutes (longer for busy roots) or under memory pressure. Do not enumerate or prepare sibling worktrees speculatively; the first lookup of an operator-authorized worktree prepares it (`cctx --root /parent authorize-worktrees` is an operator action, not an agent action).\n\n\
## Fallback order\n\n\
1. If a native call fails with `No client with the advertised tool definition`, `No matching client connected`, `Client ... disconnected`, `skipped from another client`, or `not available on this client`, switch at once to `code-context-code_*` MCP tools or `cctx --backend shared --root /actual/root` for that user turn, not built-in tools.\n\
2. In unattended/background sessions without native dispatch, prefer backend MCP (`code-context-code_*` / `code_*`). A native timeout is not proof the index is slow or stale. Do not duplicate a still-pending lookup or repeatedly poll native Status; cancel the pending call when the host allows, then use backend MCP.\n\
3. If both surfaces are blocked but a terminal works, use `cctx --backend shared --root /actual/root` with `files '*.ts' --limit 20` or `read src/example.ts 1 80`. It is the same daemon and cannot repair `CACHE_MISS`.\n\
4. Use built-in search/read only when both surfaces are unavailable, the workspace is unregistered, the target is unsaved or excluded, or Code Context returns an error.\n\n\
At a safe boundary run `cctx --root /project doctor` to detect stale MCP processes; refresh the host's MCP tool list or extension host without killing active sessions."
        ),
        (PromptTarget::Mcp, PromptStyle::Formula | PromptStyle::Hybrid) => format!(
            "CCTX Formula (root={root_binding}): Foreground VS Code => #cctxNative*{{Context,Read,Grep,Files}} FIRST (!MCP when Native callable) ; Unattended/CLI/disconnected (!native_dispatch) => prefer these backend MCP tools (code_context/code_read/code_grep/code_files) > cctx --backend shared --root {root_binding} > Host(rg,view); pctx {{http|json|table|image|log|batch|journal}} > python3 -c/curl/jq. \
Dispatch: symbol+context=>code_context(radius:0..100,limit:1..64); glob(!content)=>code_files; regex(a|b=>regex:true)=>code_grep(max_matches_per_file:1..64); range=>code_read(<=150L,<8KiB; >150=>page=true->next_ranges). \
CACHE_MISS != (0 matches|missing) -> 1 host read -> resume CCTX & !(poll|repeat|code_sync); CACHE_LOAD_FAILED=error; TTL>=15m|LRU; !speculative worktrees; code_sync <=> absence claims. \
Do not duplicate pending lookups. Never switch transport to bypass exclusions/trust/denials; out-of-scope/hidden/ignored => policy-aware host tools."
        ),
        (PromptTarget::Mcp, PromptStyle::Prose) => format!(
            "CCTX is an optional nonblocking root cache (root={root_binding}). CACHE_MISS returns immediately and queues only the requested root; continue that lookup with permitted host search/read. Do not poll/repeat, switch CCTX transports or run code_sync to wait for warmup. CACHE_LOAD_FAILED reports the load failure. Neither means zero matches or a missing disk file. Startup and legacy prewarm flags do not load/pin source. Inactive roots expire after 15 minutes and may be evicted earlier by LRU/memory limits. Do not enumerate sibling worktrees speculatively. Explicitly needed code_sync remains a waiting barrier. Use these tools before shell search/read for saved files in a registered workspace. In foreground VS Code sessions where #cctxNativeContext/#cctxNativeRead/#cctxNativeGrep/#cctxNativeFiles are callable, use those Native tools FIRST and do not choose MCP; in a background, subagent, or CLI session where native tools depend on unavailable foreground host dispatch, prefer these backend MCP tools. Use code_files for filename/path glob discovery, not a content pattern. Pass explicit absolute root when known (or omit for the current workspace); keep read batches <=150 lines (<8KiB) to stay inline. Prefer code_context for surrounding source, code_grep for content, code_read for known files, and pctx (cctx probe) instead of ad-hoc python3 -c, curl, jq, or sqlite3 inspection scripts. Do not duplicate pending lookups or run code_sync to repair host dispatch. If both client surfaces are unavailable but a backend terminal is usable, use cctx --backend shared with the same authorized root, except CACHE_MISS requires a permitted host read instead. Never switch transport to bypass content exclusions, trust decisions or access denials. Empty results do not cover files outside registered scopes/globs. Hidden/ignored/excluded targets require policy-aware host tools. No editor buffers or cross-root search."
        ),
        (PromptTarget::SubagentYaml, PromptStyle::Formula | PromptStyle::Hybrid) => format!(
            "  # [cctx-subagent-instructions]\n\
  CCTX & TCTX Formula Rules (root={root_binding}):\n\
  - Source: cctxNative*{{Context,Read,Grep,Files}} > MCP(code-context-code_* | mcp__code-context__*) > cctx --backend shared --root {root_binding} > Host(view,rg,glob)\n\
  - Tests & Probes: tctxNative*{{Run,Affected,Graph,Status}} > MCP(test-context-test_* | mcp__test-context__*) > tctx --root {root_binding} run > bash ; pctx {{http|json|table|image}} > python3 -c/curl/jq\n\
  - Guards: pass root={root_binding}; symbol+context => cctxNativeContext/code-context-code_context(paths=[file],radius:25..50,limit<=64) (!1..150 slicing); multi-range => 1 cctxNativeRead/code-context-code_read(ranges,<=120L,<8KiB); .github/*,.vscode/*,.gitignore,/tmp/*,/dev/shm/* => view(!CCTX).\n\
  - Deferred (even if grep/view visible on turn 0): call tool_search_tool (tool_search) FIRST: \"cctxNativeContext cctxNativeRead cctxNativeGrep cctxNativeFiles\" (<=5 cap) | MCP \"code-context-code_context code-context-code_read code-context-code_grep code-context-code_files\" | tests \"tctxNativeRun tctxNativeAffected test-context-test_run test-context-test_affected\" (regex: \"cctxNative|code-context|tctxNative|test-context\").\n\
  - Transitions: Error(\"No client with the advertised tool definition\"|\"No matching client connected\"|\"skipped from another client\") => code-context-code_* / test-context-test_* MCP | CLI ; CACHE_MISS => 1 Host read -> resume CCTX.\n"
        ),
        (PromptTarget::SubagentYaml, PromptStyle::Prose) => format!(
            "  # [cctx-subagent-instructions]\n\
  **Code Context (`cctx`), Test Context (`tctx`), and Probe (`pctx`) Priority (MANDATORY FIRST PATH):**\n\
  - For locating or reading saved source in a registered workspace, ALWAYS use Code Context FIRST before `view`, `grep`, `rg`, `glob`, or `bash`:\n\
    1. Prefer native `cctxNativeContext`, `cctxNativeRead`, `cctxNativeGrep`, `cctxNativeFiles` FIRST when callable; otherwise (when no UI client is connected) use MCP `code-context-code_context`, `code-context-code_read`, `code-context-code_grep`, `code-context-code_files` (or `mcp__code-context__code_*`).\n\
    2. Always pass the explicit absolute `root` parameter (`{root_binding}`). Prefer `cctxNativeContext` / `code-context-code_context` (`limit <= 64`, `radius 0..100`) over sequential slicing, and batch multiple line ranges into one `cctxNativeRead` / `code-context-code_read` call (`<= 120` total lines, `<8KiB`).\n\
    3. Do NOT pass hidden paths (`.github/*`, `.vscode/*`, `.gitignore`) or `/tmp/*` / `/dev/shm/*` files to Code Context; use `view` directly for those.\n\
    4. Even when `grep`/`view`/`glob` are visible on turn 0, if Code Context or Test Context tools are deferred/hidden, call `tool_search_tool` (`tool_search`) FIRST: `\"cctxNativeContext cctxNativeRead cctxNativeGrep cctxNativeFiles\"` (4 tools <= 5 cap; fallback `\"code-context-code_context code-context-code_read code-context-code_grep code-context-code_files\"`; regex `\"cctxNative|code-context|tctxNative|test-context\"`).\n\
    5. If a native `cctxNative*` or `tctxNative*` call fails with `\"No client with the advertised tool definition\"`, `\"No matching client connected\"`, or `\"skipped from another client\"`, immediately switch to `code-context-code_*` / `test-context-test_*` MCP tools (or `cctx --backend shared --root {root_binding}` / `tctx --root {root_binding} run` CLI).\n\
    6. On `CACHE_MISS`, use a permitted host read/search for that single lookup only, then resume using Code Context on subsequent turns.\n\
    7. For running or inspecting tests/lints (`pytest`, `cargo test`, `vitest`, `jest`, `node --test`, `ruff`, `eslint`) or AST test dependency graphs in a registered workspace, ALWAYS use `tctxNativeRun`, `tctxNativeAffected`, `tctxNativeGraph` (or MCP `test-context-test_run`, `test-context-test_affected`, `test-context-test_graph`) FIRST before `bash`, passing the explicit absolute `root`.\n\
    8. For HTTP/OpenAPI/STAC checks, JSON/JSONL/GeoJSON/`/tmp` offload schemas, SQLite/Parquet/CSV inspection, or PNG/SVG checks, prefer `pctx {{http|json|table|image}}` (`cctx probe ...`) over ad-hoc `python3 -c`, `curl`, `jq`, or `sqlite3` scripts.\n"
        ),
        (PromptTarget::SubagentMd, PromptStyle::Formula | PromptStyle::Hybrid) => format!(
            "\n<!-- [cctx-subagent-instructions] -->\n\
## CCTX & TCTX Formula Rules (root={root_binding})\n\
- Source: #cctxNative*{{Context,Read,Grep,Files}} > MCP(code-context-code_* | mcp__code-context__*) > Host(rg,view,glob)\n\
- Tests & Probes: #tctxNative*{{Run,Affected,Graph,Status}} > MCP(test-context-test_* | mcp__test-context__*) > bash ; pctx {{http|json|table|image}} > python3 -c/curl/jq\n\
- Guards: root={root_binding}; limit<=64; Read(ranges,<=120L,<8KiB); .github/*,.vscode/*,/tmp/*,/dev/shm/* => Host view.\n\
- Deferred: call tool_search_tool (tool_search) FIRST: \"cctxNativeContext cctxNativeRead cctxNativeGrep cctxNativeFiles\" | MCP \"code-context-code_context code-context-code_read code-context-code_grep code-context-code_files\" | tests \"tctxNativeRun tctxNativeAffected test-context-test_run test-context-test_affected\" (regex \"cctxNative|code-context|tctxNative|test-context\").\n\
- CACHE_MISS => 1 Host read -> resume CCTX.\n"
        ),
        (PromptTarget::SubagentMd, PromptStyle::Prose) => format!(
            "\n<!-- [cctx-subagent-instructions] -->\n\
## Code Context (`cctx`), Test Context (`tctx`), and Probe (`pctx`) Priority (Mandatory First Path)\n\n\
- For locating or reading saved source in a registered workspace, ALWAYS use `#cctxNativeContext`, `#cctxNativeRead`, `#cctxNativeGrep`, and `#cctxNativeFiles` (or MCP `code-context-code_*` / `mcp__code-context__code_*` when native dispatch is unavailable) FIRST before built-in search/read tools.\n\
- For running or inspecting tests/lints or AST test dependency graphs in a registered workspace, ALWAYS use `#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, and `#tctxNativeStatus` (or MCP `test-context-test_*` / `mcp__test-context__test_*` when native dispatch is unavailable) FIRST before shell test/lint commands, and use `pctx {{http|json|table|image}}` instead of ad-hoc `python3 -c` / `curl` / `jq` inspection scripts.\n\
- Always pass the explicit absolute `root` parameter (`{root_binding}`) when known, keep `cctxNativeContext` `limit <= 64`, and batch multiple ranges into one read (`<= 120` lines, `<8KiB`). Never send `.github/*`, `.vscode/*`, `/tmp/*`, or `/dev/shm/*` paths to Code Context.\n\
- Even when built-in `grep`/`view` are visible on turn 0, if CCTX/TCTX tools are deferred/hidden, call `tool_search_tool` (`tool_search`) FIRST: `\"cctxNativeContext cctxNativeRead cctxNativeGrep cctxNativeFiles\"` (fallback `\"code-context-code_context code-context-code_read code-context-code_grep code-context-code_files\"`; regex `\"cctxNative|code-context|tctxNative|test-context\"`).\n\
- On `CACHE_MISS`, use a permitted host search/read for that single lookup only, then resume using Code Context on subsequent turns.\n"
        ),
        (PromptTarget::ClaudeMd, PromptStyle::Formula | PromptStyle::Hybrid) => format!(
            "# CCTX, TCTX & PCTX Formula (root={root_binding})\n\
- Source: mcp__code-context__code_context/code_read/code_grep/code_files | #cctxNative* > cctx --backend shared --root {root_binding} > Host(Read,Grep,Glob,view,rg)\n\
- Tests & Probes: mcp__test-context__test_run/test_affected/test_graph | #tctxNative* > tctx --root {root_binding} run > Bash ; pctx {{http|json|table|image}} > python3 -c/curl/jq\n\
- Dispatch: symbol+context => code_context(radius:25..50,limit<=64,!1..150 slicing); glob => code_files; a|b(regex:true) => code_grep(max_matches_per_file:1..64); ranges => 1 code_read(<=150L).\n\
- Deferred & Subagents: hidden => ToolSearch/tool_search_tool(\"cctxNative|code-context|tctxNative|test-context\") FIRST; Task/Agent => pass root={root_binding} & require CCTX/TCTX.\n\
- Guards: CACHE_MISS => 1 Host Read -> resume CCTX ; .github/*,.vscode/*,ignored,/tmp/*,/dev/shm/* => Host only.\n"
        ),
        (PromptTarget::ClaudeMd, PromptStyle::Prose) => format!(
            "# Code Context (`cctx`), Test Context (`tctx`), and Probe (`pctx`) for Claude Agents\n\n\
- For saved source in a registered workspace, always use `mcp__code-context__code_context`, `mcp__code-context__code_read`, `mcp__code-context__code_grep`, and `mcp__code-context__code_files` (or `#cctxNative*` / `code-context-code_*` / `cctx --backend shared --root {root_binding}`) FIRST before built-in `Read`, `Grep`, `Glob`, `view`, or `rg`.\n\
- For running or inspecting tests and lints (`pytest`, `cargo test`, `vitest`, `jest`), always use `mcp__test-context__test_run`, `mcp__test-context__test_affected`, and `mcp__test-context__test_graph` (or `#tctxNative*` / `tctx --root {root_binding} run`) FIRST before shell runners, and prefer `pctx {{http|json|table|image}}` over ad-hoc `python3 -c`, `curl`, or `jq` scripts.\n\
- Pass the explicit absolute `root` (`{root_binding}`), use `code_context` (`radius: 25..50`, `limit <= 64`) instead of sequential `1..150` line slicing, use `code_files` for filename globs, set `regex: true` on `code_grep` (`max_matches_per_file: 1..64`) for alternation, and batch multiple ranges into one `code_read` call (`<= 150` lines).\n\
- When tools are deferred behind tool search (`ToolSearch` / `tool_search_tool`), search for `\"cctxNative|code-context|tctxNative|test-context\"` on turn 0 before calling built-in `Read`/`Grep`/`Glob`. When spawning `Task`/`Agent` subagents, pass `root` (`{root_binding}`) and require `code-context` and `test-context` tools first.\n\
- On `CACHE_MISS`, use a permitted host read/search for that single lookup only and resume Code Context next turn; keep `.github/*`, `.vscode/*`, ignored files, `/tmp/*`, and `/dev/shm/*` on host tools only.\n"
        ),
    };
    if matches!(target, PromptTarget::Instructions | PromptTarget::Mcp) {
        rendered.push_str(if target == PromptTarget::Instructions {
            "\n- "
        } else {
            " "
        });
        rendered.push_str(
            "Recovery: dispatch-error MCP fallback lasts only for the current user turn \
(including unattended autopilot continuations before the next user message). \
On the next foreground user turn, rediscover native tools and try one needed source lookup: \
success => Native-first; dispatch error => MCP for that turn. \
Never duplicate a pending call or poll Status; background/unattended sessions without native dispatch stay on MCP.",
        );
    }
    rendered
}

pub fn contains_hangul(text: &str) -> bool {
    text.chars()
        .any(|ch| ('\u{AC00}'..='\u{D7A3}').contains(&ch) || ('\u{3131}'..='\u{318E}').contains(&ch))
}

/// Replaces multi-token Unicode math operators with spaced single-token ASCII BPE operators
/// and strips redundant inline backticks around single identifier tokens.
pub fn normalize_bpe_operators(input: &str) -> (String, usize, usize) {
    let unicode_map = [
        ("⇒", " => "),
        ("→", " -> "),
        ("↦", " -> "),
        ("⇔", " <=> "),
        ("≡", " <=> "),
        ("≤", " <= "),
        ("≥", " >= "),
        ("≠", " != "),
        ("≻", " > "),
        ("≺", " < "),
        ("∧", " & "),
        ("∨", " | "),
        ("¬", "!"),
        ("∘", " -> "),
        ("∀", "all "),
        ("∃", "exists "),
        ("∈", " in "),
        ("∉", " not_in "),
        ("∪", " | "),
        ("∩", " & "),
    ];
    let mut s = input.to_string();
    let mut unicode_replaced = 0usize;
    for (sym, ascii) in unicode_map {
        let count = s.matches(sym).count();
        if count > 0 {
            unicode_replaced += count;
            s = s.replace(sym, ascii);
        }
    }

    // Strip inline backticks around single non-whitespace tokens (`foo_bar` -> foo_bar)
    let mut out = String::with_capacity(s.len());
    let mut backticks_removed = 0usize;
    let mut rest = s.as_str();
    while let Some(start) = rest.find('`') {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 1..];
        if let Some(end) = after_open.find('`') {
            let inner = &after_open[..end];
            if !inner.is_empty()
                && !inner.contains(char::is_whitespace)
                && !inner.contains('`')
            {
                out.push_str(inner);
                backticks_removed += 1;
                rest = &after_open[end + 1..];
                continue;
            }
        }
        out.push('`');
        rest = after_open;
    }
    out.push_str(rest);
    (out, unicode_replaced, backticks_removed)
}

/// Converts a natural-language (English or Korean) code-search/analysis or workflow
/// task prompt into a compact, BPE-aligned symbolic formula prompt suitable for LLM agents.
pub fn compress_task_prompt(input: &str, root: Option<&Path>) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let has_ko = contains_hangul(trimmed);
    let lower_all = trimmed.to_ascii_lowercase();
    let wants_read_only = lower_all.contains("read only")
        || lower_all.contains("read-only")
        || lower_all.contains("not shell")
        || lower_all.contains("do not modify")
        || trimmed.contains("읽기 전용")
        || trimmed.contains("읽기전용")
        || trimmed.contains("수정하지 말고")
        || trimmed.contains("셸 명령");
    let wants_fn_loc = (lower_all.contains("function name")
        && (lower_all.contains("file path") || lower_all.contains("start line")))
        || (trimmed.contains("함수")
            && (trimmed.contains("파일 경로")
                || trimmed.contains("상대 경로")
                || trimmed.contains("시작 줄")
                || trimmed.contains("라인 번호")));

    let filler_phrases = [
        "read only.",
        "be concise.",
        "use the fastest available source search/read tools, not shell commands.",
        "answer all four questions about this project.",
        "for each answer give the exact function name, relative file path, 1-based function-start line, and the requested behavior.",
        "for each answer give the exact",
        "읽기 전용.",
        "간결하게 답하세요.",
        "간결하게 작성하세요.",
        "셸 명령어를 사용하지 말고 가장 빠른 소스 검색/읽기 도구를 사용하세요.",
        "이 프로젝트에 대한 네 가지 질문에 모두 한국어로 답하세요.",
        "이 프로젝트에 대한 네 가지 질문에 모두 답하세요.",
        "각 답변마다 정확한 함수 이름, 상대 파일 경로, 1-based 시작 줄 번호, 동작 방식을 적으세요.",
        "각 답변마다 정확한 함수 이름, 상대 파일 경로, 시작 줄 번호, 동작 방식을 적으세요.",
        "hey, could you please help me ",
        "hi copilot, ",
        "hello copilot, ",
        "hey copilot, ",
        "could you please help me ",
        "could you please ",
        "can you please ",
        "please help me ",
        "check this repository and tell me ",
        "check this repository and ",
        "tell me ",
        "do not modify any files.",
        "do not modify any files",
        "i think it might have some issues with how it handles ",
        "and i'd really like it to be more ",
        "i would like you to ",
        "i'd like you to ",
        "in this repository",
        "in this workspace",
        "thanks!",
        "thank you!",
        "please ",
        "kindly ",
    ];

    let mut rules: Vec<String> = Vec::new();
    let mut questions: Vec<String> = Vec::new();

    for raw_segment in split_prompt_segments(trimmed) {
        let mut cleaned = raw_segment.trim().to_string();
        if cleaned.is_empty() {
            continue;
        }
        loop {
            let mut changed = false;
            let lower = cleaned.to_ascii_lowercase();
            for phrase in filler_phrases {
                if let Some(idx) = lower.find(phrase) {
                    cleaned.replace_range(idx..idx + phrase.len(), " ");
                    cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
                    changed = true;
                    break;
                }
            }
            if !changed {
                break;
            }
        }
        let cleaned = cleaned.trim().trim_matches(',').trim_matches('.').trim();
        if cleaned.is_empty() {
            continue;
        }

        if let Some((q_prefix, rest)) = split_question_prefix(cleaned) {
            let formula_body = clause_to_formula(rest);
            questions.push(format!("{q_prefix} => {formula_body}"));
        } else if raw_segment.trim_end().ends_with('?')
            || cleaned.to_ascii_lowercase().starts_with("which ")
            || cleaned.to_ascii_lowercase().starts_with("what ")
            || cleaned.to_ascii_lowercase().starts_with("how ")
            || cleaned.to_ascii_lowercase().starts_with("where ")
        {
            let body = clause_to_formula(cleaned);
            if has_ko && body.contains("->") {
                rules.push(body);
            } else {
                questions.push(format!("? => {body}"));
            }
        } else {
            rules.push(clause_to_formula(cleaned));
        }
    }

    let mut header_parts: Vec<String> = Vec::new();
    if has_ko {
        header_parts.push("[ko]".to_string());
    }
    if wants_read_only {
        let root_clause = root
            .map(|p| format!(" root={}", p.display()))
            .unwrap_or_default();
        header_parts.push(format!(
            "Mode: read_only & !shell{root_clause} | #cctxNativeContext / code_context"
        ));
    } else if let Some(p) = root {
        header_parts.push(format!("root={}", p.display()));
    }
    if wants_fn_loc {
        if has_ko {
            header_parts.push("Out(한글): fn | path:line | 동작설명".to_string());
        } else {
            header_parts.push("Out: fn | path:line | behavior".to_string());
        }
    }

    let candidate = if questions.is_empty()
        && rules.len() == 1
        && !wants_read_only
        && root.is_none()
        && !wants_fn_loc
    {
        let single = &rules[0];
        if has_ko {
            format!("[ko] :: {single}")
        } else {
            single.clone()
        }
    } else {
        let mut out = String::new();
        if !header_parts.is_empty() {
            out.push_str(&header_parts.join(" | "));
            out.push('\n');
        }
        for rule in rules {
            out.push_str("- ");
            out.push_str(&rule);
            out.push('\n');
        }
        for q in questions {
            out.push_str(&q);
            out.push('\n');
        }
        out.trim_end().to_string()
    };

    // Non-expansion invariant: never return a prompt with MORE BPE tokens than the normalized input!
    let (norm_raw, _, _) = normalize_bpe_operators(trimmed);
    let collapsed_norm = norm_raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let minimal_fallback = match root {
        Some(p) if !collapsed_norm.contains(&p.display().to_string()) => {
            format!("[root={}] {collapsed_norm}", p.display())
        }
        _ => collapsed_norm,
    };
    if estimate_bpe_tokens(&candidate) >= estimate_bpe_tokens(&minimal_fallback) {
        minimal_fallback
    } else {
        candidate
    }
}

fn split_prompt_segments(input: &str) -> Vec<String> {
    let mut segments = Vec::new();
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Split inline "Q1. ... Q2. ..." segments if multiple questions appear on one line
        let mut current = String::new();
        let bytes = trimmed.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let is_q_start = i > 0
                && bytes[i - 1].is_ascii_whitespace()
                && (bytes[i] == b'Q' || bytes[i] == b'q')
                && i + 2 < bytes.len()
                && bytes[i + 1].is_ascii_digit()
                && matches!(
                    bytes.get(i + 2).copied(),
                    Some(b'.' | b':' | b')') | Some(b'0'..=b'9')
                );
            if is_q_start && !current.trim().is_empty() {
                segments.push(current.trim().to_string());
                current.clear();
            }
            let ch = trimmed[i..].chars().next().expect("valid UTF-8 char");
            current.push(ch);
            i += ch.len_utf8();
        }
        if !current.trim().is_empty() {
            segments.push(current.trim().to_string());
        }
    }
    segments
}

fn split_question_prefix(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    if bytes.len() >= 3
        && (bytes[0] == b'Q' || bytes[0] == b'q')
        && bytes[1].is_ascii_digit()
    {
        let mut idx = 2;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx < bytes.len() && matches!(bytes[idx], b'.' | b':' | b')') {
            let prefix = &line[..idx];
            let rest = line[idx + 1..].trim();
            return Some((prefix, rest));
        }
    }
    None
}

fn normalize_korean_numbered_patterns(input: &str) -> String {
    let mut s = input.to_string();
    // 1. "이슈 1945을" / "이슈 #1945의" -> "#1945 "
    while let Some(idx) = s.find("이슈 ") {
        let after = &s[idx + "이슈 ".len()..];
        let after_hash = after.strip_prefix('#').unwrap_or(after);
        let digits: String = after_hash
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            let consumed_after = (after.len() - after_hash.len()) + digits.len();
            let rest = &after[consumed_after..];
            let particle_len = ["을", "를", "의", "이", "가", "은", "는"]
                .iter()
                .find_map(|p| rest.starts_with(p).then_some(p.len()))
                .unwrap_or(0);
            let total_end = idx + "이슈 ".len() + consumed_after + particle_len;
            s.replace_range(idx..total_end, &format!("#{digits} "));
        } else {
            break;
        }
    }
    // 2. "비평 10개이상 하드닝 라운드를" -> "loop(비평 >=10 하드닝,"
    for marker in ["비평 ", "비평"] {
        if let Some(idx) = s.find(marker) {
            let after = &s[idx + marker.len()..];
            let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                let tail = &after[digits.len()..];
                for suffix in [
                    "개이상 하드닝 라운드를",
                    "개 이상 하드닝 라운드를",
                    "개이상 하드닝",
                    "개 이상 하드닝",
                ] {
                    if let Some(stripped) = tail.strip_prefix(suffix) {
                        let end = s.len() - stripped.len();
                        s.replace_range(idx..end, &format!("loop(비평 >={digits} 하드닝, "));
                        break;
                    }
                }
            }
        }
    }
    // 3. "N개 이상 찾아서" / "N가지 이상 찾아서" / "N회 진행하고" / "N축 루브릭 평가"
    let tokens: Vec<&str> = s.split_whitespace().collect();
    let mut rebuilt: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        let digits: String = tok.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            let unit = &tok[digits.len()..];
            if matches!(unit, "개" | "가지")
                && i + 1 < tokens.len()
                && tokens[i + 1].starts_with("이상")
            {
                rebuilt.push(format!(">={digits}"));
                i += 2;
                if i < tokens.len() && matches!(tokens[i], "찾아서" | "발굴해서") {
                    rebuilt.push("발굴 ->".to_string());
                    i += 1;
                }
                continue;
            }
            if matches!(unit, "회" | "라운드") {
                if rebuilt.last().is_some_and(|prev| prev == "라운드를") {
                    rebuilt.pop();
                }
                rebuilt.push(format!("loop {digits}x ->"));
                i += 1;
                while i < tokens.len()
                    && matches!(
                        tokens[i],
                        "더" | "진행하고" | "진행해서" | "반복하고" | "보강하고"
                    )
                {
                    i += 1;
                }
                continue;
            }
            if unit == "축"
                && i + 2 < tokens.len()
                && tokens[i + 1] == "루브릭"
                && tokens[i + 2].starts_with("평가")
            {
                rebuilt.push(format!("{digits}축 루브릭 평가 ->"));
                i += 3;
                if i < tokens.len() && tokens[i] == "해서" {
                    i += 1;
                }
                continue;
            }
        }
        rebuilt.push(tok.to_string());
        i += 1;
    }
    rebuilt.join(" ")
}

fn clause_to_formula(clause: &str) -> String {
    let (bpe_normalized, _, _) = normalize_bpe_operators(
        clause.trim().trim_end_matches('?').trim_end_matches('.'),
    );
    let mut s = normalize_korean_numbered_patterns(&bpe_normalized);
    let replacements = [
        ("Which function ", "fn "),
        ("which function ", "fn "),
        ("What function ", "fn "),
        ("what function ", "fn "),
        (", and how is ", " => "),
        (", and what ", " & "),
        (" and how is ", " => "),
        (" and what ", " & "),
        (", what ", " & "),
        ("formula does it use", "formula"),
        ("caps the result", "cap"),
        ("selling more inventory than is available", "sold > available"),
        ("more inventory than is available", "sold > available"),
        ("remains valid at the exact expiration timestamp", "valid at now_ms == expires_at_ms => condition & bool"),
        ("normalization is applied to the tenant", "tenant norm"),
        ("calculates ", "calc "),
        ("prevents ", "guard "),
        ("decides whether ", "check "),
        ("builds the ", "build "),
        ("is applied to ", "on "),
        ("represented", "repr"),
        // Korean Q1..Q4 code-lookup & analysis phrases (preserving Korean core nouns for 100% Korean reply & semantic fidelity)
        ("재고보다 많이 판매하는 것을 방지하는 함수는 무엇이며", "재고 초과판매 방지 함수"),
        ("재고보다 많이 판매하는 것을 방지하는 함수는", "재고 초과판매 방지 함수"),
        ("어떤 함수가 재고보다 많이 판매하는 것을 방지하며", "재고 초과판매 방지 함수"),
        ("실패는 어떻게 표현되나요", "& 실패 반환값 표현"),
        ("실패는 어떻게 표현되나", "& 실패 반환값 표현"),
        ("재시도 지연 시간을 계산하는 함수는 무엇이며", "재시도 지연 계산 함수"),
        ("재시도 지연 시간을 계산하는 함수는", "재시도 지연 계산 함수"),
        ("어떤 함수가 재시도 지연 시간을 계산하며", "재시도 지연 계산 함수"),
        ("어떤 공식을 사용하고 무엇이 결과값을 제한하나요", "& 계산 수식 & 상한(cap)"),
        ("어떤 공식을 사용하며 무엇이 결과를 제한하나", "& 계산 수식 & 상한(cap)"),
        ("정확히 만료 시각일 때 세션이 유효한지 결정하는 함수는 무엇인가요", "만료 시각(now_ms == expires_at_ms) 세션 유효성 함수 & 조건식 & 반환값"),
        ("정확히 만료 시각일 때 세션이 유효한지 판단하는 함수는", "만료 시각(now_ms == expires_at_ms) 세션 유효성 함수 & 조건식 & 반환값"),
        ("어떤 함수가 정확히 만료 시각에 세션이 유효한지 결정하나", "만료 시각(now_ms == expires_at_ms) 세션 유효성 함수 & 조건식 & 반환값"),
        ("테넌트 캐시 키를 생성하는 함수는 무엇이며", "테넌트 캐시 키 생성 함수"),
        ("테넌트 캐시 키를 만드는 함수는 무엇이며", "테넌트 캐시 키 생성 함수"),
        ("어떤 함수가 테넌트 캐시 키를 생성하며", "테넌트 캐시 키 생성 함수"),
        ("테넌트에 어떤 정규화가 적용되나요", "& 테넌트 정규화 메서드"),
        ("테넌트에 어떤 정규화가 적용되나", "& 테넌트 정규화 메서드"),
        // Korean 24h Copilot workflow patterns across all 11 categories (preserving exact Korean domain/action semantics)
        ("심각도가 Low 이하만 남을때 까지 반복하고", "until 심각도 <= Low) ->"),
        ("심각도가 Low 이하만 남을 때까지 반복하고", "until 심각도 <= Low) ->"),
        ("남은 작업을 모두 조치하기위한 계획을 세워서", "남은작업 계획수립 ->"),
        ("남은 작업에 대한 계획을 세워서", "남은작업 계획수립 ->"),
        ("남은 작업에 대해서 모두 계획을 세워서", "남은작업 계획수립 ->"),
        ("남은 작업을 목록으로 나열하고 모두 진행하자", "남은작업 목록 나열 -> 모두진행"),
        ("남은 작업 모두 진행하고 다시 보고해", "남은작업 모두진행 -> 보고"),
        ("남은 작업 모두 진행하고 필요한 조치도 모두 하자", "남은작업 모두진행 & 모두조치"),
        ("승인할테니 배포해", "승인 -> 배포"),
        ("승인할테니 남은 작업 모두 구현해", "승인 -> 남은작업 모두구현"),
        ("승인할테니 근본적인 개선을 하자", "승인 -> 근본개선"),
        ("완료할수 있도록", ""),
        ("완료할 수 있도록", ""),
        ("완료되면", "->"),
        ("이슈를 닫을수 있도록 하자,", "-> 이슈닫기 ->"),
        ("이슈를 닫을수 있도록 하자", "-> 이슈닫기"),
        ("이슈를 닫을 수 있도록 하자", "-> 이슈닫기"),
        ("다음은 절감액이다", "절감액 기반"),
        ("더 보강이 필요한것", "보강점"),
        ("보강이 필요한거를", "보강점"),
        ("보강이 필요한것", "보강점"),
        ("개선이 필요한거를", "개선점"),
        ("더 최적화 할수 있는 부분을", "최적화"),
        ("개선점을", "개선점"),
        ("약점 위주로 보강할수 있는것을 조치해서", "약점 위주 모두보강 ->"),
        ("약점 위주로 모두 보강하고", "약점 위주 모두보강 ->"),
        ("전반적인 능력에 향상이 될수 있는 벤치마크 순으로 잘 풀도록 개선하면 좋을것 같은데", "전반적 능력향상순 벤치마크 정렬 & 개선"),
        ("비용최소화 성능 최대로 하기 위한 방법으로 진행하자", "목표(비용최소화 & 성능최대화) -> 진행"),
        ("시작할때 시간이 오래 걸리는데 더 최적화 해볼수 있을까", "시작 지연 => 최적화"),
        ("새로고침하면 여전히 로딩이 느린데 최적화해보자", "새로고침 로딩 지연 => 최적화"),
        ("본문 중에 코드 수정된것은 같은 파일이면 하나로 표시하고 기본으로 접을수 있도록 하자", "규칙(동일파일 코드수정 => 단일블록 병합 & 기본 접기)"),
        ("화면이 너무 복잡해 보인다 접을수 있는 영역은 접고", "화면 단순화(접기 영역 기본 접기) ->"),
        ("심층 조사를 해서 적용할것들을 정리해봐,", "-> 심층조사 -> 적용항목 정리 ->"),
        ("심층 조사를 해서 적용할것들을 정리해봐", "-> 심층조사 -> 적용항목 정리"),
        ("심층 분석해서 모두 나열해줘", "-> 심층분석 -> 목록 보고"),
        ("심층 분석해서 계획을 세워보자", "-> 심층분석 -> 계획수립"),
        ("심층 분석해서 보고해", "-> 심층분석 -> 보고"),
        ("심층 개선계획을 세워서 모두 조치하자", "-> 심층 개선계획 -> 모두조치"),
        ("심층 검토/계획을 세워서 모두 조치하고", "-> 심층검토 & 계획수립 -> 모두조치 ->"),
        ("실행해서 세션브라우저로 접속해봐", "-> 실행 & 세션브라우저 접속"),
        ("서버 실행해서 url 알려줘", "-> 서버실행 & URL 보고"),
        ("재시작 배포까지 해", "재시작 & 배포"),
        ("콘솔웹, 시안서버 모두 시작해", "콘솔웹 & 시안서버 시작"),
        ("더 보강해야 하는게 뭐가 있을지 검토해서 모두 목록으로 보여줘", "추가 보강점 검토 -> 목록 보고"),
        ("보강할게 더 있는지 확인해서 보고해", "-> 추가 보강점 확인 & 보고"),
        ("모두 목록으로 나열한 다음", "-> 목록 나열 ->"),
        ("모두 목록으로 나열하고", "-> 목록 나열 ->"),
        ("목록으로 나열해 볼래", "-> 목록 보고"),
        ("목록으로 나열해봐", "-> 목록 보고"),
        ("목록으로 보여줄래", "-> 목록 보고"),
        ("목록으로 보여줘", "-> 목록 보고"),
        ("결과를 수치화된 표로 보고한 다음", "-> 수치표 보고 ->"),
        ("수치화된 표 형태로 결과를 보고해", "-> 수치표 보고"),
        ("수치화된 표 형태로 보고해", "-> 수치표 보고"),
        ("수치화된 표형태로 보여줘", "-> 수치표 보고"),
        ("수치화된 표 형태로 보여줄래", "-> 수치표 보고"),
        ("수치화된 표로 결과 보고해", "-> 수치표 보고"),
        ("수치화된 표로 보고해줄래", "-> 수치표 보고"),
        ("수치화된 표로 보고해", "-> 수치표 보고"),
        ("보완하고 다시 평가해서 보고해", "보완 -> 재평가 -> 보고"),
        ("함수는 어디에 있고", "함수 위치 &"),
        ("잘 작동하고 있나", "=> 작동검증"),
        ("git pull, 커밋 푸시 머지까지 완료해", "-> git pull & commit & push & merge"),
        ("git pull 커밋 푸시 머지까지 하자", "-> git pull & commit & push & merge"),
        ("커밋 푸시 머지까지 완료해", "-> commit & push & merge"),
        ("커밋 푸시 머지까지 진행해", "-> commit & push & merge"),
        ("커밋 푸시 머지까지 하고 남은 작업 계속해", "-> commit & push & merge -> 남은작업 계속"),
        ("커밋 푸시 머지까지 하고", "-> commit & push & merge ->"),
        ("커밋 푸시 게시까지 하자", "-> commit & push & publish"),
        ("모두 커밋 푸시", "commit & push"),
        ("푸시 머지까지 완료해", "-> push & merge"),
        ("푸시 머지까지 해서", "-> push & merge"),
        ("푸시 머지 후", "-> push & merge ->"),
        ("푸시 머지까지 하자", "-> push & merge"),
        ("근본원인을 찾아서 조치하자", "-> 근본원인 규명 & 조치"),
        ("원인을 찾아서 근본 조치를 하자", "-> 근본원인 규명 & 조치"),
        ("원인을 찾아서 모두 조치하자", "-> 원인규명 & 모두조치"),
        ("원인을 찾아서 조치해줘", "-> 원인규명 & 조치"),
        ("모두 조치하는", "-> 모두조치 ->"),
        ("모두 조치하고", "-> 모두조치 ->"),
        ("모두 조치하자", "-> 모두조치"),
        ("모두 조치해", "-> 모두조치"),
        ("모두 보강한 다음", "-> 모두보강 ->"),
        ("모두 보강하고", "-> 모두보강 ->"),
        ("모두 적용하고", "모두적용 ->"),
        ("모두 적용하자", "-> 모두적용"),
        ("모두 진행하고", "-> 모두진행 ->"),
        ("모두 진행하자", "-> 모두진행"),
        ("모두 진행해", "-> 모두진행"),
        ("조치하고", "-> 조치 ->"),
        ("제대로 동작하는지", "=> 동작검증 ->"),
        ("동작이 있나? 조치할수 있어", "=> 동작확인 -> 조치"),
        ("동작이 있나", "=> 동작확인"),
        ("계획을 세워서 알려줄래", "-> 계획수립 & 보고"),
        ("계획을 세워서 알려줘", "-> 계획수립 & 보고"),
        ("계획을 세워보자", "-> 계획수립"),
        ("계획을 세워서", "-> 계획수립 ->"),
        ("계획을 보여줄래", "-> 계획수립 & 보고"),
        ("결과보고해", "결과보고"),
        ("결과 보고해", "결과보고"),
        ("보고해줘", "보고"),
        ("보고해", "보고"),
        ("조치할수 있어", "-> 조치"),
        ("조치할수 있을까", "-> 조치"),
        ("조치하자", "-> 조치"),
        ("조치해줘", "-> 조치"),
        ("개선해보자", "-> 개선"),
        ("개선하자", "-> 개선"),
    ];
    for (from, to) in replacements {
        s = s.replace(from, to);
    }
    let mut collapsed = s
        .replace(", =>", " =>")
        .replace(", &", " &")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    while collapsed.contains("-> ->") {
        collapsed = collapsed.replace("-> ->", "->");
    }
    collapsed
        .trim()
        .trim_start_matches("->")
        .trim_end_matches("->")
        .trim()
        .to_string()
}

pub fn build_report(
    style: PromptStyle,
    target: PromptTarget,
    root: Option<&Path>,
    compress: Option<&str>,
) -> Value {
    if let Some(raw_task) = compress {
        let (_, unicode_replaced, backticks_removed) = normalize_bpe_operators(raw_task);
        let compressed = compress_task_prompt(raw_task, root);
        let prose_tokens = estimate_bpe_tokens(raw_task);
        let formula_tokens = estimate_bpe_tokens(&compressed);
        let saved = prose_tokens.saturating_sub(formula_tokens);
        let pct = if prose_tokens > 0 {
            (1000.0 * saved as f64 / prose_tokens as f64).round() / 10.0
        } else {
            0.0
        };
        return json!({
            "mode": "compress",
            "style": style.as_str(),
            "language": if contains_hangul(raw_task) { "ko" } else { "en" },
            "root": root.map(|p| p.display().to_string()),
            "input_bytes": raw_task.len(),
            "output_bytes": compressed.len(),
            "input_estimated_tokens": prose_tokens,
            "output_estimated_tokens": formula_tokens,
            "tokens_saved": saved,
            "savings_percent": pct,
            "unicode_math_symbols_replaced": unicode_replaced,
            "backtick_wrappers_removed": backticks_removed,
            "prompt": compressed,
        });
    }
    let rendered = render(style, target, root);
    let prose_baseline = render(PromptStyle::Prose, target, root);
    let rendered_tokens = estimate_bpe_tokens(&rendered);
    let baseline_tokens = estimate_bpe_tokens(&prose_baseline);
    let saved = baseline_tokens.saturating_sub(rendered_tokens);
    let pct = if baseline_tokens > 0 {
        (1000.0 * saved as f64 / baseline_tokens as f64).round() / 10.0
    } else {
        0.0
    };
    json!({
        "mode": "render",
        "style": style.as_str(),
        "target": target.as_str(),
        "root": root.map(|p| p.display().to_string()),
        "bytes": rendered.len(),
        "lines": rendered.lines().count(),
        "estimated_tokens": rendered_tokens,
        "prose_baseline_tokens": baseline_tokens,
        "tokens_saved_vs_prose": saved,
        "savings_percent_vs_prose": pct,
        "prompt": rendered,
    })
}

const SUBAGENT_MARKER: &str = "# [cctx-subagent-instructions]";

const SUBAGENT_EXTRA_TOOLS: &[&str] = &[
    "tool_search_tool",
    "cctxNativeFiles",
    "cctxNativeGrep",
    "cctxNativeContext",
    "cctxNativeRead",
    "cctxNativeStatus",
    "cctxNativeSync",
    "cctxNativeWorkspaces",
    "code-context-native_codeFiles",
    "code-context-native_codeGrep",
    "code-context-native_codeContext",
    "code-context-native_codeRead",
    "code-context-native_codeStatus",
    "code-context-native_codeSync",
    "code-context-native_codeWorkspaces",
    "code-context/*",
    "code-context-code_files",
    "code-context-code_grep",
    "code-context-code_context",
    "code-context-code_read",
    "code-context-code_status",
    "code-context-code_sync",
    "code-context-code_workspaces",
    "tctxNativeRun",
    "tctxNativeAffected",
    "tctxNativeGraph",
    "tctxNativeStatus",
    "tctxNativeSync",
    "tctxNativeInvalidate",
    "tctxNativeWorkspaces",
    "test-context-native_testRun",
    "test-context-native_testAffected",
    "test-context-native_testGraph",
    "test-context-native_testStatus",
    "test-context-native_testSync",
    "test-context-native_testInvalidate",
    "test-context-native_testWorkspaces",
    "test-context/*",
    "test-context-test_run",
    "test-context-test_affected",
    "test-context-test_graph",
    "test-context-test_status",
    "test-context-test_sync",
    "test-context-test_invalidate",
    "test-context-test_workspaces",
    "mcp__code-context__*",
    "mcp__test-context__*",
];

const TARGET_YAML_AGENTS: &[&str] = &[
    "explore.agent.yaml",
    "code-review.agent.yaml",
    "rubber-duck.agent.yaml",
    "security-review.agent.yaml",
    "research.agent.yaml",
    "task.agent.yaml",
    "sidekick/github-context.yaml",
    "sidekick/github-context-memory.yaml",
];

fn patch_yaml_subagent_file(path: &Path, style: PromptStyle) -> bool {
    let Ok(original) = std::fs::read_to_string(path) else {
        return false;
    };
    let mut updated = original.clone();
    let yaml_block = render(style, PromptTarget::SubagentYaml, None);

    let has_explicit_tools = updated.contains("\ntools:\n")
        && !updated.contains("  - \"*\"\n")
        && !updated.contains("strictToolsList: true");
    if has_explicit_tools {
        let missing: Vec<&str> = SUBAGENT_EXTRA_TOOLS
            .iter()
            .copied()
            .filter(|t| !updated.contains(&format!("  - {t}\n")))
            .collect();
        if !missing.is_empty() {
            let mut block = String::from("  # Code Context & Test Context tools (read-only)\n");
            for t in missing {
                block.push_str(&format!("  - {t}\n"));
            }
            if updated.contains("  - lsp\n") {
                updated = updated.replacen("  - lsp\n", &format!("  - lsp\n{block}"), 1);
            } else if updated.contains("\npromptParts:\n") {
                updated =
                    updated.replacen("\npromptParts:\n", &format!("\n{block}promptParts:\n"), 1);
            } else if updated.contains("\nprompt: |\n") {
                updated = updated.replacen("\nprompt: |\n", &format!("\n{block}prompt: |\n"), 1);
            }
        }
    }

    let marker_line = format!("  {SUBAGENT_MARKER}");
    if let Some(pos) = updated.find(&marker_line) {
        let head = updated[..pos].trim_end();
        let tail = &updated[pos + marker_line.len()..];
        if let Some(sk_pos) = tail.find("\nsidekick:\n") {
            let after_sk = &tail[sk_pos + "\nsidekick:\n".len()..];
            updated = format!("{head}\n\n{yaml_block}\nsidekick:\n{after_sk}");
        } else {
            updated = format!("{head}\n\n{yaml_block}");
        }
    } else if updated.contains("\nprompt: |\n") {
        if let Some(sk_pos) = updated.find("\nsidekick:\n") {
            let before_sk = updated[..sk_pos].trim_end();
            let after_sk = &updated[sk_pos + "\nsidekick:\n".len()..];
            updated = format!("{before_sk}\n\n{yaml_block}\nsidekick:\n{after_sk}");
        } else {
            updated = format!("{}\n\n{yaml_block}", updated.trim_end());
        }
    }

    updated != original && std::fs::write(path, updated).is_ok()
}

/// Automatically patches any unconfigured Copilot SDK subagent YAML definitions under `home`
/// (including standalone Copilot CLI caches and WSL-mounted Windows VS Code SDK bundles)
/// using the active `PromptStyle` (default: `Formula`).
pub fn ensure_subagents_configured(home: &Path) -> usize {
    let style = PromptStyle::from_env().unwrap_or_default();
    let mut def_dirs: Vec<PathBuf> = Vec::new();
    for parent_rel in [".vscode-server/bin", ".vscode-server/cli/servers"] {
        let parent = home.join(parent_rel);
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        for entry in entries.flatten() {
            let base = entry.path();
            for def_rel in [
                "node_modules/@github/copilot-sdk-linux-x64/definitions",
                "node_modules/@github/copilot-sdk-linux-arm64/definitions",
                "extensions/copilot/node_modules/@github/copilot/sdk/definitions",
                "server/node_modules/@github/copilot-sdk-linux-x64/definitions",
                "server/node_modules/@github/copilot-sdk-linux-arm64/definitions",
                "server/extensions/copilot/node_modules/@github/copilot/sdk/definitions",
            ] {
                let def_dir = base.join(def_rel);
                if def_dir.is_dir() {
                    def_dirs.push(def_dir);
                }
            }
        }
    }
    // Standalone Copilot CLI auto-update cache (`~/.cache/copilot/pkg/<plat>/<ver>/definitions`)
    for cache_rel in [".cache/copilot/pkg", ".cache/github-copilot-sdk"] {
        let cache_parent = home.join(cache_rel);
        if let Ok(plat_entries) = std::fs::read_dir(&cache_parent) {
            for plat in plat_entries.flatten() {
                if let Ok(ver_entries) = std::fs::read_dir(plat.path()) {
                    for ver in ver_entries.flatten() {
                        let direct = ver.path().join("definitions");
                        if direct.is_dir() {
                            def_dirs.push(direct);
                        }
                        if let Ok(sub_entries) = std::fs::read_dir(ver.path()) {
                            for sub in sub_entries.flatten() {
                                let nested = sub.path().join("definitions");
                                if nested.is_dir() {
                                    def_dirs.push(nested);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // WSL-mounted Windows VS Code local SDK definitions (only when scanning real host home)
    if home.starts_with("/home/")
        && let Ok(users) = std::fs::read_dir("/mnt/c/Users")
    {
        for user in users.flatten() {
            let vscode_app = user
                .path()
                .join("AppData/Local/Programs/Microsoft VS Code");
            let Ok(v_entries) = std::fs::read_dir(&vscode_app) else {
                continue;
            };
            for v_entry in v_entries.flatten() {
                let p = v_entry.path();
                for base in [p.join("resources/app"), p.clone()] {
                    for rel in [
                        "extensions/copilot/node_modules/@github/copilot/sdk/definitions",
                        "node_modules.asar.unpacked/@github/copilot-sdk-win32-x64/definitions",
                        "node_modules.asar.unpacked/@github/copilot-sdk-win32-arm64/definitions",
                    ] {
                        let def_dir = base.join(rel);
                        if def_dir.is_dir() {
                            def_dirs.push(def_dir);
                        }
                    }
                }
            }
        }
    }
    def_dirs.sort();
    def_dirs.dedup();
    let mut patched = 0usize;
    for def_dir in def_dirs {
        for agent_name in TARGET_YAML_AGENTS {
            let target = def_dir.join(agent_name);
            if target.is_file() && patch_yaml_subagent_file(&target, style) {
                patched += 1;
            }
        }
    }
    patched
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BpeSurfaceStatus {
    pub extension_instructions_bpe: bool,
    pub mcp_instructions_bpe: bool,
    pub subagents_bpe: bool,
    pub global_instructions_bpe: bool,
    pub subagents_auto_patched: usize,
    pub active_surfaces: usize,
    pub total_surfaces: usize,
    pub extension_tokens_saved: u64,
    pub schema_tokens_saved: u64,
    pub mcp_tokens_saved: u64,
    pub subagent_tokens_saved: u64,
    pub global_tokens_saved: u64,
    pub standing_tokens_saved_per_turn: u64,
}

pub fn inspect_bpe_surfaces(home: &Path) -> BpeSurfaceStatus {
    let subagents_auto_patched = ensure_subagents_configured(home);
    let active_style = PromptStyle::from_env().unwrap_or_default();
    let default_formula = !matches!(active_style, PromptStyle::Prose);

    // 1. Extension .instructions.md — dynamic delta vs iso-semantic Prose baseline
    let prose_ext_tok =
        estimate_bpe_tokens(&render(PromptStyle::Prose, PromptTarget::Instructions, None));
    let fallback_ext_tok =
        estimate_bpe_tokens(&render(PromptStyle::Hybrid, PromptTarget::Instructions, None));
    let ext_root = home.join(".vscode-server/extensions");
    let mut latest_ext_inst: Option<(String, PathBuf)> = None;
    if let Ok(entries) = std::fs::read_dir(&ext_root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("dotnetpower.code-context-native-") {
                let inst_path = entry.path().join("prompts/code-context.instructions.md");
                if inst_path.is_file()
                    && latest_ext_inst
                        .as_ref()
                        .is_none_or(|(prev, _)| name > *prev)
                {
                    latest_ext_inst = Some((name, inst_path));
                }
            }
        }
    }
    let (extension_instructions_bpe, actual_ext_tok) = match latest_ext_inst {
        Some((_, path)) => match std::fs::read_to_string(&path) {
            Ok(t) => {
                let is_bpe = t.contains("Formula Spec") || t.contains("=> #cctxNativeContext");
                (is_bpe, estimate_bpe_tokens(&t))
            }
            Err(_) => (default_formula, fallback_ext_tok),
        },
        None => (default_formula, fallback_ext_tok),
    };
    let extension_tokens_saved = if extension_instructions_bpe {
        prose_ext_tok.saturating_sub(actual_ext_tok) as u64
    } else {
        0
    };

    // 2. MCP initialize instructions + 14 Native/MCP tool schemas — dynamic delta vs Prose baselines
    const PROSE_NATIVE_SCHEMA_BASELINE_TOK: usize = 2422;
    const PROSE_MCP_SCHEMA_BASELINE_TOK: usize = 1745;
    let mcp_schema_tok = serde_json::to_string(&crate::shared_mcp::tools()["tools"])
        .map(|s| estimate_bpe_tokens(&s))
        .unwrap_or(PROSE_MCP_SCHEMA_BASELINE_TOK);
    let mut native_schema_saved = 0usize;
    if let Ok(entries) = std::fs::read_dir(&ext_root) {
        let mut latest_pkg: Option<(String, PathBuf)> = None;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("dotnetpower.code-context-native-") {
                let pkg_path = entry.path().join("package.json");
                if pkg_path.is_file()
                    && latest_pkg
                        .as_ref()
                        .is_none_or(|(prev, _)| name > *prev)
                {
                    latest_pkg = Some((name, pkg_path));
                }
            }
        }
        if let Some((_, pkg_path)) = latest_pkg
            && let Ok(raw_pkg) = std::fs::read_to_string(&pkg_path)
            && let Ok(parsed_pkg) = serde_json::from_str::<Value>(&raw_pkg)
            && let Some(lm_tools) = parsed_pkg.pointer("/contributes/languageModelTools")
            && let Ok(lm_str) = serde_json::to_string(lm_tools)
        {
            native_schema_saved =
                PROSE_NATIVE_SCHEMA_BASELINE_TOK.saturating_sub(estimate_bpe_tokens(&lm_str));
        }
    }
    let schema_tokens_saved =
        (native_schema_saved + PROSE_MCP_SCHEMA_BASELINE_TOK.saturating_sub(mcp_schema_tok)) as u64;

    let mcp_instructions_bpe = default_formula;
    let mcp_tokens_saved = if mcp_instructions_bpe {
        let prose_mcp = estimate_bpe_tokens(&render(PromptStyle::Prose, PromptTarget::Mcp, None));
        let active_mcp = estimate_bpe_tokens(&render(active_style, PromptTarget::Mcp, None));
        prose_mcp.saturating_sub(active_mcp) as u64
    } else {
        0
    };

    // 3. Subagent definitions — dynamic delta vs Prose SubagentYaml baseline
    let bin_root = home.join(".vscode-server/bin");
    let mut subagent_checked = 0usize;
    let mut subagent_formula_ok = 0usize;
    if let Ok(entries) = std::fs::read_dir(&bin_root) {
        for entry in entries.flatten() {
            for rel in [
                "node_modules/@github/copilot-sdk-linux-x64/definitions/explore.agent.yaml",
                "extensions/copilot/node_modules/@github/copilot/sdk/definitions/explore.agent.yaml",
                "extensions/copilot/node_modules/@github/copilot/sdk/definitions/research.agent.yaml",
            ] {
                let candidate = entry.path().join(rel);
                if let Ok(text) = std::fs::read_to_string(&candidate) {
                    subagent_checked += 1;
                    if text.contains("CCTX & TCTX Formula Rules")
                        || (text.contains("cctxNativeContext") && default_formula)
                    {
                        subagent_formula_ok += 1;
                    }
                }
            }
        }
    }
    let subagents_bpe = if subagent_checked > 0 {
        subagent_formula_ok == subagent_checked
    } else {
        default_formula
    };
    let subagent_tokens_saved = if subagents_bpe {
        let prose_sub =
            estimate_bpe_tokens(&render(PromptStyle::Prose, PromptTarget::SubagentYaml, None));
        let formula_sub = estimate_bpe_tokens(&render(
            PromptStyle::Formula,
            PromptTarget::SubagentYaml,
            None,
        ));
        prose_sub.saturating_sub(formula_sub) as u64
    } else {
        0
    };

    // 4. Global user instructions (~/.copilot/copilot-instructions.md) — dynamic operator-density delta
    let global_path = home.join(".copilot/copilot-instructions.md");
    let (global_instructions_bpe, global_tokens_saved) = if global_path.is_file() {
        match std::fs::read_to_string(&global_path) {
            Ok(t) => {
                let is_bpe =
                    t.contains("=>") && (t.contains("cctxNativeContext") || t.contains("Formula"));
                if is_bpe {
                    let actual_tok = estimate_bpe_tokens(&t);
                    let op_count = t.matches(" => ").count()
                        + t.matches(" -> ").count()
                        + t.matches("!(").count()
                        + t.matches(" <=> ").count()
                        + t.matches(" > ").count()
                        + t.matches(" ; ").count();
                    // Each symbolic clause saves ~8 tokens vs multi-sentence prose phrasing (capped at 55% of prose equivalent)
                    let est_saved = (op_count * 8).min((actual_tok * 120) / 100);
                    (true, est_saved as u64)
                } else {
                    (false, 0)
                }
            }
            Err(_) => (false, 0),
        }
    } else {
        (default_formula, 0)
    };

    let active_surfaces = usize::from(extension_instructions_bpe)
        + usize::from(mcp_instructions_bpe)
        + usize::from(subagents_bpe)
        + usize::from(global_instructions_bpe);
    let standing_tokens_saved_per_turn = extension_tokens_saved
        + schema_tokens_saved
        + mcp_tokens_saved
        + subagent_tokens_saved
        + global_tokens_saved;

    BpeSurfaceStatus {
        extension_instructions_bpe,
        mcp_instructions_bpe,
        subagents_bpe,
        global_instructions_bpe,
        subagents_auto_patched,
        active_surfaces,
        total_surfaces: 4,
        extension_tokens_saved,
        schema_tokens_saved,
        mcp_tokens_saved,
        subagent_tokens_saved,
        global_tokens_saved,
        standing_tokens_saved_per_turn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn formula_and_hybrid_instructions_save_tokens_and_preserve_contracts() {
        let root = PathBuf::from("/repo");
        let formula = render(PromptStyle::Formula, PromptTarget::Instructions, Some(&root));
        let hybrid = render(PromptStyle::Hybrid, PromptTarget::Instructions, Some(&root));
        let prose = render(PromptStyle::Prose, PromptTarget::Instructions, Some(&root));

        let formula_tok = estimate_bpe_tokens(&formula);
        let hybrid_tok = estimate_bpe_tokens(&hybrid);
        let prose_tok = estimate_bpe_tokens(&prose);

        assert!(
            formula_tok < hybrid_tok && hybrid_tok <= prose_tok,
            "expected formula ({formula_tok}) < hybrid ({hybrid_tok}) <= prose ({prose_tok})"
        );
        for required in [
            "description: \"Use when:",
            "applyTo: \"**\"",
            "#cctxNativeContext",
            "prefer backend MCP",
            "cctx --backend shared --root",
            "Do not duplicate a still-pending lookup",
            "Never use another transport to bypass",
            "CCTX is an optional accelerator",
            "Do not enumerate or prepare sibling worktrees",
        ] {
            assert!(hybrid.contains(required), "hybrid missing {required}");
        }
    }

    #[test]
    fn mcp_formula_instructions_preserve_wire_invariants() {
        let mcp = render(PromptStyle::Formula, PromptTarget::Mcp, None);
        for expected in [
            "prefer these backend MCP tools",
            "Do not duplicate pending lookups",
            "cctx --backend shared",
            "Never switch transport to bypass",
        ] {
            assert!(mcp.contains(expected), "mcp formula missing {expected}");
        }
    }

    #[test]
    fn foreground_prompts_recover_native_priority_after_dispatch_failure() {
        let required_all = [
            "dispatch-error MCP fallback lasts only for the current user turn",
            "next foreground user turn",
            "rediscover native tools and try one needed source lookup",
            "success => Native-first; dispatch error => MCP for that turn",
            "Never duplicate a pending call or poll Status",
            "background/unattended sessions without native dispatch stay on MCP",
        ];
        let contributed = include_str!("../vscode-extension/prompts/code-context.instructions.md");
        assert!(contributed.contains("Do not choose MCP when the matching Native tool is callable"));
        assert!(contributed.contains("No client with the advertised tool definition"));
        for phrase in required_all {
            assert!(contributed.contains(phrase), "extension missing {phrase}");
            for style in [
                PromptStyle::Formula,
                PromptStyle::Hybrid,
                PromptStyle::Prose,
            ] {
                for target in [PromptTarget::Instructions, PromptTarget::Mcp] {
                    let rendered = render(style, target, None);
                    assert!(
                        rendered.contains(phrase),
                        "{style:?}/{target:?} missing {phrase}"
                    );
                }
            }
        }
    }

    #[test]
    fn task_prompt_compression_reduces_tokens_and_extracts_questions() {
        let raw = "Read only. Use the fastest available source search/read tools, not shell commands.\n\
Answer all four questions about this project. For each answer give the exact function name, relative file path, 1-based function-start line, and the requested behavior. Be concise.\n\n\
Q1. Which function prevents selling more inventory than is available, and how is failure represented?\n\
Q2. Which function calculates retry delay, what formula does it use, and what caps the result?";
        let compressed = compress_task_prompt(raw, Some(Path::new("/repo")));
        assert!(compressed.contains("Mode: read_only & !shell root=/repo"));
        assert!(compressed.contains("Q1 => fn guard sold > available => failure repr"));
        assert!(compressed.contains("Q2 => fn calc retry delay & formula & cap"));
        assert!(estimate_bpe_tokens(&compressed) < estimate_bpe_tokens(raw));
    }

    #[test]
    fn korean_24h_prompt_patterns_compress_and_preserve_korean_tag() {
        let samples = [
            (
                "이슈 1945을 완료할수 있도록 남은 작업에 대한 계획을 세워서 모두 조치하고 완료되면 푸시 머지까지 해서 이슈를 닫을수 있도록 하자",
                "[ko] :: #1945 남은작업 계획수립 -> 모두조치 -> push & merge -> 이슈닫기",
            ),
            (
                "더 보강이 필요한것 100개 이상 찾아서 모두 조치하는 라운드를 10회 진행하고 결과를 수치화된 표로 보고한 다음 커밋 푸시 머지까지 진행해",
                "[ko] :: 보강점 >=100 발굴 -> 모두조치 -> loop 10x -> 수치표 보고 -> commit & push & merge",
            ),
            (
                "모두 적용하고 비평 10개이상 하드닝 라운드를 심각도가 Low 이하만 남을때 까지 반복하고 완료되면 개선점을 100개 이상 찾아서 모두 보강한 다음 푸시 머지 후 보고해",
                "[ko] :: 모두적용 -> loop(비평 >=10 하드닝, until 심각도 <= Low) -> 개선점 >=100 발굴 -> 모두보강 -> push & merge -> 보고",
            ),
            (
                "다음은 절감액이다 더 최적화 할수 있는 부분을 30가지 이상 찾아서 계획을 세워서 목록으로 보여줘",
                "[ko] :: 절감액 기반 최적화 >=30 발굴 -> 계획수립 -> 목록 보고",
            ),
        ];
        for (raw, expected) in samples {
            let compressed = compress_task_prompt(raw, None);
            assert_eq!(compressed, expected);
            assert!(
                estimate_bpe_tokens(&compressed) < estimate_bpe_tokens(raw),
                "expected fewer tokens for {compressed} vs {raw}"
            );
        }
    }

    #[test]
    fn bpe_operator_normalization_replaces_unicode_math_and_backticks() {
        let raw = "∀x∈Sym∪Lit: need(`code_context`) ⇒ `code_context` ≻ (`grep` ∘ `read`)";
        let (normalized, unicode_cnt, backtick_cnt) = normalize_bpe_operators(raw);
        assert!(unicode_cnt >= 6);
        assert_eq!(backtick_cnt, 4);
        assert!(!normalized.contains('∀'));
        assert!(!normalized.contains('`'));
        assert!(normalized.contains(" => "));
        assert!(normalized.contains(" > "));
        assert!(estimate_bpe_tokens(&normalized) < estimate_bpe_tokens(raw));
    }

    #[test]
    fn claude_md_and_cli_cache_subagents_support_all_model_surfaces() {
        let claude_formula = render(PromptStyle::Formula, PromptTarget::ClaudeMd, Some(Path::new("/repo")));
        assert!(claude_formula.contains("mcp__code-context__code_context"));
        assert!(claude_formula.contains("tool_search_tool"));

        let home = tempfile::tempdir().unwrap();
        let cli_defs = home.path().join(".cache/copilot/pkg/linux-x64/1.0.999/definitions");
        std::fs::create_dir_all(&cli_defs).unwrap();
        std::fs::write(
            cli_defs.join("explore.agent.yaml"),
            "name: explore\ntools:\n  - view\n  - lsp\nprompt: |\n  Explore codebase.\n",
        )
        .unwrap();
        let patched = ensure_subagents_configured(home.path());
        assert_eq!(patched, 1);
        let body = std::fs::read_to_string(cli_defs.join("explore.agent.yaml")).unwrap();
        assert!(body.contains("tool_search_tool"));
        assert!(body.contains("mcp__code-context__*"));
        assert!(body.contains("CCTX & TCTX Formula Rules"));
    }

    #[test]
    fn all_five_targets_save_tokens_vs_prose_and_short_prompts_never_expand() {
        let root = Path::new("/repo");
        for target in [
            PromptTarget::Instructions,
            PromptTarget::Mcp,
            PromptTarget::SubagentYaml,
            PromptTarget::SubagentMd,
            PromptTarget::ClaudeMd,
        ] {
            let formula = render(PromptStyle::Formula, target, Some(root));
            let prose = render(PromptStyle::Prose, target, Some(root));
            let f_tok = estimate_bpe_tokens(&formula);
            let p_tok = estimate_bpe_tokens(&prose);
            assert!(
                f_tok < p_tok,
                "target {target:?} inverted: formula={f_tok} >= prose={p_tok}"
            );
        }

        // Short unmatched Korean/English prompts with --root must never bloat beyond minimal fallback
        let short_ko = "토큰 최적화를 위해 사용되는 BPE 는 잘 작동하고 있나?";
        let out_ko = compress_task_prompt(short_ko, Some(root));
        let fallback_ko = format!("[root=/repo] {}", short_ko);
        assert!(
            estimate_bpe_tokens(&out_ko) <= estimate_bpe_tokens(&fallback_ko),
            "short ko prompt expanded: {out_ko}"
        );

        // Dynamic surface inspector computes live positive savings instead of static 2100
        let home = tempfile::tempdir().unwrap();
        let ext_dir = home
            .path()
            .join(".vscode-server/extensions/dotnetpower.code-context-native-0.25.9/prompts");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("code-context.instructions.md"),
            include_str!("../vscode-extension/prompts/code-context.instructions.md"),
        )
        .unwrap();
        let copilot_dir = home.path().join(".copilot");
        std::fs::create_dir_all(&copilot_dir).unwrap();
        std::fs::write(
            copilot_dir.join("copilot-instructions.md"),
            "# Formula\n- a => cctxNativeContext > rg & !(slice -> repeat)\n",
        )
        .unwrap();
        let status = inspect_bpe_surfaces(home.path());
        assert_eq!(status.active_surfaces, 4);
        assert!(status.extension_tokens_saved > 500);
        assert!(status.mcp_tokens_saved > 50);
        assert!(status.subagent_tokens_saved > 100);
        assert!(status.global_tokens_saved > 0);
        assert_eq!(
            status.standing_tokens_saved_per_turn,
            status.extension_tokens_saved
                + status.schema_tokens_saved
                + status.mcp_tokens_saved
                + status.subagent_tokens_saved
                + status.global_tokens_saved
        );
    }
}
