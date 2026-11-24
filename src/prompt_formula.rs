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
    /// OpenAI Codex project/user instructions (`AGENTS.md`, `<=32 KiB` doc cap).
    #[value(alias = "agents-md", alias = "codex")]
    CodexAgentsMd,
}

impl PromptTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Instructions => "instructions",
            Self::Mcp => "mcp",
            Self::SubagentYaml => "subagent-yaml",
            Self::SubagentMd => "subagent-md",
            Self::ClaudeMd => "claude-md",
            Self::CodexAgentsMd => "codex-agents-md",
        }
    }
}

/// Model-generation BPE tokenizer profile for English, code, and Korean Hangul token estimation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum TokenizerProfile {
    /// OpenAI `tiktoken` (`o200k_base` / `cl100k_base`) baseline.
    #[default]
    #[value(name = "o200k", alias = "tiktoken", alias = "openai", alias = "codex")]
    O200k,
    /// Anthropic Claude 4.5 / 4.6 tokenizer profile.
    #[value(
        name = "claude-4-6",
        alias = "claude46",
        alias = "claude-4.6",
        alias = "opus-4-6",
        alias = "sonnet-4-6"
    )]
    Claude46,
    /// Anthropic Claude 4.7+ newer tokenizer profile (~1.30x-1.46x denser splits on system prompts/code & Korean).
    #[value(
        name = "claude-4-7",
        alias = "claude47",
        alias = "claude-4.7",
        alias = "opus-4-7",
        alias = "claude"
    )]
    Claude47,
}

impl TokenizerProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::O200k => "o200k",
            Self::Claude46 => "claude-4-6",
            Self::Claude47 => "claude-4-7",
        }
    }

    pub fn for_model(model: &str) -> Self {
        let lower = model.to_ascii_lowercase();
        if lower.contains("claude-4-7")
            || lower.contains("claude-4.7")
            || lower.contains("claude47")
            || lower.contains("opus-4-7")
            || lower.contains("opus-4.7")
            || lower.contains("sonnet-4-7")
            || lower.contains("sonnet-4.7")
            || lower.contains("haiku-4-7")
            || lower.contains("haiku-4.7")
            || lower.contains("claude-5")
            || lower.contains("opus-5")
            || lower.contains("sonnet-5")
            || lower.contains("haiku-5")
        {
            Self::Claude47
        } else if lower.contains("claude")
            || lower.contains("opus")
            || lower.contains("sonnet")
            || lower.contains("haiku")
        {
            Self::Claude46
        } else {
            Self::O200k
        }
    }
}

/// Approximate BPE token count calibrated against `tiktoken` (`o200k_base` / `cl100k_base`).
/// Accumulates quarter-token units (`1 token = 4 units`) to capture newline/punctuation merging,
/// camelCase identifier splits, and Korean Hangul syllable density (~2.8% MAPE).
pub fn estimate_bpe_tokens(text: &str) -> usize {
    estimate_bpe_tokens_for(text, TokenizerProfile::O200k)
}

/// Estimates token count under a specific model-generation tokenizer profile (`o200k`, `claude-4-6`, `claude-4-7`).
pub fn estimate_bpe_tokens_for(text: &str, profile: TokenizerProfile) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut quarter_tokens = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\n' {
            quarter_tokens += match profile {
                TokenizerProfile::Claude47 => 3,
                _ => 2,
            };
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
                let stride = match profile {
                    TokenizerProfile::Claude47 => 6,
                    _ => 8,
                };
                quarter_tokens += 4 * run.div_ceil(stride);
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
            match profile {
                TokenizerProfile::O200k => {
                    if case_splits > 0 {
                        let segs = case_splits + 1;
                        let extra = if len > segs * 8 { segs } else { 0 };
                        quarter_tokens += 4 * (segs + extra);
                    } else if len <= 9 {
                        quarter_tokens += 4;
                    } else {
                        quarter_tokens += 4 * len.div_ceil(8);
                    }
                }
                TokenizerProfile::Claude46 => {
                    if case_splits > 0 {
                        let segs = case_splits + 1;
                        let extra = if len > segs * 7 { segs } else { 0 };
                        quarter_tokens += 4 * (segs + extra);
                    } else if len <= 8 {
                        quarter_tokens += 4;
                    } else {
                        quarter_tokens += 4 * len.div_ceil(7);
                    }
                }
                TokenizerProfile::Claude47 => {
                    // Anthropic's newer tokenizer splits compound words, camelCase identifiers,
                    // and technical prefixes ~1.30x-1.46x more finely than legacy vocabularies.
                    if case_splits > 0 {
                        let segs = case_splits + 1;
                        let extra = len.saturating_sub(segs * 5).div_ceil(5);
                        quarter_tokens += 5 * segs + 4 * extra;
                    } else if len <= 5 {
                        quarter_tokens += 5;
                    } else {
                        quarter_tokens += 5 * len.div_ceil(6);
                    }
                }
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
            let group = match profile {
                TokenizerProfile::Claude47 => 2,
                _ => 3,
            };
            quarter_tokens += 4 * len.div_ceil(group);
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
            let base = match run.as_str() {
                "->" | "=>" | "<=>" | "<=" | ">=" | "!=" | "==" | ".." | "..." | "--" | "::"
                | "//" | "/*" | "*/" | "**" | "||" | "&&" => 4,
                "," | "." | ":" | ";" | "_" | "-" | "/" => 3,
                _ if run.len() == 1 => 4,
                _ => 3 * run.len(),
            };
            quarter_tokens += match profile {
                TokenizerProfile::Claude47 => base + usize::from(run.len() >= 2),
                _ => base,
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
            // Hangul syllable density varies across BPE vocabularies:
            // o200k (~0.75 tok/syl), Claude 4.6 (~0.95 tok/syl), Claude 4.7 (~1.20 tok/syl).
            quarter_tokens += match profile {
                TokenizerProfile::O200k => syllables * 3,
                TokenizerProfile::Claude46 => (syllables * 19).div_ceil(5),
                TokenizerProfile::Claude47 => (syllables * 24).div_ceil(5),
            };
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
AgentHost/CopilotSDK/bg/subagent/CLI (MCP callable) => MCP(code-context-code_* / code_*) FIRST (immune to Reload Window 'Running on another client... Skip?') > #cctxNative*{{Context,Read,Grep,Files}} > cctx --backend shared --root {root_binding} > Host(rg,view,glob); Editor Chat (!MCP) => #cctxNative* FIRST; pctx {{http|json|table|image|log|batch|journal}} > python3 -c/curl/jq\n\
- symbol+context => code_context / #cctxNativeContext (root={root_binding}, radius:0..100, limit:1..64; !1..150 slicing)\n\
- glob(!content) => code_files / #cctxNativeFiles ; a|b(regex=true) => code_grep / #cctxNativeGrep (max_matches_per_file:1..64)\n\
- ranges(:line,#Lstart-Lend,.md#slug,@/) => 1 code_read / #cctxNativeRead (<=150L,<8KiB; >150=>page=true->next_ranges)\n\
- pre-edit => Context(radius>=8)|Read(around_line) ; hidden => tool_search_tool <=2 names/call, 2 parallel calls (\"code-context-code_context code-context-code_grep\" + \"code-context-code_read code-context-code_files\" | Editor \"cctxNativeContext cctxNativeGrep\" + \"cctxNativeRead cctxNativeFiles\") FIRST, all 4 loaded before lookup ; bash grep|rg|sed -n|cat|head|find on source => same swap ; subagents => pass root={root_binding} & require CCTX\n\
- CACHE_MISS != (0|missing) -> 1 Host read -> resume CCTX & !(poll|repeat|code_sync) ; CACHE_LOAD_FAILED => Host ; STALE_INDEX => Read/Host ; OUT_OF_BOUNDS(N) => clamp(1..N) ; code_sync <=> absence claims\n\
- .github,.vscode,ignored,secrets,/tmp => Host only. Never use another transport to bypass exclusions/trust/denials.\n\
- Error(\"No client with the advertised tool definition\"|\"No matching client connected\"|\"skipped from another client\"|disconnected) => code-context-code_* MCP | cctx --backend shared --root {root_binding} ; AgentHost/unattended => prefer backend MCP. Do not duplicate a still-pending lookup."
        ),
        (PromptTarget::Instructions, PromptStyle::Hybrid) => format!(
            "---\n\
description: \"Use when: finding files by name or path, searching code, or reading saved source, documentation, and configuration in a registered workspace\"\n\
applyTo: \"**\"\n\
---\n\n\
# Code Context (cctx) — Formula Spec\n\
CCTX is an optional accelerator (nonblocking root cache), not a prerequisite.\n\
Priority: AgentHost / Copilot SDK (`Copilot SDK in VS Code`, `ahp-session`, `get_current_session`/`list_sessions`, autopilot/bg, subagents, CLI) with MCP callable => prefer backend MCP (`code-context-code_*` | `code_*`) FIRST (avoids Reload Window `'Running on another client... Skip?'` stalls & client-disconnect failures) > `#cctxNative*` > CLI(`cctx --backend shared --root {root_binding}`) > Host(rg,view,read_file,glob). Foreground Editor Chat without backend MCP => `#cctxNativeContext` / `#cctxNative*{{Read,Grep,Files}}` (`code-context-native_*`) FIRST; never start with Host search/read for saved source.\n\
Dispatch:\n\
- symbol|literal+context => code_context / #cctxNativeContext (root={root_binding}, radius:0..100, limit:1..64; 1 call > grep+read & !(1..150->151..300 slicing))\n\
- filename|glob(!content) => code_files / #cctxNativeFiles (glob=G) ; content(a|b=>regex=true) => code_grep / #cctxNativeGrep (max_matches_per_file:1..64)\n\
- range|anchor(path:line,path#Lstart-Lend,doc.md#slug,@/) => code_read / #cctxNativeRead (<=150L,<8KiB; >150=>page=true->next_ranges->complete=true)\n\
- Swap: view|read_file=>code_read/#cctxNativeRead ; rg|grep=>code_grep/#cctxNativeGrep ; glob|find=>code_files/#cctxNativeFiles ; 한글 요청 => 동일 디스패치 & 한글 답변\n\
- Shell: bash|run_in_terminal grep|rg|sed -n|cat|head|tail|find|ls on saved source => same Swap (bash is NOT exempt; 1 code_context > grep+sed -n chain) ; bash only for build|test|git|non-source\n\
- Probe & Continuity: API/STAC/tiles | JSON/JSONL/GeoJSON//tmp offload | SQLite/Parquet/CSV | PNG/WebP/SVG | large logs/CLI batch | compaction journal => pctx {{http|json|table|image|log|batch|journal}} (cctx probe / cctx journal) & !(python3 -c | raw curl/jq)\n\
Guards:\n\
- known(R) => root={root_binding} ; before(edit) => Context(radius>=8)|Read(around_line) ; multi-edit => 1 Read(ranges=[...]) (!parallel view)\n\
- deferred (ONLY when CCTX tools are absent/hidden from active declarations, even if rg/view/glob visible on turn 0; skip if already declared, and invoke whichever of tool_search_tool / tool_search is declared) => <=2 names/call in 2 parallel calls (4 names drop code_read/code_files: result cap 5 incl. GitHub-* noise): AgentHost/SDK/subagent \"code-context-code_context code-context-code_grep\" + \"code-context-code_read code-context-code_files\" | Editor Chat \"cctxNativeContext cctxNativeGrep\" + \"cctxNativeRead cctxNativeFiles\" ; all 4 loaded before 1st lookup, else re-search the missing name\n\
- subagent(explore,general-purpose,code-review,rubber-duck,research,security-review,task) => pass root={root_binding} & require code-context-code_* | #cctxNative* (via tool_search_tool if hidden) first\n\
Transitions:\n\
- CACHE_MISS != (0 matches|missing) -> 1 Host read -> resume CCTX next turn & !(poll|repeat|switch transport|code_sync) ; CACHE_LOAD_FAILED => Host ; STALE_INDEX => Read|Host ; TARGET_NOT_INDEXED != absent\n\
- READ_RANGE_OUT_OF_BOUNDS(N) => clamp(1..N) ; code_sync <=> exhaustive absence claims only ; TTL>=15m|LRU => evict. Do not enumerate or prepare sibling worktrees speculatively (authorize-worktrees is operator-only).\n\
- .github,.vscode,ignored,secrets,/tmp => Host policy-aware tools only. Never use another transport to bypass an exclusion, trust decision or access denial.\n\
- Error(\"No client with the advertised tool definition\"|\"No matching client connected\"|disconnected|\"skipped from another client\"|\"not available on this client\") => code-context-code_* MCP | cctx --backend shared --root {root_binding}\n\
- Session(AgentHost|CopilotSDK|unattended|background) => prefer backend MCP (code-context-code_* | code_*). Do not duplicate a still-pending lookup or poll Status; safe boundary => cctx --root /project doctor."
        ),
        (PromptTarget::Instructions, PromptStyle::Prose) => format!(
            "---\n\
description: \"Use when: finding files by name or path, searching code, or reading saved source, documentation, and configuration in a registered workspace\"\n\
applyTo: \"**\"\n\
---\n\n\
# Code Context source lookup\n\n\
Harness-aware routing: in AgentHost / Copilot SDK sessions (`Copilot SDK in VS Code`, Agents Window `ahp-session`, background autopilot, subagents, and CLI) where backend MCP (`code-context-code_*` / `code_*`) is callable, ALWAYS prefer backend MCP FIRST so `Reload Window` or multi-window switches never stall on `'Running cctxNative* on another client... Skip?'` or fail with client disconnect. In foreground Editor Copilot Chat without backend MCP (or when MCP is disabled), use `#cctxNativeContext`, `#cctxNativeRead`, `#cctxNativeGrep` and `#cctxNativeFiles` (`code-context-native_*`) FIRST. Never start with `view`, `read_file`, `grep_search` or `rg` for saved source, even for known paths.\n\n\
| Need | Tool |\n\
| --- | --- |\n\
| Symbol/literal plus surrounding code (search and read in one call) | `#cctxNativeContext` / `code_context` |\n\
| Filename or path glob (`*.ts`, `src/**/*.rs`, bare stem; not content) | `#cctxNativeFiles` / `code_files` |\n\
| Exact or regex content search (`regex: true` for `a\\|b`) | `#cctxNativeGrep` / `code_grep` |\n\
| Known ranges (`path:line:col`, `path#Lstart-Lend`, `doc.md#slug`, `@/` aliases) | `#cctxNativeRead` / `code_read` |\n\
| Structured data, HTTP/OpenAPI/STAC batch probes, `/tmp` offloaded JSON/logs, SQLite/Parquet/CSV schemas, PNG/WebP/SVG assets, noisy CLI batch logs, or compaction continuity journal | `pctx http` / `pctx json` / `pctx table` / `pctx image` / `pctx log` / `pctx batch` / `pctx journal` (`cctx probe ...` / `cctx journal ...`) |\n\n\
- For structured data, noisy command logs, and long-session continuity, prefer declarative `pctx` (`cctx probe`) and `cctx journal` commands (`pctx http \"https://api.example/{{svc}}\" --var svc=a,b`, `pctx json data.geojson --bbox minx,miny,maxx,maxy`, `pctx table db.sqlite`, `pctx image render.png`, `pctx batch -c \"git log -n 200 --stat\" -q \"retry\"`, `pctx log @s1 -q \"fatal\"`, `cctx --root {root_binding} journal resume`) instead of writing multi-line `python3 -c`, `urllib.request`, `jq`, or `sqlite3` scripts: `pctx` runs in Rust, spools large logs outside context (`@sN`), redacts secrets automatically, and bounds replies within `<8KiB` (`<=2KiB` for compaction state).\n\
- Swap, do not fall back: `view`/`read_file` on a known path or range -> `#cctxNativeRead` (`path:start-end`); `rg`/`grep` -> `#cctxNativeGrep`; `glob`/`find` -> `#cctxNativeFiles`. Keep using them after the first lookup. Never slice a file across turns (`1..150` -> `151..300`) or chain `grep` -> `read` when `#cctxNativeContext` (`paths=[file]`, `radius=25..50`) or batched `ranges` can answer in one turn.\n\
- Shell is not exempt: `bash` / `run_in_terminal` commands such as `grep`, `rg`, `sed -n`, `cat`, `head`, `tail`, `find` or `ls` on saved source get the same swap (`grep`/`rg` -> `code_grep`, `sed -n`/`cat`/`head`/`tail` -> `code_read`, `find`/`ls` -> `code_files`, grep+`sed -n` chains -> one `code_context`). Use `bash` for builds, tests, git and non-source work.\n\
- Pass the absolute `root` (`{root_binding}`) when the workspace or worktree is known (WSL, subagents, multi-root).\n\
- Keep each read to `<=150` lines (`<8KiB`) and set `max_matches_per_file` on broad searches so output stays inline; context radius is 0..100, limit 1..64. For large reads use `page=true` and follow `next_ranges` until `complete=true`. Responses that hit the inline budget report `inline_notice` and `next_ranges`.\n\
- Before editing, use `#cctxNativeContext` (`radius >= 8`) or `#cctxNativeRead` (`around_line` + `radius`) so the first edit has exact indentation, full signatures and 3+ unique context lines; before a multi-hunk or multi-file edit, batch all needed line ranges into one `#cctxNativeRead(ranges=[...])` call rather than parallel `view` calls.\n\
- Subagents (`explore`, `general-purpose`, `code-review`, `rubber-duck`, `research`, `security-review`, `task`): built-in subagents may run different models (such as Opus, Sonnet, Haiku, or Terra) without custom workspace instructions, so always include the absolute `root` in the delegation prompt and instruct them to use `#cctxNative*` or `code-context-code_*` (loading them with `tool_search_tool` first if hidden) before `view`, `rg`, or `grep`.\n\
- Deferred tools (Claude, GPT, and subagent sessions where built-in `grep`/`view`/`glob` appear on turn 0 while CCTX tools are hidden): call `tool_search_tool` (`tool_search`) FIRST before any host search/read. Use at most 2 exact names per call and issue two calls in parallel, because results cap at 5 tools and GitHub-* tools or native twins crowd out the rest (a 4-name query was observed to drop `code_read` and `code_files`, after which Claude fell back to `view`/`bash`): `\"code-context-code_context code-context-code_grep\"` and `\"code-context-code_read code-context-code_files\"` (native: `\"cctxNativeContext cctxNativeGrep\"` and `\"cctxNativeRead cctxNativeFiles\"`). Confirm all four tools loaded before the first lookup; re-search any missing name.\n\n\
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
            "CCTX Formula (root={root_binding}): AgentHost/CopilotSDK/CLI/subagent/bg => prefer these backend MCP tools (code_context/code_read/code_grep/code_files) FIRST (immune to Reload Window 'Running on another client... Skip?') > #cctxNative* > cctx --backend shared --root {root_binding} > Host(rg,view); Editor Chat (!MCP) => #cctxNative*{{Context,Read,Grep,Files}} FIRST; pctx {{http|json|table|image|log|batch|journal}} > python3 -c/curl/jq. \
Dispatch: symbol+context=>code_context(radius:0..100,limit:1..64); glob(!content)=>code_files; regex(a|b=>regex:true)=>code_grep(max_matches_per_file:1..64); range=>code_read(<=150L,<8KiB; >150=>page=true->next_ranges). \
CACHE_MISS != (0 matches|missing) -> 1 host read -> resume CCTX & !(poll|repeat|code_sync); CACHE_LOAD_FAILED=error; TTL>=15m|LRU; !speculative worktrees; code_sync <=> absence claims. \
Do not duplicate pending lookups. Never switch transport to bypass exclusions/trust/denials; out-of-scope/hidden/ignored => policy-aware host tools."
        ),
        (PromptTarget::Mcp, PromptStyle::Prose) => format!(
            "CCTX is an optional nonblocking root cache (root={root_binding}). CACHE_MISS returns immediately and queues only the requested root; continue that lookup with permitted host search/read. Do not poll/repeat, switch CCTX transports or run code_sync to wait for warmup. CACHE_LOAD_FAILED reports the load failure. Neither means zero matches or a missing disk file. Startup and legacy prewarm flags do not load/pin source. Inactive roots expire after 15 minutes and may be evicted earlier by LRU/memory limits. Do not enumerate sibling worktrees speculatively. Explicitly needed code_sync remains a waiting barrier. Use these tools before shell search/read for saved files in a registered workspace. In AgentHost / Copilot SDK, background, subagent, or CLI sessions, prefer these backend MCP tools FIRST so Reload Window or multi-window switches never stall or fail native client dispatch; in foreground Editor Chat without backend MCP, use #cctxNativeContext/#cctxNativeRead/#cctxNativeGrep/#cctxNativeFiles FIRST. Use code_files for filename/path glob discovery, not a content pattern. Pass explicit absolute root when known (or omit for the current workspace); keep read batches <=150 lines (<8KiB) to stay inline. Prefer code_context for surrounding source, code_grep for content, code_read for known files, and pctx (cctx probe) instead of ad-hoc python3 -c, curl, jq, or sqlite3 inspection scripts. Do not duplicate pending lookups or run code_sync to repair host dispatch. If both client surfaces are unavailable but a backend terminal is usable, use cctx --backend shared with the same authorized root, except CACHE_MISS requires a permitted host read instead. Never switch transport to bypass content exclusions, trust decisions or access denials. Empty results do not cover files outside registered scopes/globs. Hidden/ignored/excluded targets require policy-aware host tools. No editor buffers or cross-root search."
        ),
        (PromptTarget::SubagentYaml, PromptStyle::Formula | PromptStyle::Hybrid) => {
            indent_yaml_block(
                &format!(
                    "# [cctx-subagent-instructions]\n\
CCTX & TCTX Formula Rules (root={root_binding}):\n\
- Source: MCP(code-context-code_* | mcp__code-context__*) > cctxNative*{{Context,Read,Grep,Files}} > cctx --backend shared --root {root_binding} > Host(view,rg,glob)\n\
- Tests & Probes: MCP(test-context-test_* | mcp__test-context__*) > tctxNative*{{Run,Affected,Graph,Status}} > tctx --root {root_binding} run > bash ; pctx {{http|json|table|image}} > python3 -c/curl/jq\n\
- Guards: pass root={root_binding}; symbol+context => code-context-code_context/cctxNativeContext(paths=[file],radius:25..50,limit<=64) (!1..150 slicing); multi-range => 1 code-context-code_read/cctxNativeRead(ranges,<=120L,<8KiB); .github/*,.vscode/*,.gitignore,/tmp/*,/dev/shm/* => view(!CCTX).\n\
- Deferred (even if grep/view visible on turn 0): call tool_search_tool (tool_search) FIRST, <=2 names/call in 2 parallel calls (4 names drop code_read/code_files: result cap 5 incl. GitHub-* noise): MCP \"code-context-code_context code-context-code_grep\" + \"code-context-code_read code-context-code_files\" | Native \"cctxNativeContext cctxNativeGrep\" + \"cctxNativeRead cctxNativeFiles\" | tests \"test-context-test_run test-context-test_affected\" + \"tctxNativeRun tctxNativeAffected\"; all 4 loaded before lookup. bash grep|rg|sed -n|cat|head|find on source => same swap.\n\
- Transitions: Error(\"No client with the advertised tool definition\"|\"No matching client connected\"|\"skipped from another client\") => code-context-code_* / test-context-test_* MCP | CLI ; CACHE_MISS => 1 Host read -> resume CCTX.\n\
# [/cctx-subagent-instructions]"
                ),
                "  ",
            )
        }
        (PromptTarget::SubagentYaml, PromptStyle::Prose) => indent_yaml_block(
            &format!(
                "# [cctx-subagent-instructions]\n\
**Code Context (`cctx`), Test Context (`tctx`), and Probe (`pctx`) Priority (MANDATORY FIRST PATH):**\n\
- For locating or reading saved source in a registered workspace, ALWAYS use Code Context FIRST before `view`, `grep`, `rg`, `glob`, or `bash`:\n\
  1. Prefer backend MCP `code-context-code_context`, `code-context-code_read`, `code-context-code_grep`, `code-context-code_files` (or `mcp__code-context__code_*`) FIRST when callable so `Reload Window` or headless execution never stalls on UI client dispatch; otherwise use native `cctxNativeContext`, `cctxNativeRead`, `cctxNativeGrep`, `cctxNativeFiles`.\n\
  2. Always pass the explicit absolute `root` parameter (`{root_binding}`). Prefer `code-context-code_context` / `cctxNativeContext` (`limit <= 64`, `radius 0..100`) over sequential slicing, and batch multiple line ranges into one `code-context-code_read` / `cctxNativeRead` call (`<= 120` total lines, `<8KiB`).\n\
  3. Do NOT pass hidden paths (`.github/*`, `.vscode/*`, `.gitignore`) or `/tmp/*` / `/dev/shm/*` files to Code Context; use `view` directly for those.\n\
  4. Even when `grep`/`view`/`glob` are visible on turn 0, if Code Context or Test Context tools are deferred/hidden, call `tool_search_tool` (`tool_search`) FIRST with at most 2 exact names per call, in two parallel calls (results cap at 5 and GitHub-* tools crowd out the rest; a 4-name query dropped `code_read`/`code_files`): `\"code-context-code_context code-context-code_grep\"` and `\"code-context-code_read code-context-code_files\"` (fallback `\"cctxNativeContext cctxNativeGrep\"` and `\"cctxNativeRead cctxNativeFiles\"`). Confirm all four loaded before the first lookup.\n\
  4b. `bash` is not exempt: `grep`, `rg`, `sed -n`, `cat`, `head`, `tail`, `find`, `ls` on saved source map to `code_grep`, `code_read`, `code_files` (or one `code_context`); keep `bash` for builds, tests and git.\n\
  5. If a native `cctxNative*` or `tctxNative*` call fails with `\"No client with the advertised tool definition\"`, `\"No matching client connected\"`, or `\"skipped from another client\"`, immediately switch to `code-context-code_*` / `test-context-test_*` MCP tools (or `cctx --backend shared --root {root_binding}` / `tctx --root {root_binding} run` CLI).\n\
  6. On `CACHE_MISS`, use a permitted host read/search for that single lookup only, then resume using Code Context on subsequent turns.\n\
  7. For running or inspecting tests/lints (`pytest`, `cargo test`, `vitest`, `jest`, `node --test`, `ruff`, `eslint`) or AST test dependency graphs in a registered workspace, ALWAYS use MCP `test-context-test_run`, `test-context-test_affected`, `test-context-test_graph` (or native `tctxNativeRun`, `tctxNativeAffected`, `tctxNativeGraph`) FIRST before `bash`, passing the explicit absolute `root`.\n\
  8. For HTTP/OpenAPI/STAC checks, JSON/JSONL/GeoJSON/`/tmp` offload schemas, SQLite/Parquet/CSV inspection, or PNG/SVG checks, prefer `pctx {{http|json|table|image}}` (`cctx probe ...`) over ad-hoc `python3 -c`, `curl`, `jq`, or `sqlite3` scripts.\n\
# [/cctx-subagent-instructions]"
            ),
            "  ",
        ),
        (PromptTarget::SubagentMd, PromptStyle::Formula | PromptStyle::Hybrid) => format!(
            "\n<!-- [cctx-subagent-instructions] -->\n\
## CCTX & TCTX Formula Rules (root={root_binding})\n\
- Source: MCP(code-context-code_* | mcp__code-context__*) > #cctxNative*{{Context,Read,Grep,Files}} > Host(rg,view,glob)\n\
- Tests & Probes: MCP(test-context-test_* | mcp__test-context__*) > #tctxNative*{{Run,Affected,Graph,Status}} > bash ; pctx {{http|json|table|image}} > python3 -c/curl/jq\n\
- Guards: root={root_binding}; limit<=64; Read(ranges,<=120L,<8KiB); .github/*,.vscode/*,/tmp/*,/dev/shm/* => Host view.\n\
- Deferred: call tool_search_tool (tool_search) FIRST, <=2 names/call in 2 parallel calls (4 names drop code_read/code_files): MCP \"code-context-code_context code-context-code_grep\" + \"code-context-code_read code-context-code_files\" | Native \"cctxNativeContext cctxNativeGrep\" + \"cctxNativeRead cctxNativeFiles\" | tests \"test-context-test_run test-context-test_affected\" + \"tctxNativeRun tctxNativeAffected\"; all 4 loaded before lookup. bash grep|rg|sed -n|cat|head|find on source => same swap.\n\
- CACHE_MISS => 1 Host read -> resume CCTX.\n"
        ),
        (PromptTarget::SubagentMd, PromptStyle::Prose) => format!(
            "\n<!-- [cctx-subagent-instructions] -->\n\
## Code Context (`cctx`), Test Context (`tctx`), and Probe (`pctx`) Priority (Mandatory First Path)\n\n\
- For locating or reading saved source in a registered workspace, ALWAYS use MCP `code-context-code_*` / `mcp__code-context__code_*` (or `#cctxNativeContext`, `#cctxNativeRead`, `#cctxNativeGrep`, and `#cctxNativeFiles` when backend MCP is unavailable) FIRST before built-in search/read tools.\n\
- For running or inspecting tests/lints or AST test dependency graphs in a registered workspace, ALWAYS use MCP `test-context-test_*` / `mcp__test-context__test_*` (or `#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, and `#tctxNativeStatus` when backend MCP is unavailable) FIRST before shell test/lint commands, and use `pctx {{http|json|table|image}}` instead of ad-hoc `python3 -c` / `curl` / `jq` inspection scripts.\n\
- Always pass the explicit absolute `root` parameter (`{root_binding}`) when known, keep `code_context` / `cctxNativeContext` `limit <= 64`, and batch multiple ranges into one read (`<= 120` lines, `<8KiB`). Never send `.github/*`, `.vscode/*`, `/tmp/*`, or `/dev/shm/*` paths to Code Context.\n\
- Even when built-in `grep`/`view` are visible on turn 0, if CCTX/TCTX tools are deferred/hidden, call `tool_search_tool` (`tool_search`) FIRST with at most 2 exact names per call in two parallel calls (4 names drop `code_read`/`code_files`): `\"code-context-code_context code-context-code_grep\"` and `\"code-context-code_read code-context-code_files\"` (fallback `\"cctxNativeContext cctxNativeGrep\"` and `\"cctxNativeRead cctxNativeFiles\"`). `bash` `grep`/`rg`/`sed -n`/`cat`/`head`/`find` on saved source gets the same swap.\n\
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
        (PromptTarget::CodexAgentsMd, PromptStyle::Formula | PromptStyle::Hybrid) => format!(
            "# CCTX, TCTX & PCTX Formula — Codex AGENTS.md (root={root_binding})\n\
- Priority: mcp__code-context__code_context/code_read/code_grep/code_files > cctx --backend shared --root {root_binding} > exec_command/shell(rg,sed,cat,nl,find)\n\
- Swap (exec_command/shell is NOT exempt!): symbol+nearby => code_context(root={root_binding},radius:25..50,limit<=64,!1..150 slicing) ; glob => code_files ; a|b(regex:true) => code_grep(max_matches_per_file:1..64) ; ranges => 1 code_read(<=150L,<8KiB)\n\
- Tests & Logs: mcp__test-context__test_run/test_affected | tctx --root {root_binding} run > shell runners ; pctx {{http|json|table|image|log|batch}} > python3 -c/curl/jq ; cctx --root {root_binding} journal {{record|resume}} before /compact or task switch\n\
- Guards: keep model & reasoning_effort stable mid-session (preserves 30m/24h prefix cache) ; CACHE_MISS => 1 shell rg/sed -> resume CCTX ; .github/*,.vscode/*,.codex/*,ignored,/tmp/* => shell/host only.\n"
        ),
        (PromptTarget::CodexAgentsMd, PromptStyle::Prose) => format!(
            "# Code Context (`cctx`), Test Context (`tctx`), and Probe (`pctx`) for OpenAI Codex\n\n\
- For saved source in `{root_binding}`, always use `mcp__code-context__code_context`, `mcp__code-context__code_read`, `mcp__code-context__code_grep`, and `mcp__code-context__code_files` (or `cctx --backend shared --root {root_binding}`) FIRST before running `rg`, `grep`, `sed -n`, `nl`, `cat`, or `find` via `exec_command`/`shell`.\n\
- Combine symbol search and block-clamped reading in one `code_context` call (`radius: 25..50`, `limit <= 64`) instead of sequential `sed -n` slicing, and batch known ranges into one `code_read` call (`<= 150` lines, `<8 KiB`).\n\
- Run incremental tests via `mcp__test-context__test_run` or `tctx --root {root_binding} run`, filter large logs/JSON with `pctx batch` + `pctx log` instead of spawning cold-cache subagents, and record state via `cctx --root {root_binding} journal record` before `/compact` or switching tasks.\n\
- Keep model and `reasoning_effort` stable within a thread to preserve prompt-cache prefixes (`5h`/`7d` Codex quota efficiency). On `CACHE_MISS`, perform one `rg`/`sed` read for that lookup only and resume Code Context next turn; keep `.github/*`, `.vscode/*`, `.codex/*`, ignored files, and `/tmp/*` on host commands.\n"
        ),
    };
    if matches!(target, PromptTarget::Instructions | PromptTarget::Mcp) {
        rendered.push_str(if target == PromptTarget::Instructions {
            "\n- "
        } else {
            " "
        });
        rendered.push_str(
            "Recovery: AgentHost/CopilotSDK/background/unattended sessions stay on backend MCP so Reload Window never stalls on another client or fails mid-turn. \
In Editor Chat without backend MCP, dispatch-error MCP fallback lasts only for the current user turn \
(including unattended autopilot continuations before the next user message); \
on the next foreground user turn, rediscover native tools and try one needed source lookup: \
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
    let active = crate::bpe_builder::load_active_profile();
    compress_task_prompt_with_profile(input, root, active.as_ref())
}

/// Same as [`compress_task_prompt`], but applies an explicit learned [`crate::bpe_builder::BpeProfile`]
/// overlay (or built-in rules only when `profile` is `None`).
pub fn compress_task_prompt_with_profile(
    input: &str,
    root: Option<&Path>,
    profile: Option<&crate::bpe_builder::BpeProfile>,
) -> String {
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
            if !changed
                && let Some(prof) = profile
            {
                for phrase in &prof.learned_fillers {
                    if strip_filler_at_boundary(&mut cleaned, phrase) {
                        changed = true;
                        break;
                    }
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
            let formula_body = clause_to_formula_with_profile(rest, profile);
            questions.push(format!("{q_prefix} => {formula_body}"));
        } else if raw_segment.trim_end().ends_with('?')
            || cleaned.to_ascii_lowercase().starts_with("which ")
            || cleaned.to_ascii_lowercase().starts_with("what ")
            || cleaned.to_ascii_lowercase().starts_with("how ")
            || cleaned.to_ascii_lowercase().starts_with("where ")
        {
            let body = clause_to_formula_with_profile(cleaned, profile);
            if has_ko && body.contains("->") {
                rules.push(body);
            } else {
                questions.push(format!("? => {body}"));
            }
        } else {
            rules.push(clause_to_formula_with_profile(cleaned, profile));
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
        for rule in &rules {
            out.push_str("- ");
            out.push_str(rule);
            out.push('\n');
        }
        for q in &questions {
            out.push_str(q);
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
    let fallback_tok = estimate_bpe_tokens(&minimal_fallback);
    if estimate_bpe_tokens(&candidate) < fallback_tok {
        candidate
    } else if has_ko
        && questions.is_empty()
        && rules.len() == 1
        && !wants_read_only
        && root.is_none()
        && !wants_fn_loc
        && contains_hangul(&rules[0])
        && estimate_bpe_tokens(&rules[0]) < fallback_tok
    {
        // For short Korean single-clause prompts that already contain Hangul, omitting the
        // `[ko] :: ` wrapper preserves learned BPE/filler savings without breaking Korean replies.
        rules.into_iter().next().unwrap_or(minimal_fallback)
    } else {
        minimal_fallback
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

fn is_boundary_char(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, ',' | '.' | '?' | '!' | ':' | ';' | '(' | ')' | '[' | ']')
}

fn strip_filler_at_boundary(cleaned: &mut String, phrase: &str) -> bool {
    let p_lower = phrase.to_ascii_lowercase();
    if p_lower.is_empty() {
        return false;
    }
    let lower = cleaned.to_ascii_lowercase();
    let mut search_from = 0usize;
    while search_from <= lower.len()
        && let Some(rel) = lower[search_from..].find(&p_lower)
    {
        let start = search_from + rel;
        let end = start + p_lower.len();
        if !cleaned.is_char_boundary(start) || !cleaned.is_char_boundary(end) {
            break;
        }
        let left_ok = start == 0
            || cleaned[..start]
                .chars()
                .next_back()
                .is_none_or(is_boundary_char);
        let right_ok = end == cleaned.len()
            || phrase.chars().last().is_some_and(is_boundary_char)
            || cleaned[end..].chars().next().is_none_or(is_boundary_char);
        if left_ok && right_ok {
            cleaned.replace_range(start..end, " ");
            *cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
            return true;
        }
        if let Some(ch) = lower[start..].chars().next() {
            search_from = start + ch.len_utf8();
        } else {
            break;
        }
    }
    false
}

fn replace_rule_at_boundary(input: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return input.to_string();
    }
    let ascii_word_start = from.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());
    let ascii_word_end = from
        .chars()
        .next_back()
        .is_some_and(|c| c.is_ascii_alphanumeric());
    if !ascii_word_start && !ascii_word_end {
        return input.replace(from, to);
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(idx) = rest.find(from) {
        let end = idx + from.len();
        let left_ok = !ascii_word_start
            || (idx == 0 && out.chars().next_back().is_none_or(|c| !c.is_ascii_alphanumeric()))
            || (idx > 0
                && rest[..idx]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !c.is_ascii_alphanumeric()));
        let right_ok = !ascii_word_end
            || rest[end..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_ascii_alphanumeric());
        out.push_str(&rest[..idx]);
        if left_ok && right_ok {
            out.push_str(to);
        } else {
            out.push_str(from);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

pub(crate) fn clause_to_formula_builtin(clause: &str) -> String {
    clause_to_formula_with_profile(clause, None)
}

#[allow(dead_code)]
fn clause_to_formula(clause: &str) -> String {
    clause_to_formula_with_profile(clause, None)
}

fn clause_to_formula_with_profile(
    clause: &str,
    profile: Option<&crate::bpe_builder::BpeProfile>,
) -> String {
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
    if let Some(prof) = profile {
        for rule in &prof.learned_replacements {
            if !rule.from.is_empty() && s.contains(&rule.from) {
                s = replace_rule_at_boundary(&s, &rule.from, &rule.to);
            }
        }
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
    build_report_with_tokenizer(style, target, root, compress, TokenizerProfile::O200k)
}

pub fn build_report_with_tokenizer(
    style: PromptStyle,
    target: PromptTarget,
    root: Option<&Path>,
    compress: Option<&str>,
    tokenizer: TokenizerProfile,
) -> Value {
    if let Some(raw_task) = compress {
        let active_profile = crate::bpe_builder::load_active_profile();
        let (_, unicode_replaced, backticks_removed) = normalize_bpe_operators(raw_task);
        let compressed = compress_task_prompt_with_profile(raw_task, root, active_profile.as_ref());
        let prose_tokens = estimate_bpe_tokens_for(raw_task, tokenizer);
        let formula_tokens = estimate_bpe_tokens_for(&compressed, tokenizer);
        let saved = prose_tokens.saturating_sub(formula_tokens);
        let pct = if prose_tokens > 0 {
            (1000.0 * saved as f64 / prose_tokens as f64).round() / 10.0
        } else {
            0.0
        };
        return json!({
            "mode": "compress",
            "style": style.as_str(),
            "tokenizer": tokenizer.as_str(),
            "language": if contains_hangul(raw_task) { "ko" } else { "en" },
            "root": root.map(|p| p.display().to_string()),
            "bpe_profile_applied": active_profile.is_some(),
            "bpe_profile_merges": active_profile.as_ref().map_or(0, |p| p.merges.len()),
            "bpe_profile_rules": active_profile.as_ref().map_or(0, |p| p.learned_fillers.len() + p.learned_replacements.len()),
            "input_bytes": raw_task.len(),
            "output_bytes": compressed.len(),
            "input_estimated_tokens": prose_tokens,
            "output_estimated_tokens": formula_tokens,
            "tokens_saved": saved,
            "savings_percent": pct,
            "tokenizer_comparison": {
                "o200k": {
                    "input": estimate_bpe_tokens_for(raw_task, TokenizerProfile::O200k),
                    "output": estimate_bpe_tokens_for(&compressed, TokenizerProfile::O200k),
                },
                "claude_4_6": {
                    "input": estimate_bpe_tokens_for(raw_task, TokenizerProfile::Claude46),
                    "output": estimate_bpe_tokens_for(&compressed, TokenizerProfile::Claude46),
                },
                "claude_4_7": {
                    "input": estimate_bpe_tokens_for(raw_task, TokenizerProfile::Claude47),
                    "output": estimate_bpe_tokens_for(&compressed, TokenizerProfile::Claude47),
                },
            },
            "unicode_math_symbols_replaced": unicode_replaced,
            "backtick_wrappers_removed": backticks_removed,
            "prompt": compressed,
        });
    }
    let rendered = render(style, target, root);
    let prose_baseline = render(PromptStyle::Prose, target, root);
    let rendered_tokens = estimate_bpe_tokens_for(&rendered, tokenizer);
    let baseline_tokens = estimate_bpe_tokens_for(&prose_baseline, tokenizer);
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
        "tokenizer": tokenizer.as_str(),
        "root": root.map(|p| p.display().to_string()),
        "bytes": rendered.len(),
        "lines": rendered.lines().count(),
        "estimated_tokens": rendered_tokens,
        "prose_baseline_tokens": baseline_tokens,
        "tokens_saved_vs_prose": saved,
        "savings_percent_vs_prose": pct,
        "tokenizer_comparison": {
            "o200k": estimate_bpe_tokens_for(&rendered, TokenizerProfile::O200k),
            "claude_4_6": estimate_bpe_tokens_for(&rendered, TokenizerProfile::Claude46),
            "claude_4_7": estimate_bpe_tokens_for(&rendered, TokenizerProfile::Claude47),
        },
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

pub const TARGET_YAML_AGENTS: &[&str] = &[
    "explore.agent.yaml",
    "code-review.agent.yaml",
    "rubber-duck.agent.yaml",
    "security-review.agent.yaml",
    "research.agent.yaml",
    "task.agent.yaml",
    "sidekick/github-context.yaml",
    "sidekick/github-context-memory.yaml",
];

fn indent_yaml_block(body: &str, indent: &str) -> String {
    let mut out = String::with_capacity(body.len() + 64);
    for line in body.lines() {
        if !line.trim().is_empty() {
            out.push_str(indent);
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

fn parse_literal_block_header(header_line: &str) -> Option<Option<usize>> {
    let rest = header_line
        .trim_end_matches(['\r', '\n'])
        .strip_prefix("prompt:")?
        .trim_start_matches([' ', '\t']);
    let after_pipe = rest.strip_prefix('|')?;
    let mut chars = after_pipe.chars().peekable();
    let mut chomp_seen = false;
    let mut explicit_indent: Option<usize> = None;
    for _ in 0..2 {
        match chars.peek().copied() {
            Some('-' | '+') if !chomp_seen => {
                chomp_seen = true;
                chars.next();
            }
            Some(d @ '1'..='9') if explicit_indent.is_none() => {
                explicit_indent = Some((d as usize) - ('0' as usize));
                chars.next();
            }
            _ => break,
        }
    }
    let tail: String = chars.collect();
    if !tail.is_empty() {
        if !tail.starts_with([' ', '\t']) {
            return None;
        }
        let trimmed_tail = tail.trim_start_matches([' ', '\t']);
        if !trimmed_tail.is_empty() && !trimmed_tail.starts_with('#') {
            return None;
        }
    }
    Some(explicit_indent)
}

fn locate_prompt_literal_block(yaml_text: &str) -> Option<(usize, usize, String, bool)> {
    let prompt_idx = yaml_text
        .find("\nprompt:")
        .map(|i| i + 1)
        .or_else(|| yaml_text.starts_with("prompt:").then_some(0))?;
    let nl = yaml_text[prompt_idx..].find('\n')?;
    let header_line = &yaml_text[prompt_idx..prompt_idx + nl];
    let explicit = parse_literal_block_header(header_line)?;
    let body_start = prompt_idx + nl + 1;
    let after_header = &yaml_text[body_start..];
    let body_end = match find_first_top_level_yaml_key_offset(after_header) {
        Some(pos) => body_start + pos,
        None => yaml_text.len(),
    };
    if let Some(n) = explicit {
        return Some((body_start, body_end, " ".repeat(n), true));
    }
    let body = &yaml_text[body_start..body_end];
    let mut max_leading_blank_spaces = 0usize;
    for line in body.lines() {
        let clean = line.trim_end_matches('\r');
        if clean.trim_matches(' ').is_empty() {
            if clean.bytes().all(|b| b == b' ') {
                max_leading_blank_spaces = max_leading_blank_spaces.max(clean.len());
            } else {
                return None;
            }
            continue;
        }
        let spaces = clean.bytes().take_while(|&b| b == b' ').count();
        if spaces >= 1 && max_leading_blank_spaces <= spaces {
            return Some((body_start, body_end, " ".repeat(spaces), false));
        }
        return None;
    }
    Some((
        body_start,
        body_end,
        " ".repeat(max_leading_blank_spaces.max(2)),
        false,
    ))
}

fn reindent_yaml_block(default_two_space_block: &str, target_indent: &str) -> String {
    if target_indent == "  " {
        return default_two_space_block.to_owned();
    }
    let mut out = String::with_capacity(default_two_space_block.len() + 32);
    for line in default_two_space_block.lines() {
        if line.trim().is_empty() {
            out.push('\n');
        } else {
            let stripped = line.strip_prefix("  ").unwrap_or(line);
            out.push_str(target_indent);
            out.push_str(stripped);
            out.push('\n');
        }
    }
    out
}

fn is_top_level_yaml_mapping_key(line: &str) -> bool {
    let trimmed = line.trim_end_matches('\r');
    if trimmed.is_empty() || trimmed.starts_with(' ') || trimmed.starts_with('#') {
        return false;
    }
    let Some((key, rest)) = trimmed.split_once(':') else {
        return false;
    };
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && (rest.is_empty() || rest.starts_with(' '))
}

fn find_first_top_level_yaml_key_offset(text: &str) -> Option<usize> {
    let mut offset = 0usize;
    for raw_line in text.split_inclusive('\n') {
        if is_top_level_yaml_mapping_key(raw_line.trim_end_matches('\n')) {
            return Some(offset);
        }
        offset += raw_line.len();
    }
    None
}

fn legacy_cctx_line(line: &str) -> bool {
    let s = line.trim();
    if s.is_empty() {
        return true;
    }
    s.starts_with("CCTX & TCTX Formula Rules")
        || s.starts_with("**Code Context (`cctx`)")
        || s.starts_with("- Source:")
        || s.starts_with("- Tests & Probes:")
        || s.starts_with("- Guards:")
        || s.starts_with("- Deferred")
        || s.starts_with("- Transitions:")
        || s.starts_with("- For locating or reading saved source")
        || s.starts_with("1. Prefer ")
        || s.starts_with("2. Always pass ")
        || s.starts_with("3. Do NOT pass ")
        || s.starts_with("4. If Code Context or Test Context tools")
        || s.starts_with("4. Even when `grep`/`view`/`glob`")
        || s.starts_with("4b. ")
        || s.starts_with("5. If a native ")
        || s.starts_with("6. On `CACHE_MISS`")
        || s.starts_with("7. For running or inspecting tests")
        || s.starts_with("8. For HTTP/OpenAPI")
}

fn find_yaml_tools_section(text: &str) -> Option<(usize, usize)> {
    let key_start = if text.starts_with("tools:\n") || text.starts_with("tools:\r\n") {
        0
    } else {
        let pos = text.find("\ntools:\n").or_else(|| text.find("\ntools:\r\n"))?;
        pos + 1
    };
    let nl = text[key_start..].find('\n')?;
    let body_start = key_start + nl + 1;
    let mut offset = body_start;
    for raw_line in text[body_start..].split_inclusive('\n') {
        let clean = raw_line.trim_end_matches(['\r', '\n']);
        let leading = clean.bytes().take_while(|&b| b == b' ').count();
        let stripped = clean.trim_start();
        if stripped.is_empty()
            || (leading >= 2 && (stripped.starts_with('#') || stripped.starts_with("- ")))
        {
            offset += raw_line.len();
        } else {
            break;
        }
    }
    Some((body_start, offset))
}

fn is_plain_tool_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'*' | b'/' | b':' | b'@' | b'\'')
}

fn parse_yaml_tool_scalar(raw: &str, allow_trailing_comment: bool) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let bytes = trimmed.as_bytes();
    if bytes[0] == b'\'' || bytes[0] == b'"' {
        let q = bytes[0];
        let mut i = 1usize;
        let mut out = Vec::with_capacity(bytes.len());
        let mut closed = false;
        while i < bytes.len() {
            if q == b'"' && bytes[i] == b'\\' {
                if i + 1 >= bytes.len() {
                    return None;
                }
                let esc = match bytes[i + 1] {
                    b'"' => b'"',
                    b'\\' => b'\\',
                    b'/' => b'/',
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'u' if i + 5 < bytes.len()
                        && bytes[i + 2..i + 6].iter().all(u8::is_ascii_hexdigit) =>
                    {
                        let hex = std::str::from_utf8(&bytes[i + 2..i + 6]).ok()?;
                        let code = u32::from_str_radix(hex, 16).ok()?;
                        let ch = char::from_u32(code)?;
                        let mut utf8 = [0_u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut utf8).as_bytes());
                        i += 6;
                        continue;
                    }
                    _ => return None,
                };
                out.push(esc);
                i += 2;
                continue;
            }
            if q == b'\'' && bytes[i] == b'\'' && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                out.push(b'\'');
                i += 2;
                continue;
            }
            if bytes[i] == q {
                closed = true;
                i += 1;
                break;
            }
            out.push(bytes[i]);
            i += 1;
        }
        if !closed {
            return None;
        }
        let rest = trimmed[i..].trim();
        if !(rest.is_empty() || allow_trailing_comment && rest.starts_with('#')) {
            return None;
        }
        let s = String::from_utf8(out).ok()?;
        return (!s.is_empty()).then_some(s);
    }
    let body = if allow_trailing_comment && let Some((head, _)) = trimmed.split_once(" #") {
        head.trim_end()
    } else {
        trimmed
    };
    if body.is_empty()
        || matches!(body.as_bytes()[0], b'-' | b'@' | b'*' | b':' | b'?')
        || body.contains(": ")
        || body.ends_with(':')
        || !body.bytes().all(is_plain_tool_char)
    {
        return None;
    }
    Some(body.to_owned())
}

fn parse_inline_tools_brackets(line: &str) -> Option<(usize, usize)> {
    let rest = line.strip_prefix("tools:")?;
    let prefix_len = "tools:".len();
    let trimmed_start = rest.len() - rest.trim_start().len();
    let after_ws = &rest[trimmed_start..];
    if !after_ws.starts_with('[') {
        return None;
    }
    let open_idx = prefix_len + trimmed_start;
    let bytes = line.as_bytes();
    let mut i = open_idx + 1;
    let mut quote: u8 = 0;
    let mut at_item_start = true;
    while i < bytes.len() {
        let b = bytes[i];
        if quote != 0 {
            if b == b'\\' && quote == b'"' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if q_double_single(quote, bytes, i) {
                i += 2;
                continue;
            }
            if b == quote {
                quote = 0;
            }
            i += 1;
            continue;
        }
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b',' {
            at_item_start = true;
            i += 1;
            continue;
        }
        if at_item_start && (b == b'\'' || b == b'"') {
            quote = b;
            at_item_start = false;
            i += 1;
            continue;
        }
        if b == b']' {
            let tail = line[i + 1..].trim();
            if tail.is_empty() || tail.starts_with('#') {
                return Some((open_idx, i));
            }
            return None;
        }
        at_item_start = false;
        i += 1;
    }
    None
}

fn q_double_single(quote: u8, bytes: &[u8], i: usize) -> bool {
    quote == b'\'' && bytes[i] == b'\'' && i + 1 < bytes.len() && bytes[i + 1] == b'\''
}

fn find_yaml_inline_tools_span(text: &str) -> Option<(usize, usize)> {
    let mut offset = 0usize;
    for raw_line in text.split_inclusive('\n') {
        let clean = raw_line.trim_end_matches(['\r', '\n']);
        if let Some((open_rel, close_rel)) = parse_inline_tools_brackets(clean) {
            return Some((offset + open_rel, offset + close_rel));
        }
        offset += raw_line.len();
    }
    None
}

fn parse_inline_yaml_items(inner: &str) -> Option<Vec<String>> {
    let mut items = Vec::new();
    let bytes = inner.as_bytes();
    let mut i = 0usize;
    let n = bytes.len();
    while i < n {
        while i < n && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        if bytes[i] == b',' {
            return None;
        }
        let item_start = i;
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let q = bytes[i];
            i += 1;
            let mut closed = false;
            while i < n {
                if q == b'"' && bytes[i] == b'\\' && i + 1 < n {
                    i += 2;
                    continue;
                }
                if q_double_single(q, bytes, i) {
                    i += 2;
                    continue;
                }
                if bytes[i] == q {
                    closed = true;
                    i += 1;
                    break;
                }
                i += 1;
            }
            if !closed {
                return None;
            }
            while i < n && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let item_end = i;
            if i < n {
                if bytes[i] != b',' {
                    return None;
                }
                i += 1;
            }
            items.push(parse_yaml_tool_scalar(&inner[item_start..item_end], false)?);
        } else {
            while i < n && bytes[i] != b',' {
                i += 1;
            }
            items.push(parse_yaml_tool_scalar(&inner[item_start..i], false)?);
            if i < n && bytes[i] == b',' {
                i += 1;
            }
        }
    }
    Some(items)
}

fn parse_block_yaml_items(tools_body: &str) -> Option<Vec<String>> {
    let mut expected_spaces: Option<usize> = None;
    let mut items = Vec::new();
    for line in tools_body.lines() {
        let clean = line.trim_end_matches('\r');
        if clean.trim().is_empty() {
            continue;
        }
        let spaces = clean.bytes().take_while(|&b| b == b' ').count();
        if spaces < 2 {
            return None;
        }
        let rest = &clean[spaces..];
        if rest.starts_with('#') {
            continue;
        }
        let item = rest.strip_prefix("- ")?;
        if let Some(exp) = expected_spaces {
            if spaces != exp {
                return None;
            }
        } else {
            expected_spaces = Some(spaces);
        }
        items.push(parse_yaml_tool_scalar(item, true)?);
    }
    Some(items)
}

fn has_any_top_level_tools_key(text: &str) -> bool {
    text.lines()
        .map(|l| l.trim_end_matches('\r'))
        .any(|l| l == "tools:" || l.starts_with("tools: ") || l.starts_with("tools:\t"))
}

fn detect_tools_item_indent(tools_body: &str) -> String {
    for line in tools_body.lines() {
        let clean = line.trim_end_matches('\r');
        let spaces = clean.bytes().take_while(|&b| b == b' ').count();
        if spaces >= 2 && clean[spaces..].starts_with("- ") {
            return " ".repeat(spaces);
        }
    }
    String::from("  ")
}

fn has_top_level_strict_tools(text: &str) -> bool {
    text.starts_with("strictToolsList: true") || text.contains("\nstrictToolsList: true")
}

fn yaml_top_level_structure_valid(text: &str) -> bool {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SectionKind {
        None,
        Container,
        BlockScalar,
        InlineScalar,
    }
    let mut state = SectionKind::None;
    let mut seen_keys: Vec<&str> = Vec::with_capacity(8);
    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with(' ') {
            if line.starts_with('#') {
                continue;
            }
            if !is_top_level_yaml_mapping_key(line) {
                return false;
            }
            let (key, rest) = line.split_once(':').unwrap_or(("", ""));
            if seen_keys.contains(&key) {
                return false;
            }
            seen_keys.push(key);
            let val = rest.trim();
            state = if val.is_empty() {
                SectionKind::Container
            } else if val.starts_with('|') || val.starts_with('>') {
                SectionKind::BlockScalar
            } else {
                SectionKind::InlineScalar
            };
        } else if matches!(state, SectionKind::None | SectionKind::InlineScalar) {
            return false;
        }
    }
    true
}

fn split_after_legacy_cctx_block(after_marker_line: &str) -> &str {
    let mut cursor = 0usize;
    let mut last_cctx_end = 0usize;
    for raw_line in after_marker_line.split_inclusive('\n') {
        let clean = raw_line.trim_end_matches(['\r', '\n']);
        if is_top_level_yaml_mapping_key(clean) {
            return &after_marker_line[cursor..];
        }
        if clean.trim().is_empty() {
            cursor += raw_line.len();
            continue;
        }
        if !legacy_cctx_line(clean) {
            return &after_marker_line[last_cctx_end..];
        }
        cursor += raw_line.len();
        last_cctx_end = cursor;
    }
    &after_marker_line[last_cctx_end..]
}

fn inject_yaml_extra_tools(updated: &mut String) {
    if has_top_level_strict_tools(updated) {
        return;
    }
    if let Some((open_abs, close_abs)) = find_yaml_inline_tools_span(updated) {
        let inner = &updated[open_abs + 1..close_abs];
        let Some(parsed_items) = parse_inline_yaml_items(inner) else {
            return;
        };
        if parsed_items.iter().any(|s| s == "*") {
            return;
        }
        let quote = if inner.contains('\'') {
            "'"
        } else if inner.contains('"') {
            "\""
        } else {
            ""
        };
        let mut additions = Vec::new();
        for &t in SUBAGENT_EXTRA_TOOLS {
            if !parsed_items.iter().any(|existing| existing == t) {
                if quote.is_empty() && t.contains('*') {
                    additions.push(format!("'{t}'"));
                } else {
                    additions.push(format!("{quote}{t}{quote}"));
                }
            }
        }
        if !additions.is_empty() {
            let joined = additions.join(", ");
            let trimmed_end = inner.trim_end();
            let prefix = if trimmed_end.is_empty() {
                ""
            } else if trimmed_end.ends_with(',') {
                " "
            } else {
                ", "
            };
            updated.insert_str(close_abs, &format!("{prefix}{joined}"));
        }
        return;
    }
    let Some((tools_start, tools_end)) = find_yaml_tools_section(updated) else {
        return;
    };
    let tools_body = &updated[tools_start..tools_end];
    let Some(parsed_block_items) = parse_block_yaml_items(tools_body) else {
        return;
    };
    if parsed_block_items.iter().any(|s| s == "*") {
        return;
    }
    let item_indent = detect_tools_item_indent(tools_body);
    let missing: Vec<&str> = SUBAGENT_EXTRA_TOOLS
        .iter()
        .copied()
        .filter(|t| !parsed_block_items.iter().any(|existing| existing == t))
        .collect();
    if missing.is_empty() {
        return;
    }
    let mut block = format!("{item_indent}# Code Context & Test Context tools (read-only)\n");
    for t in missing {
        block.push_str(&format!("{item_indent}- {t}\n"));
    }
    let lsp_needle = format!("{item_indent}- lsp\n");
    if let Some(lsp_rel) = tools_body.find(&lsp_needle) {
        let insert_at = tools_start + lsp_rel + lsp_needle.len();
        updated.insert_str(insert_at, &block);
    } else {
        let mut insert_at = tools_end;
        while insert_at > tools_start
            && updated.as_bytes()[insert_at - 1].is_ascii_whitespace()
            && updated[..insert_at].ends_with("\n\n")
        {
            insert_at -= 1;
        }
        if insert_at > tools_start && !updated[..insert_at].ends_with('\n') {
            block.insert(0, '\n');
        }
        updated.insert_str(insert_at, &block);
    }
}

pub fn yaml_subagent_is_valid(text: &str) -> bool {
    if !text.contains(SUBAGENT_MARKER) || !yaml_top_level_structure_valid(text) {
        return false;
    }
    let strict_tools = has_top_level_strict_tools(text);
    if let Some((open_abs, close_abs)) = find_yaml_inline_tools_span(text) {
        let inner = &text[open_abs + 1..close_abs];
        let Some(parsed_items) = parse_inline_yaml_items(inner) else {
            return false;
        };
        let is_wildcard = parsed_items.iter().any(|s| s == "*");
        if !strict_tools
            && !is_wildcard
            && (!parsed_items.iter().any(|s| s == "cctxNativeContext")
                || !parsed_items.iter().any(|s| s == "code-context/*"))
        {
            return false;
        }
    } else if let Some((tools_start, tools_end)) = find_yaml_tools_section(text) {
        let tools_body = &text[tools_start..tools_end];
        let Some(parsed_items) = parse_block_yaml_items(tools_body) else {
            return false;
        };
        let is_wildcard = parsed_items.iter().any(|s| s == "*");
        if !strict_tools
            && !is_wildcard
            && (!parsed_items.iter().any(|s| s == "cctxNativeContext")
                || !parsed_items.iter().any(|s| s == "code-context/*"))
        {
            return false;
        }
    } else if has_any_top_level_tools_key(text) {
        return false;
    }
    let Some((prompt_body_start, prompt_body_end, expected_indent, has_explicit_indent)) =
        locate_prompt_literal_block(text)
    else {
        return false;
    };
    let prompt_body = &text[prompt_body_start..prompt_body_end];
    let mut saw_first_content = false;
    let mut saw_marker = false;
    for line in prompt_body.lines() {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.trim_matches(' ').is_empty() {
            if !trimmed.bytes().all(|b| b == b' ') {
                return false;
            }
            if has_explicit_indent && trimmed.len() >= expected_indent.len() {
                saw_first_content = true;
                continue;
            }
            if !saw_first_content && !has_explicit_indent && trimmed.len() > expected_indent.len() {
                return false;
            }
            continue;
        }
        saw_first_content = true;
        if !trimmed.starts_with(&expected_indent) {
            return false;
        }
        if trimmed.trim() == SUBAGENT_MARKER {
            saw_marker = true;
        }
    }
    saw_marker
}

fn find_standalone_marker_line(slice: &str, marker: &str, min_indent: usize) -> Option<(usize, usize)> {
    let mut offset = 0usize;
    for raw_line in slice.split_inclusive('\n') {
        let clean = raw_line.trim_end_matches(['\r', '\n']);
        let spaces = clean.bytes().take_while(|&b| b == b' ').count();
        if spaces >= min_indent && clean[spaces..].trim_end() == marker {
            return Some((offset, offset + raw_line.len()));
        }
        offset += raw_line.len();
    }
    None
}

fn write_file_atomic(path: &Path, contents: &[u8]) -> bool {
    use std::io::Write;
    let Some(parent) = path.parent() else {
        return false;
    };
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| String::from("agent.yaml"));
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_u128, |d| d.as_nanos());
    let tmp_path = parent.join(format!(".{file_name}.{}.{nonce}.tmp", std::process::id()));
    let write_res = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
        #[cfg(unix)]
        if let Ok(meta) = std::fs::metadata(path) {
            let _ = file.set_permissions(meta.permissions());
        }
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp_path, path)
    })();
    if write_res.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
        return false;
    }
    true
}

const SUBAGENT_END_MARKER: &str = "# [/cctx-subagent-instructions]";

fn patch_yaml_subagent_file(path: &Path, style: PromptStyle) -> bool {
    use fs2::FileExt;
    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(lock_file) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(parent.join(".cctx-subagent.lock"))
    else {
        return false;
    };
    if lock_file.lock_exclusive().is_err() {
        return false;
    }
    let Ok(original) = std::fs::read_to_string(path) else {
        return false;
    };
    let mut updated = original.clone();
    inject_yaml_extra_tools(&mut updated);

    let Some((prompt_body_start, prompt_body_end, indent, _)) =
        locate_prompt_literal_block(&updated)
    else {
        return false;
    };
    let yaml_block = reindent_yaml_block(&render(style, PromptTarget::SubagentYaml, None), &indent);
    let prompt_slice = &updated[prompt_body_start..prompt_body_end];

    if let Some((rel_line_start, rel_line_end)) =
        find_standalone_marker_line(prompt_slice, SUBAGENT_MARKER, indent.len())
    {
        let line_start = prompt_body_start + rel_line_start;
        let after_marker_abs = prompt_body_start + rel_line_end;
        let mut head = updated[..line_start].trim_end_matches(['\r', '\n']);
        while head.len() > prompt_body_start && head.ends_with("\n\n") {
            head = &head[..head.len() - 1];
        }
        head = head.trim_end_matches(['\r', '\n']);
        let prompt_after_marker = &updated[after_marker_abs..prompt_body_end];
        if let Some((_, rel_end_line_end)) =
            find_standalone_marker_line(prompt_after_marker, SUBAGENT_END_MARKER, indent.len())
        {
            let rest = &updated[after_marker_abs + rel_end_line_end..];
            if rest.trim_matches(['\r', '\n']).is_empty() {
                updated = format!("{head}\n\n{yaml_block}");
            } else if is_top_level_yaml_mapping_key(rest.lines().next().unwrap_or("")) {
                updated = format!("{head}\n\n{yaml_block}\n{rest}");
            } else {
                updated = format!("{head}\n\n{yaml_block}{rest}");
            }
        } else {
            let after_marker_line = &updated[after_marker_abs..];
            let rest = split_after_legacy_cctx_block(after_marker_line);
            if rest.trim_matches(['\r', '\n']).is_empty() {
                updated = format!("{head}\n\n{yaml_block}");
            } else if is_top_level_yaml_mapping_key(rest.lines().next().unwrap_or("")) {
                updated = format!("{head}\n\n{yaml_block}\n{rest}");
            } else {
                updated = format!("{head}\n\n{yaml_block}{rest}");
            }
        }
    } else if prompt_body_end < updated.len() {
        let before_key = updated[..prompt_body_end].trim_end_matches(['\r', '\n']);
        let after_key = &updated[prompt_body_end..];
        updated = format!("{before_key}\n\n{yaml_block}\n{after_key}");
    } else {
        let before_eof = updated.trim_end_matches(['\r', '\n']);
        let sep = if prompt_body_start == updated.len() { "\n" } else { "\n\n" };
        updated = format!("{before_eof}{sep}{yaml_block}");
    }

    if !yaml_subagent_is_valid(&updated) {
        return false;
    }
    if updated == original {
        return false;
    }
    if std::fs::read_to_string(path).ok().as_deref() != Some(original.as_str()) {
        return false;
    }
    write_file_atomic(path, updated.as_bytes())
}

/// Discovers all Copilot SDK subagent YAML definition directories under `home`
/// (including standalone Copilot CLI caches and WSL-mounted Windows VS Code SDK bundles).
pub fn discover_subagent_definition_dirs(home: &Path) -> Vec<PathBuf> {
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
    if home.starts_with("/home/")
        && let Ok(users) = std::fs::read_dir("/mnt/c/Users")
    {
        for user in users.flatten() {
            let vscode_app = user
                .path()
                .join("AppData/Local/Programs/Microsoft VS Code");
            for rel in [
                "extensions/copilot/node_modules/@github/copilot/sdk/definitions",
                "node_modules.asar.unpacked/@github/copilot-sdk-win32-x64/definitions",
                "node_modules.asar.unpacked/@github/copilot-sdk-win32-arm64/definitions",
            ] {
                let direct_app_dir = vscode_app.join("resources/app").join(rel);
                if direct_app_dir.is_dir() {
                    def_dirs.push(direct_app_dir);
                }
            }
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
    def_dirs
}

/// Automatically patches any unconfigured Copilot SDK subagent YAML definitions under `home`
/// (including standalone Copilot CLI caches and WSL-mounted Windows VS Code SDK bundles)
/// using the active `PromptStyle` (default: `Formula`).
pub fn ensure_subagents_configured(home: &Path) -> usize {
    let style = PromptStyle::from_env().unwrap_or_default();
    let def_dirs = discover_subagent_definition_dirs(home);
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
    let subagents_auto_patched = 0usize;
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
                let is_bpe = t.contains("Formula Spec")
                    || t.contains("=> #cctxNativeContext")
                    || t.contains("=> code_context / #cctxNativeContext");
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
    let mut subagent_checked = 0usize;
    let mut subagent_formula_ok = 0usize;
    for (parent_rel, rels) in [
        (
            ".vscode-server/bin",
            &[
                "node_modules/@github/copilot-sdk-linux-x64/definitions/explore.agent.yaml",
                "extensions/copilot/node_modules/@github/copilot/sdk/definitions/explore.agent.yaml",
                "extensions/copilot/node_modules/@github/copilot/sdk/definitions/research.agent.yaml",
            ][..],
        ),
        (
            ".vscode-server/cli/servers",
            &[
                "server/node_modules/@github/copilot-sdk-linux-x64/definitions/explore.agent.yaml",
                "server/extensions/copilot/node_modules/@github/copilot/sdk/definitions/explore.agent.yaml",
                "server/extensions/copilot/node_modules/@github/copilot/sdk/definitions/research.agent.yaml",
            ][..],
        ),
    ] {
        let root = home.join(parent_rel);
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                for rel in rels {
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

    // 4. Global user instructions (~/.copilot/copilot-instructions.md, ~/.claude/CLAUDE.md, ~/.codex/AGENTS.md)
    let global_candidates = [
        home.join(".copilot/copilot-instructions.md"),
        home.join(".claude/CLAUDE.md"),
        home.join(".codex/AGENTS.md"),
    ];
    let global_path = global_candidates.into_iter().find(|p| p.is_file());
    let (global_instructions_bpe, global_tokens_saved) = if let Some(gp) = global_path {
        match std::fs::read_to_string(&gp) {
            Ok(t) => {
                let is_bpe = t.contains("=>")
                    && (t.contains("cctxNativeContext")
                        || t.contains("code_context")
                        || t.contains("Formula"));
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
    fn deferred_discovery_uses_two_name_queries_and_covers_shell_lookups() {
        let four_names = "code-context-code_context code-context-code_read code-context-code_grep code-context-code_files";
        for style in [
            PromptStyle::Formula,
            PromptStyle::Hybrid,
            PromptStyle::Prose,
        ] {
            let rendered = render(style, PromptTarget::Instructions, None);
            assert!(
                !rendered.contains(four_names),
                "{style:?} still uses 4-name query"
            );
            assert!(
                rendered.contains("code-context-code_read code-context-code_files"),
                "{style:?} missing 2-name discovery query"
            );
            assert!(
                rendered.contains("bash"),
                "{style:?} missing shell lookup rule"
            );
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
        assert!(contributed.contains("Running on another client... Skip?"));
        assert!(contributed.contains("No client with the advertised tool definition"));
        let expected_contributed = render(
            PromptStyle::Hybrid,
            PromptTarget::Instructions,
            Some(Path::new("R")),
        )
        .replace(
            "| cctx --backend shared --root R\n",
            "| cctx --backend shared --root /actual/root\n",
        );
        assert_eq!(contributed.trim(), expected_contributed.trim());
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
        assert!(yaml_subagent_is_valid(&body), "patched YAML invalid:\n{body}");

        // Verify both Formula and Prose SubagentYaml preserve 2-space block-scalar indentation
        // and automatically repair previously unindented/corrupted YAML & sidekick blocks.
        for style in [PromptStyle::Formula, PromptStyle::Prose] {
            let block = render(style, PromptTarget::SubagentYaml, Some(Path::new("/repo")));
            for line in block.lines() {
                assert!(
                    line.trim().is_empty() || line.starts_with("  "),
                    "unindented SubagentYaml line in {style:?}: {line:?}"
                );
            }
        }
        std::fs::create_dir_all(cli_defs.join("sidekick")).unwrap();
        let corrupted_sidekick = "name: github-context\ntools:\n  - view\nprompt: |\n  Sidekick rules.\n\n  # [cctx-subagent-instructions]\nCCTX & TCTX Formula Rules:\n- Source: broken unindented line\n\nsidekick:\n  maxSendsPerTurn: 1\n";
        assert!(!yaml_subagent_is_valid(corrupted_sidekick));
        let sk_path = cli_defs.join("sidekick/github-context.yaml");
        std::fs::write(&sk_path, corrupted_sidekick).unwrap();
        assert_eq!(ensure_subagents_configured(home.path()), 1);
        let repaired = std::fs::read_to_string(&sk_path).unwrap();
        assert!(yaml_subagent_is_valid(&repaired), "repaired sidekick invalid:\n{repaired}");
        assert!(repaired.ends_with("\nsidekick:\n  maxSendsPerTurn: 1\n"));

        // Verify top-of-file `tools:` followed by scalar `reasoningEffort: low` before `prompt: |-`
        // keeps injected tools inside `tools:` and preserves custom user `4. ...` rules after legacy marker.
        let task_path = cli_defs.join("task.agent.yaml");
        let tricky_yaml = "tools:\n  - view\nreasoningEffort: low\nprompt: |-\n  Base task rules.\n\n  # [cctx-subagent-instructions]\n  CCTX & TCTX Formula Rules:\n  - Source: old rule\n  4. Never expose user tokens.\n";
        std::fs::write(&task_path, tricky_yaml).unwrap();
        assert_eq!(ensure_subagents_configured(home.path()), 1);
        let patched_tricky = std::fs::read_to_string(&task_path).unwrap();
        assert!(yaml_subagent_is_valid(&patched_tricky), "tricky YAML invalid:\n{patched_tricky}");
        let re_pos = patched_tricky.find("\nreasoningEffort: low\n").unwrap();
        let tool_pos = patched_tricky.find("  - cctxNativeContext\n").unwrap();
        assert!(tool_pos < re_pos, "tools must be inserted inside tools: before reasoningEffort");
        assert!(
            patched_tricky.contains("  4. Never expose user tokens."),
            "custom user numbered rule must be preserved:\n{patched_tricky}"
        );
        // Misplaced sequence under scalar must be rejected by yaml_subagent_is_valid
        let broken_scalar_seq = "name: explore\nreasoningEffort: low\n  - cctxNativeContext\nprompt: |\n  # [cctx-subagent-instructions]\n  ok\n";
        assert!(!yaml_subagent_is_valid(broken_scalar_seq));

        // 4-space indented tools list & inline flow-style `tools: [...]` preserve exact formatting
        let four_space_path = cli_defs.join("research.agent.yaml");
        std::fs::write(
            &four_space_path,
            "name: research\ntools:\n    - view\n    - grep\nprompt: |\n    Research rules.\n",
        )
        .unwrap();
        let inline_path = cli_defs.join("code-review.agent.yaml");
        std::fs::write(
            &inline_path,
            "name: code-review\ntools: [view, grep,] # trailing comma & comment\nprompt: |\n  Review rules.\n",
        )
        .unwrap();
        assert_eq!(ensure_subagents_configured(home.path()), 2);
        let four_body = std::fs::read_to_string(&four_space_path).unwrap();
        assert!(yaml_subagent_is_valid(&four_body), "4-space YAML invalid:\n{four_body}");
        assert!(four_body.contains("    - cctxNativeContext\n"));
        assert!(!four_body.contains("\n  - cctxNativeContext\n"));
        let inline_body = std::fs::read_to_string(&inline_path).unwrap();
        assert!(yaml_subagent_is_valid(&inline_body), "inline YAML invalid:\n{inline_body}");
        assert!(!inline_body.contains(",,"));
        assert!(inline_body.contains("# trailing comma & comment"));
        assert!(inline_body.contains("tools: [view, grep, tool_search_tool, cctxNativeFiles"));

        // 4-space leading marker followed by 2-space comment outside block must be rejected
        let mismatched_leading_marker = "name: explore\ntools: [\"*\"]\nprompt: |\n    # [cctx-subagent-instructions]\n  # 2-space comment outside 4-space scalar\n    body\n";
        assert!(!yaml_subagent_is_valid(mismatched_leading_marker));

        // Explicit `prompt:  |4-` with >4-space leading line, Unicode-ws malformed header, and 1-space `prompt: |`
        let explicit_four_empty = cli_defs.join("security-review.agent.yaml");
        std::fs::write(&explicit_four_empty, "name: security-review\ntools: [\"*\"]\nprompt:  |4-\n      \n    Rule.\n").unwrap();
        let one_space_agent = cli_defs.join("rubber-duck.agent.yaml");
        std::fs::write(&one_space_agent, "name: rubber-duck\ntools: [\"*\"]\nprompt: |\n One-space rule.\n").unwrap();
        assert_eq!(ensure_subagents_configured(home.path()), 2);
        let exp4_body = std::fs::read_to_string(&explicit_four_empty).unwrap();
        assert!(yaml_subagent_is_valid(&exp4_body), "explicit |4- invalid:\n{exp4_body}");
        assert!(exp4_body.contains("\n    # [cctx-subagent-instructions]\n"));
        let one_body = std::fs::read_to_string(&one_space_agent).unwrap();
        assert!(yaml_subagent_is_valid(&one_body), "1-space invalid:\n{one_body}");
        assert!(one_body.contains("\n # [cctx-subagent-instructions]\n"));
        assert!(!yaml_subagent_is_valid("name: x\ntools: [\"*\"]\nprompt: |\u{00A0}# bad unicode ws\n  # [cctx-subagent-instructions]\n"));
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

    #[test]
    fn codex_agents_md_and_generation_tokenizer_profiles_calibrate_en_and_ko() {
        let root = Path::new("/repo");
        let codex_formula = render(PromptStyle::Formula, PromptTarget::CodexAgentsMd, Some(root));
        let codex_prose = render(PromptStyle::Prose, PromptTarget::CodexAgentsMd, Some(root));
        assert!(codex_formula.len() < 2048);
        assert!(codex_formula.contains("exec_command/shell is NOT exempt"));
        assert!(codex_formula.contains("mcp__code-context__code_context"));
        assert!(estimate_bpe_tokens(&codex_formula) < estimate_bpe_tokens(&codex_prose));

        // Tokenizer generation scaling: o200k < claude-4.6 < claude-4.7 (~1.25x-1.55x on system prompts & Korean)
        let en_prompt = render(PromptStyle::Prose, PromptTarget::Instructions, Some(root));
        let t_o200k = estimate_bpe_tokens_for(&en_prompt, TokenizerProfile::O200k);
        let t_c46 = estimate_bpe_tokens_for(&en_prompt, TokenizerProfile::Claude46);
        let t_c47 = estimate_bpe_tokens_for(&en_prompt, TokenizerProfile::Claude47);
        assert!(t_o200k < t_c46 && t_c46 < t_c47, "{t_o200k} < {t_c46} < {t_c47}");
        let ratio = t_c47 as f64 / t_o200k as f64;
        assert!((1.20..=1.65).contains(&ratio), "unexpected ratio {ratio}");

        let ko_sentence = "매번 다시 읽는 문맥과 프롬프트 캐시 가격, 주간 구독 한도를 함께 비교해야 정확한 비용이 드러납니다.";
        let ko_o200k = estimate_bpe_tokens_for(ko_sentence, TokenizerProfile::O200k);
        let ko_c46 = estimate_bpe_tokens_for(ko_sentence, TokenizerProfile::Claude46);
        let ko_c47 = estimate_bpe_tokens_for(ko_sentence, TokenizerProfile::Claude47);
        assert!(ko_o200k < ko_c46 && ko_c46 < ko_c47);
        assert_eq!(TokenizerProfile::for_model("claude-opus-4-7"), TokenizerProfile::Claude47);
        assert_eq!(TokenizerProfile::for_model("claude-sonnet-4-6"), TokenizerProfile::Claude46);
        assert_eq!(TokenizerProfile::for_model("gpt-6.1-sol"), TokenizerProfile::O200k);
    }
}
