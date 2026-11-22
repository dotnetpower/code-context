//! Failure, offload, built-in fallback and cost-guard analysis for `cctx usage`.

use crate::diagnostics::usage_display::{Style, UsageDisplay};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Sessions longer than this re-read a large cached prefix on every turn.
pub const LONG_SESSION_TURNS: usize = 150;
const SPEND_CONCENTRATION_SHARE: f64 = 0.25;
const REASONING_SHARE_ALERT: f64 = 0.30;
const OFFLOAD_RATE_ALERT: f64 = 0.05;
const CACHE_MISS_RATE_ALERT: f64 = 0.02;
const MIN_MODEL_TURNS: usize = 20;
const MIN_WORKSPACE_LOOKUPS: usize = 20;
const LOW_ADOPTION_PERCENT: f64 = 60.0;
const MIN_SEQUENTIAL_SLICES: usize = 10;
const SEQUENTIAL_SLICE_SHARE_ALERT: f64 = 0.10;

/// Calls slower than this are treated as stalled for stall and host-delay analysis.
pub const SLOW_CALL_MS: f64 = 5_000.0;
/// Slow calls from different sessions that finish within this window share one stall.
const STALL_WINDOW_SECS: f64 = 1.5;
const HOST_DELAY_ALERT_MS: f64 = 2_000.0;

#[derive(Clone, Debug, Default)]
pub struct CallFacts {
    pub session: String,
    pub completed_at: Option<f64>,
    /// Time outside the extension and daemon (host dispatch), known only when the result carries trace timing.
    pub host_delay_ms: Option<f64>,
    pub workspace: String,
    pub tool: String,
    /// `native`, `mcp`, `builtin_search_read`, `shell_search_read` or `other`.
    pub group: &'static str,
    pub success: Option<bool>,
    pub offloaded: bool,
    pub error_detail: &'static str,
    pub duration_ms: Option<f64>,
    pub trace_total_ms: Option<f64>,
    pub payload_tokens: u64,
    pub eligible_lookup: bool,
    pub sequential_slice: bool,
    pub unregistered_root: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct SessionFacts {
    pub id: String,
    pub workspace: String,
    pub turns: usize,
    pub used_aic: f64,
}

#[derive(Clone, Debug, Default)]
pub struct ModelFacts {
    pub name: String,
    pub turns: usize,
    pub lookups: usize,
    pub cctx_calls: usize,
    pub used_aic: f64,
    pub saved_aic: f64,
    pub cold_turns: usize,
    pub cold_excess_aic: f64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Clone, Debug, Default)]
pub struct CacheLifecycleFacts {
    pub config_bust_turns: usize,
    pub config_bust_aic: f64,
    pub ttl_5m_subagent_turns: usize,
    pub ttl_long_main_turns: usize,
    pub premature_compactions: usize,
    pub premature_compaction_write_tokens: u64,
    pub subagent_short_cold_spawns: usize,
    pub subagent_cold_write_tokens: u64,
}

#[derive(Clone, Debug)]
pub struct Advisory {
    pub code: &'static str,
    pub message: String,
}

impl Advisory {
    pub(crate) fn compact_hint(&self) -> (u8, &str) {
        match self.code {
            "native_stall" => (0, "Shared stall detected; check cctx doctor and machine load."),
            "client_disconnected" => (
                1,
                "Native client disconnected; prefer direct code-context-code_* MCP in Agent Host/SDK sessions and keep Windows profiles synced.",
            ),
            "mid_session_config_cache_bust" => (
                2,
                "Avoid mid-session reasoningEffort or model switches; delegate or start a new session to preserve prompt cache.",
            ),
            "cold_cache_spike" | "ttl_cache_expiry" => (
                2,
                "Reduce cold-cache costs; compact long sessions and avoid 5m/1h idle TTL gaps.",
            ),
            "premature_compaction" => (
                3,
                "Avoid habitual /compact; record decisions with cctx journal and /clear when switching tasks.",
            ),
            "host_dispatch_delay" => (4, "Host dispatch is slow; check the extension host or use MCP."),
            "unregistered_workspace" => (5, "Register missing workspaces before source lookups."),
            "cache_miss_rate" => (6, "Frequent cache misses; check root admission and memory limits."),
            "shell_lookup_bypass" => (
                7,
                "Replace shell rg/grep/sed/cat source lookups with code_context, code_grep, or code_read.",
            ),
            "low_adoption_workspace" | "model_without_cctx" => {
                (7, "Prefer cctx for source lookups; check loaded instructions.")
            }
            "sequential_slicing" => (8, "Avoid sequential reads; request relevant ranges in one call."),
            "low_context_share" => (9, "Use cctxNativeContext to combine source search and reading."),
            "subagent_cold_cache" => (
                10,
                "Avoid spawning cold-cache subagents for log filtering; use pctx log / pctx batch.",
            ),
            "offload_rate" | "other_tool_offload" => {
                (11, "Reduce output offloads; narrow queries and response sizes.")
            }
            "long_session" => (12, "Compact long sessions to reduce repeated context costs."),
            "spend_concentration" => (13, "Review the highest-spend session for repeated work."),
            "reasoning_share" => (14, "Review reasoning effort for routine tasks."),
            "model_efficiency" => (15, "Review per-model efficiency and warm-cache costs in --details."),
            _ => (16, &self.message),
        }
    }
}

#[derive(Debug, Default)]
pub struct ToolStat {
    pub calls: usize,
    pub failures: usize,
    pub offloads: usize,
    pub payload_tokens: u64,
    pub durations: Vec<f64>,
    pub trace_durations: Vec<f64>,
    pub causes: BTreeMap<&'static str, usize>,
}

#[derive(Debug, Default)]
pub struct FallbackStat {
    pub calls: usize,
    pub payload_tokens: u64,
}

#[derive(Debug, Default)]
pub struct WorkspaceStat {
    pub cctx: usize,
    pub builtin: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stall {
    pub ended_at: f64,
    pub calls: usize,
    pub sessions: usize,
    pub max_ms: f64,
}

/// Finds slow CCTX calls from several sessions that all finish at the same instant,
/// which points to a shared host or daemon stall rather than slow individual lookups.
pub fn detect_stalls(calls: &[CallFacts]) -> Vec<Stall> {
    let mut slow: Vec<(f64, f64, &str)> = calls
        .iter()
        .filter(|c| matches!(c.group, "native" | "mcp"))
        .filter_map(|c| match (c.completed_at, c.duration_ms) {
            (Some(end), Some(ms)) if ms >= SLOW_CALL_MS => Some((end, ms, c.session.as_str())),
            _ => None,
        })
        .collect();
    slow.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut stalls = Vec::new();
    let mut index = 0;
    while index < slow.len() {
        let first = slow[index].0;
        let mut end = index;
        while end < slow.len() && slow[end].0 - first <= STALL_WINDOW_SECS {
            end += 1;
        }
        let group = &slow[index..end];
        let sessions: std::collections::BTreeSet<&str> = group.iter().map(|g| g.2).collect();
        if sessions.len() >= 2 {
            stalls.push(Stall {
                ended_at: group[group.len() - 1].0,
                calls: group.len(),
                sessions: sessions.len(),
                max_ms: group.iter().map(|g| g.1).fold(0.0, f64::max),
            });
        }
        index = end;
    }
    stalls
}

fn utc_clock(unix_secs: f64) -> String {
    let day = (unix_secs.max(0.0) as u64) % 86_400;
    format!("{:02}:{:02}:{:02} UTC", day / 3600, day % 3600 / 60, day % 60)
}

#[derive(Debug, Default)]
pub struct Insights {
    pub stalls: Vec<Stall>,
    pub tools: BTreeMap<String, ToolStat>,
    pub fallbacks: BTreeMap<String, FallbackStat>,
    pub workspaces: BTreeMap<String, WorkspaceStat>,
    pub advisories: Vec<Advisory>,
}

/// CCTX tool that should replace a built-in search/read tool.
pub fn cctx_replacement(tool: &str) -> &'static str {
    let lowered = tool.to_ascii_lowercase();
    if lowered == "bash"
        || lowered == "exec_command"
        || lowered == "shell"
        || lowered == "local_shell"
        || lowered == "run_in_terminal"
        || lowered == "shell_command"
        || lowered.starts_with("shell:")
    {
        "code_context / code_grep / code_read"
    } else if lowered.contains("usages") {
        "cctxNativeContext"
    } else if lowered.contains("glob")
        || lowered.contains("file_search")
        || lowered.contains("findfiles")
        || lowered.contains("find_by_name")
        || lowered.contains("list_dir")
    {
        "cctxNativeFiles"
    } else if lowered.contains("read") || lowered == "view" {
        "cctxNativeRead"
    } else {
        "cctxNativeGrep"
    }
}

pub fn percentile(sorted: &[f64], quantile: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = ((sorted.len() as f64 * quantile).ceil() as usize).saturating_sub(1);
    Some(sorted[idx.min(sorted.len() - 1)])
}

fn short(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn ratio(part: usize, whole: usize) -> f64 {
    if whole == 0 { 0.0 } else { part as f64 / whole as f64 }
}

pub fn analyze(
    calls: &[CallFacts],
    sessions: &[SessionFacts],
    models: &[ModelFacts],
    total_output_tokens: u64,
    total_reasoning_tokens: u64,
) -> Insights {
    analyze_with_lifecycle(
        calls,
        sessions,
        models,
        total_output_tokens,
        total_reasoning_tokens,
        &CacheLifecycleFacts::default(),
    )
}

pub fn analyze_with_lifecycle(
    calls: &[CallFacts],
    sessions: &[SessionFacts],
    models: &[ModelFacts],
    total_output_tokens: u64,
    total_reasoning_tokens: u64,
    lifecycle: &CacheLifecycleFacts,
) -> Insights {
    let mut insights = Insights::default();
    for call in calls {
        match call.group {
            "native" | "mcp" => {
                let stat = insights.tools.entry(call.tool.clone()).or_default();
                stat.calls += 1;
                stat.payload_tokens += call.payload_tokens;
                stat.offloads += usize::from(call.offloaded);
                if call.success == Some(false) {
                    stat.failures += 1;
                    *stat.causes.entry(call.error_detail).or_default() += 1;
                }
                if let Some(ms) = call.duration_ms {
                    stat.durations.push(ms);
                }
                if let Some(tms) = call.trace_total_ms {
                    stat.trace_durations.push(tms);
                }
                if call.eligible_lookup {
                    insights.workspaces.entry(call.workspace.clone()).or_default().cctx += 1;
                }
            }
            "builtin_search_read" | "shell_search_read" if call.eligible_lookup => {
                let label = if call.group == "shell_search_read" && !call.tool.starts_with("shell:") {
                    format!("shell:{}", call.tool)
                } else {
                    call.tool.clone()
                };
                let stat = insights.fallbacks.entry(label).or_default();
                stat.calls += 1;
                stat.payload_tokens += call.payload_tokens;
                insights.workspaces.entry(call.workspace.clone()).or_default().builtin += 1;
            }
            _ => {}
        }
    }
    for stat in insights.tools.values_mut() {
        stat.durations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        stat.trace_durations
            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    }
    insights.stalls = detect_stalls(calls);
    insights.advisories = advisories(
        &insights,
        calls,
        sessions,
        models,
        total_output_tokens,
        total_reasoning_tokens,
        lifecycle,
    );
    insights
}

fn advisories(
    insights: &Insights,
    calls: &[CallFacts],
    sessions: &[SessionFacts],
    models: &[ModelFacts],
    total_output_tokens: u64,
    total_reasoning_tokens: u64,
    lifecycle: &CacheLifecycleFacts,
) -> Vec<Advisory> {
    let mut out = Vec::new();
    let total_aic: f64 = sessions.iter().map(|s| s.used_aic).sum();

    let mut long: Vec<&SessionFacts> = sessions.iter().filter(|s| s.turns >= LONG_SESSION_TURNS).collect();
    long.sort_by(|a, b| b.used_aic.total_cmp(&a.used_aic));
    if !long.is_empty() {
        let listed = long
            .iter()
            .take(3)
            .map(|s| format!("{} ({}, {} turns, ${:.2})", short(&s.id, 12), s.workspace, s.turns, s.used_aic * 0.001))
            .collect::<Vec<_>>()
            .join("; ");
        let rest = long.len().saturating_sub(3);
        out.push(Advisory {
            code: "long_session",
            message: format!(
                "{} sessions exceed {LONG_SESSION_TURNS} turns, top by spend: {listed}{}. Compact or start a fresh session near that size to cut cached-prefix re-reads.",
                long.len(),
                if rest > 0 { format!(" (+{rest} more)") } else { String::new() }
            ),
        });
    }
    if let Some(top) = sessions.iter().max_by(|a, b| a.used_aic.total_cmp(&b.used_aic))
        && total_aic > 0.0
        && top.used_aic / total_aic >= SPEND_CONCENTRATION_SHARE
    {
        out.push(Advisory {
            code: "spend_concentration",
            message: format!(
                "Session {} ({}) holds {:.0}% of spend: split long investigations into delegated explore tasks.",
                short(&top.id, 12),
                top.workspace,
                100.0 * top.used_aic / total_aic
            ),
        });
    }
    for model in models.iter().filter(|m| m.turns >= MIN_MODEL_TURNS && m.cctx_calls == 0 && m.lookups > 0) {
        out.push(Advisory {
            code: "model_without_cctx",
            message: format!(
                "{} ran {} turns with {} built-in source lookups and 0 CCTX calls: if its sessions do find the tools via tool_search, this is instruction drift toward view/rg, otherwise check tool exposure and instruction loading.",
                model.name, model.turns, model.lookups
            ),
        });
    }
    for model in models.iter().filter(|m| m.used_aic > 0.0) {
        let adopt_label = if model.lookups == 0 {
            "no source lookups".to_string()
        } else {
            format!("adoption {:.1}%", 100.0 * ratio(model.cctx_calls, model.lookups))
        };
        let warm_adj_note = if model.cold_excess_aic >= 20.0
            && model.cold_excess_aic >= model.used_aic * 0.20
        {
            let warm_used = (model.used_aic - model.cold_excess_aic).max(0.1);
            let warm_pct = 100.0 * model.saved_aic / (warm_used + model.saved_aic);
            format!(
                "; warm-adjusted {warm_pct:.1}% excl. {:.0} AIC cold-cache misses",
                model.cold_excess_aic
            )
        } else {
            String::new()
        };
        out.push(Advisory {
            code: "model_efficiency",
            message: format!(
                "{}: saved {:.1}% of spend ({:.1} saved vs {:.1} spent AIC, {adopt_label}{warm_adj_note}).",
                model.name,
                100.0 * model.saved_aic / (model.used_aic + model.saved_aic),
                model.saved_aic,
                model.used_aic,
            ),
        });
        if model.cold_turns >= 3
            && model.cold_excess_aic >= 25.0
            && model.cold_excess_aic >= model.used_aic * 0.20
        {
            out.push(Advisory {
                code: "cold_cache_spike",
                message: format!(
                    "{} had {} large-context cold-cache turns (<50% hit), adding ~{:.1} AIC (${:.2}, {:.0}% of model spend): delegate/compact near 250K tok and use #tctxNativeRun before >60s shell waits to avoid 5m implicit-cache TTL expiry.",
                    model.name,
                    model.cold_turns,
                    model.cold_excess_aic,
                    model.cold_excess_aic * 0.001,
                    100.0 * model.cold_excess_aic / model.used_aic,
                ),
            });
        }
    }
    for (workspace, stat) in &insights.workspaces {
        let lookups = stat.cctx + stat.builtin;
        let adoption = 100.0 * ratio(stat.cctx, lookups);
        if lookups >= MIN_WORKSPACE_LOOKUPS && adoption < LOW_ADOPTION_PERCENT {
            out.push(Advisory {
                code: "low_adoption_workspace",
                message: format!(
                    "Workspace {workspace} adoption {adoption:.1}% ({}/{lookups}): check root registration, exclusions and instruction loading.",
                    stat.cctx
                ),
            });
        }
    }
    let mut unreg_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for call in calls.iter().filter(|c| {
        matches!(c.group, "builtin_search_read" | "shell_search_read") && c.eligible_lookup
    }) {
        if let Some(ref root) = call.unregistered_root {
            *unreg_counts.entry(root.as_str()).or_default() += 1;
        }
    }
    for (root, count) in unreg_counts {
        if count >= 5 {
            out.push(Advisory {
                code: "unregistered_workspace",
                message: format!(
                    "Unregistered repo {root} had {count} built-in source lookups: run `cctx --root {root} --allow-root-scope register` to index it."
                ),
            });
        }
    }
    let mcp_calls: usize = calls.iter().filter(|c| c.group == "mcp").count();
    let mcp_offloads: usize = calls.iter().filter(|c| c.group == "mcp" && c.offloaded).count();
    if mcp_calls > 0 && ratio(mcp_offloads, mcp_calls) >= OFFLOAD_RATE_ALERT {
        out.push(Advisory {
            code: "offload_rate",
            message: format!(
                "{mcp_offloads}/{mcp_calls} MCP responses exceeded the inline budget: lower limit/radius or set max_matches_per_file."
            ),
        });
    }
    let other_offloads = calls.iter().filter(|c| c.group == "other" && c.offloaded).count();
    if other_offloads > 0 {
        out.push(Advisory {
            code: "other_tool_offload",
            message: format!(
                "{other_offloads} non-CCTX tool outputs were offloaded: bound shell/git output (head, --stat, jq) before it reaches the model."
            ),
        });
    }
    let cctx_total = calls.iter().filter(|c| matches!(c.group, "native" | "mcp")).count();
    let misses = calls.iter().filter(|c| matches!(c.error_detail, "cache_miss" | "cache_load_failed")).count();
    if cctx_total > 0 && ratio(misses, cctx_total) >= CACHE_MISS_RATE_ALERT {
        out.push(Advisory {
            code: "cache_miss_rate",
            message: format!(
                "{misses}/{cctx_total} CCTX calls hit a cold cache: keep active roots warm and avoid sibling-worktree churn."
            ),
        });
    }
    for stall in insights.stalls.iter().take(3) {
        out.push(Advisory {
            code: "native_stall",
            message: format!(
                "{} slow CCTX calls from {} sessions all finished at {} (longest {:.0}s): a shared host or daemon stall, not slow lookups. Check machine load, the extension host and `cctx doctor`; agents should fall back to MCP/CLI after {:.0}s.",
                stall.calls,
                stall.sessions,
                utc_clock(stall.ended_at),
                stall.max_ms / 1000.0,
                SLOW_CALL_MS / 1000.0
            ),
        });
    }
    if insights.stalls.len() > 3 {
        out.push(Advisory {
            code: "native_stall",
            message: format!("{} more stalls not listed.", insights.stalls.len() - 3),
        });
    }
    let host_delays: Vec<f64> = calls.iter().filter_map(|c| c.host_delay_ms).collect();
    let host_slow = host_delays.iter().filter(|ms| **ms >= HOST_DELAY_ALERT_MS).count();
    if host_slow > 0 {
        out.push(Advisory {
            code: "host_dispatch_delay",
            message: format!(
                "{host_slow}/{} timed native calls spent over {:.0}s outside the extension and daemon (max {:.1}s): VS Code host dispatch is the bottleneck.",
                host_delays.len(),
                HOST_DELAY_ALERT_MS / 1000.0,
                host_delays.iter().copied().fold(0.0, f64::max) / 1000.0
            ),
        });
    }
    let disconnects = calls.iter().filter(|c| c.error_detail == "client_disconnected").count();
    if disconnects > 0 {
        out.push(Advisory {
            code: "client_disconnected",
            message: format!(
                "{disconnects} native calls failed on client disconnect or another-client dispatch stall: prefer direct code-context-code_* backend MCP in Agent Host / Copilot SDK / background sessions so Reload Window never stalls or fails client dispatch, and keep Windows profiles synced (`install-vscode-extension.sh`) for Editor Chat."
            ),
        });
    }
    let read_calls = calls
        .iter()
        .filter(|c| {
            matches!(
                c.tool.as_str(),
                "cctxNativeRead" | "code-context-code_read" | "code_read" | "code-context-native_codeRead"
            )
        })
        .count();
    let seq_slices = calls.iter().filter(|c| c.sequential_slice).count();
    let read_denom = read_calls.max(seq_slices);
    if seq_slices >= MIN_SEQUENTIAL_SLICES && ratio(seq_slices, read_denom) >= SEQUENTIAL_SLICE_SHARE_ALERT {
        out.push(Advisory {
            code: "sequential_slicing",
            message: format!(
                "{seq_slices}/{read_denom} ({:.1}%) CCTX reads triggered SEQUENTIAL_SLICE (0 avoided-turn credit): jump via #cctxNativeContext, around_line+radius, or batch multiple ranges in 1 call.",
                100.0 * ratio(seq_slices, read_denom)
            ),
        });
    }
    let context_calls = calls
        .iter()
        .filter(|c| {
            matches!(
                c.tool.as_str(),
                "cctxNativeContext"
                    | "code-context-code_context"
                    | "code_context"
                    | "code-context-native_codeContext"
                    | "mcp__code-context__code_context"
            )
        })
        .count();
    if cctx_total >= 25 && context_calls * 20 < cctx_total {
        out.push(Advisory {
            code: "low_context_share",
            message: format!(
                "Only {context_calls}/{cctx_total} ({:.1}%) CCTX calls used code_context: prefer #cctxNativeContext / code_context (1-call symbol + surrounding lines) over Grep->Read chains or sequential 150-line file slicing.",
                100.0 * ratio(context_calls, cctx_total)
            ),
        });
    } else {
        for (workspace, stat) in &insights.workspaces {
            if stat.cctx >= 25 {
                let ws_ctx = calls
                    .iter()
                    .filter(|c| {
                        &c.workspace == workspace
                            && matches!(
                                c.tool.as_str(),
                                "cctxNativeContext"
                                    | "code-context-code_context"
                                    | "code_context"
                                    | "code-context-native_codeContext"
                                    | "mcp__code-context__code_context"
                            )
                    })
                    .count();
                if ws_ctx * 20 < stat.cctx {
                    out.push(Advisory {
                        code: "low_context_share",
                        message: format!(
                            "Workspace {workspace} used code_context on only {ws_ctx}/{} ({:.1}%) CCTX calls: prefer #cctxNativeContext / code_context over Grep->Read chains or sequential 150-line file slicing.",
                            stat.cctx,
                            100.0 * ratio(ws_ctx, stat.cctx)
                        ),
                    });
                }
            }
        }
    }
    let shell_lookups: Vec<&CallFacts> = calls
        .iter()
        .filter(|c| c.group == "shell_search_read" && c.eligible_lookup)
        .collect();
    if shell_lookups.len() >= 3 {
        let shell_tok: u64 = shell_lookups.iter().map(|c| c.payload_tokens).sum();
        out.push(Advisory {
            code: "shell_lookup_bypass",
            message: format!(
                "{} eligible source lookups bypassed CCTX via shell commands (~{} payload tok): replace Bash/exec_command rg/grep/sed -n/cat/find with code_context, code_grep, or code_read.",
                shell_lookups.len(),
                shell_tok
            ),
        });
    }
    if lifecycle.config_bust_turns > 0 {
        out.push(Advisory {
            code: "mid_session_config_cache_bust",
            message: format!(
                "{} large-context turns dropped prompt cache (<50% hit) immediately after a mid-session reasoningEffort or model change (+~{:.1} AIC / ${:.2}): keep model and effort stable inside long threads or delegate via subagents.",
                lifecycle.config_bust_turns,
                lifecycle.config_bust_aic,
                lifecycle.config_bust_aic * 0.001,
            ),
        });
    }
    if lifecycle.ttl_5m_subagent_turns + lifecycle.ttl_long_main_turns >= 2 {
        out.push(Advisory {
            code: "ttl_cache_expiry",
            message: format!(
                "Idle TTL cache expirations detected: {} subagent turns (>5m gap) and {} main-session turns (>30m/1h gap) rebuilt cold prefixes; batch subagent work within 5m and record state with `cctx journal` before long breaks.",
                lifecycle.ttl_5m_subagent_turns,
                lifecycle.ttl_long_main_turns,
            ),
        });
    }
    if lifecycle.premature_compactions > 0 {
        out.push(Advisory {
            code: "premature_compaction",
            message: format!(
                "{} compactions ran with <5 follow-up turns or repeated within <30 turns (~{} cache-write tok rebuilt at 1.25x-2.0x): avoid habitual `/compact`; use `cctx journal record` + `/clear` when switching to unrelated work.",
                lifecycle.premature_compactions,
                lifecycle.premature_compaction_write_tokens,
            ),
        });
    }
    if lifecycle.subagent_short_cold_spawns >= 2 {
        out.push(Advisory {
            code: "subagent_cold_cache",
            message: format!(
                "{} short subagent runs (<=4 turns) wrote ~{} fresh cache tokens on separate 5m caches: use `pctx batch` + `pctx log` in the main session for log/JSON filtering instead of spawning subagents.",
                lifecycle.subagent_short_cold_spawns,
                lifecycle.subagent_cold_write_tokens,
            ),
        });
    }
    let thinking = total_output_tokens + total_reasoning_tokens;
    if thinking > 0 && total_reasoning_tokens as f64 / thinking as f64 >= REASONING_SHARE_ALERT {
        out.push(Advisory {
            code: "reasoning_share",
            message: format!(
                "Reasoning is {:.0}% of generated tokens: delegate simple lookups to low-effort subagents.",
                100.0 * total_reasoning_tokens as f64 / thinking as f64
            ),
        });
    }
    out
}

fn canonical_cctx_operation(tool: &str) -> Option<&'static str> {
    let lower = tool.trim().to_ascii_lowercase();
    if lower.ends_with("code_context")
        || lower.ends_with("cctxnativecontext")
        || lower.ends_with("_codecontext")
    {
        Some("code_context")
    } else if lower.ends_with("code_read")
        || lower.ends_with("cctxnativeread")
        || lower.ends_with("_coderead")
    {
        Some("code_read")
    } else if lower.ends_with("code_grep")
        || lower.ends_with("cctxnativegrep")
        || lower.ends_with("_codegrep")
    {
        Some("code_grep")
    } else if lower.ends_with("code_files")
        || lower.ends_with("cctxnativefiles")
        || lower.ends_with("_codefiles")
    {
        Some("code_files")
    } else if lower.ends_with("code_workspaces")
        || lower.ends_with("cctxnativeworkspaces")
        || lower.ends_with("_codeworkspaces")
    {
        Some("code_workspaces")
    } else if lower.ends_with("code_status")
        || lower.ends_with("cctxnativestatus")
        || lower.ends_with("_codestatus")
    {
        Some("code_status")
    } else if lower.ends_with("code_sync")
        || lower.ends_with("cctxnativesync")
        || lower.ends_with("_codesync")
    {
        Some("code_sync")
    } else {
        None
    }
}

impl Insights {
    pub fn to_json(&self) -> Value {
        self.to_json_bounded(false)
    }

    pub fn to_json_bounded(&self, compact_bounds: bool) -> Value {
        let trunc_label = |s: &str| -> String {
            if compact_bounds && s.len() > 256 {
                let mut end = 256;
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…", &s[..end])
            } else {
                s.to_owned()
            }
        };
        let tool_limit = if compact_bounds { 64 } else { usize::MAX };
        let mut sorted_tools: Vec<(&String, &ToolStat)> = self.tools.iter().collect();
        if compact_bounds {
            sorted_tools.sort_by_key(|(_, s)| std::cmp::Reverse(s.calls));
        }
        let tools: Vec<Value> = sorted_tools
            .into_iter()
            .take(tool_limit)
            .map(|(tool, s)| {
                json!({
                    "tool": trunc_label(tool),
                    "calls": s.calls,
                    "failures": s.failures,
                    "failure_causes": s.causes,
                    "offloads": s.offloads,
                    "median_ms": percentile(&s.durations, 0.5),
                    "p95_ms": percentile(&s.durations, 0.95),
                    "p99_ms": percentile(&s.durations, 0.99),
                    "trace_median_ms": percentile(&s.trace_durations, 0.5),
                    "trace_p95_ms": percentile(&s.trace_durations, 0.95),
                    "avg_payload_tokens": if s.calls == 0 { 0 } else { s.payload_tokens / s.calls as u64 },
                })
            })
            .collect();

        // Pool raw duration samples per canonical CCTX operation so medians & P95s across
        // MCP (`code-context-code_*`) and Native (`cctxNative*`) aliases are mathematically exact.
        let mut op_buckets: BTreeMap<&'static str, ToolStat> = BTreeMap::new();
        let mut pooled_source_trace: Vec<f64> = Vec::new();
        let mut pooled_source_wall: Vec<f64> = Vec::new();
        for (tool, s) in &self.tools {
            let Some(op) = canonical_cctx_operation(tool) else {
                continue;
            };
            let b = op_buckets.entry(op).or_default();
            b.calls += s.calls;
            b.failures += s.failures;
            b.offloads += s.offloads;
            b.payload_tokens += s.payload_tokens;
            b.durations.extend_from_slice(&s.durations);
            b.trace_durations.extend_from_slice(&s.trace_durations);
            for (&cause, &cnt) in &s.causes {
                *b.causes.entry(cause).or_default() += cnt;
            }
            if matches!(op, "code_context" | "code_read" | "code_grep" | "code_files") {
                pooled_source_trace.extend_from_slice(&s.trace_durations);
                pooled_source_wall.extend_from_slice(&s.durations);
            }
        }
        pooled_source_trace.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        pooled_source_wall.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut cctx_operations: Vec<Value> = op_buckets
            .into_iter()
            .map(|(op, mut b)| {
                b.durations
                    .sort_by(|a, c| a.partial_cmp(c).unwrap_or(std::cmp::Ordering::Equal));
                b.trace_durations
                    .sort_by(|a, c| a.partial_cmp(c).unwrap_or(std::cmp::Ordering::Equal));
                let med = percentile(&b.trace_durations, 0.5).or_else(|| percentile(&b.durations, 0.5));
                let p95 = percentile(&b.trace_durations, 0.95).or_else(|| percentile(&b.durations, 0.95));
                json!({
                    "operation": op,
                    "calls": b.calls,
                    "failures": b.failures,
                    "failure_causes": b.causes,
                    "offloads": b.offloads,
                    "median_ms": med,
                    "p95_ms": p95,
                    "wall_median_ms": percentile(&b.durations, 0.5),
                    "wall_p95_ms": percentile(&b.durations, 0.95),
                    "avg_payload_tokens": if b.calls == 0 { 0 } else { b.payload_tokens / b.calls as u64 },
                })
            })
            .collect();
        cctx_operations.sort_by_key(|v| std::cmp::Reverse(v["calls"].as_u64().unwrap_or(0)));
        let source_median_ms = percentile(&pooled_source_trace, 0.5)
            .or_else(|| percentile(&pooled_source_wall, 0.5));
        let source_p95_ms = percentile(&pooled_source_trace, 0.95)
            .or_else(|| percentile(&pooled_source_wall, 0.95));

        let fb_limit = if compact_bounds { 32 } else { usize::MAX };
        let ws_limit = if compact_bounds { 32 } else { usize::MAX };
        let stall_limit = if compact_bounds { 16 } else { usize::MAX };
        let adv_limit = if compact_bounds { 16 } else { usize::MAX };
        let fallbacks: Vec<Value> = self
            .fallbacks
            .iter()
            .take(fb_limit)
            .map(|(tool, s)| {
                json!({
                    "tool": trunc_label(tool),
                    "calls": s.calls,
                    "avg_payload_tokens": if s.calls == 0 { 0 } else { s.payload_tokens / s.calls as u64 },
                    "replace_with": cctx_replacement(tool),
                })
            })
            .collect();
        let workspaces: Vec<Value> = self
            .workspaces
            .iter()
            .take(ws_limit)
            .map(|(workspace, s)| {
                json!({
                    "workspace": trunc_label(workspace),
                    "cctx": s.cctx,
                    "builtin": s.builtin,
                    "adoption_percent": 100.0 * ratio(s.cctx, s.cctx + s.builtin),
                })
            })
            .collect();
        json!({
            "tool_reliability": tools,
            "cctx_operations": cctx_operations,
            "cctx_source_median_ms": source_median_ms,
            "cctx_source_p95_ms": source_p95_ms,
            "builtin_fallbacks": fallbacks,
            "workspace_adoption": workspaces,
            "stalls": self.stalls.iter().take(stall_limit).map(|s| json!({
                "ended_at_unix": s.ended_at, "calls": s.calls, "sessions": s.sessions, "max_ms": s.max_ms,
            })).collect::<Vec<_>>(),
            "advisories": self.advisories.iter().take(adv_limit).map(|a| json!({"code": a.code, "message": trunc_label(&a.message)})).collect::<Vec<_>>(),
        })
    }

    pub fn render(&self) -> Vec<String> {
        let mut out = Vec::new();
        let display = UsageDisplay::terminal();
        let ms = |v: Option<f64>| v.map_or_else(|| "-".into(), |m| format!("{m:.0}ms"));

        let mut tool_rows: Vec<(&String, &ToolStat)> = self.tools.iter().collect();
        tool_rows.sort_by_key(|row| std::cmp::Reverse(row.1.calls));
        let rows: Vec<Vec<String>> = tool_rows
            .into_iter()
            .map(|(tool, s)| {
                let mut causes: Vec<(&&str, &usize)> = s.causes.iter().collect();
                causes.sort_by(|a, b| b.1.cmp(a.1));
                let cause_text = causes
                    .iter()
                    .take(3)
                    .map(|(name, n)| format!("{name}×{n}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                vec![
                    tool.clone(),
                    s.calls.to_string(),
                    s.failures.to_string(),
                    if cause_text.is_empty() { "-".into() } else { cause_text },
                    s.offloads.to_string(),
                    ms(percentile(&s.durations, 0.5)),
                    ms(percentile(&s.durations, 0.95)),
                    ms(percentile(&s.durations, 0.99)),
                    format!("{} tok", if s.calls == 0 { 0 } else { s.payload_tokens / s.calls as u64 }),
                ]
            })
            .collect();
        out.push(String::new());
        out.push(display.cards(
            "4. CCTX TOOL RELIABILITY, FAILURE CAUSES & TAIL LATENCY",
            &["Tool", "Calls", "Fail", "Top Causes", "Offload", "Median", "P95", "P99", "Avg Payload"],
            &rows,
        ));

        let mut fb: Vec<(&String, &FallbackStat)> = self.fallbacks.iter().collect();
        fb.sort_by_key(|row| std::cmp::Reverse(row.1.calls));
        let rows: Vec<Vec<String>> = fb
            .into_iter()
            .map(|(tool, s)| {
                vec![
                    tool.clone(),
                    s.calls.to_string(),
                    format!("{} tok", if s.calls == 0 { 0 } else { s.payload_tokens / s.calls as u64 }),
                    cctx_replacement(tool).into(),
                ]
            })
            .collect();
        out.push(String::new());
        out.push(display.cards(
            "5. BUILT-IN FALLBACKS TO REPLACE (eligible source lookups only)",
            &["Built-in Tool", "Calls", "Avg Payload", "Use Instead"],
            &rows,
        ));

        let mut ws: Vec<(&String, &WorkspaceStat)> = self.workspaces.iter().filter(|(_, s)| s.builtin > 0).collect();
        ws.sort_by_key(|row| std::cmp::Reverse(row.1.builtin));
        let rows: Vec<Vec<String>> = ws
            .into_iter()
            .take(8)
            .map(|(workspace, s)| {
                vec![
                    workspace.clone(),
                    s.cctx.to_string(),
                    s.builtin.to_string(),
                    format!("{:.1}%", 100.0 * ratio(s.cctx, s.cctx + s.builtin)),
                ]
            })
            .collect();
        out.push(String::new());
        out.push(display.cards(
            "6. WORKSPACES WITH THE MOST BUILT-IN FALLBACKS (Top 8)",
            &["Workspace", "CCTX", "Built-in", "Adoption"],
            &rows,
        ));

        out.push(String::new());
        out.push(display.section("7. OPTIMIZATION ADVISORIES", Style::Advisory));
        if self.advisories.is_empty() {
            out.push(display.text("No long sessions, offload spikes or adoption gaps detected.", Style::Muted));
        }
        for (index, advisory) in self.advisories.iter().enumerate() {
            out.push(String::new());
            out.push(display.text(&format!("{:02} / {}", index + 1, advisory.code), Style::Advisory));
            out.push(display.text(&format!("  {}", advisory.message), Style::Normal));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(tool: &str, group: &'static str) -> CallFacts {
        CallFacts {
            workspace: "w".into(),
            tool: tool.into(),
            group,
            success: Some(true),
            eligible_lookup: true,
            ..CallFacts::default()
        }
    }

    #[test]
    fn replacement_maps_builtin_tools_to_native_equivalents() {
        assert_eq!(cctx_replacement("rg"), "cctxNativeGrep");
        assert_eq!(cctx_replacement("grep_search"), "cctxNativeGrep");
        assert_eq!(cctx_replacement("glob"), "cctxNativeFiles");
        assert_eq!(cctx_replacement("view"), "cctxNativeRead");
        assert_eq!(cctx_replacement("read_file"), "cctxNativeRead");
        assert_eq!(cctx_replacement("usages"), "cctxNativeContext");
    }

    fn slow(session: &str, end: f64, ms: f64) -> CallFacts {
        CallFacts {
            session: session.into(),
            completed_at: Some(end),
            duration_ms: Some(ms),
            ..call("cctxNativeRead", "native")
        }
    }

    #[test]
    fn simultaneous_slow_calls_across_sessions_form_one_stall() {
        let calls = vec![
            slow("a", 1000.0, 77_000.0),
            slow("b", 1000.2, 60_000.0),
            slow("c", 1000.4, 34_000.0),
            slow("a", 1000.4, 17_000.0),
            slow("d", 5000.0, 9_000.0),
            slow("a", 8000.0, 9_000.0),
            slow("a", 8000.1, 9_500.0),
            slow("e", 9000.0, 100.0),
        ];
        let stalls = detect_stalls(&calls);
        assert_eq!(stalls, vec![Stall { ended_at: 1000.4, calls: 4, sessions: 3, max_ms: 77_000.0 }]);
        let insights = analyze(&calls, &[], &[], 0, 0);
        let advisory = insights.advisories.iter().find(|a| a.code == "native_stall").expect("stall advisory");
        assert!(advisory.message.contains("4 slow CCTX calls from 3 sessions"), "{}", advisory.message);
        assert!(insights.to_json()["stalls"][0]["max_ms"] == 77_000.0);
    }

    #[test]
    fn models_without_source_lookups_are_not_flagged_for_missing_cctx() {
        let models = vec![ModelFacts { name: "Quiet".into(), turns: 100, lookups: 0, cctx_calls: 0, ..ModelFacts::default() }];
        let insights = analyze(&[], &[], &models, 0, 0);
        assert!(insights.advisories.iter().all(|a| a.code != "model_without_cctx"));
    }

    #[test]
    fn host_delay_beyond_threshold_is_reported() {
        let mut delayed = call("cctxNativeRead", "native");
        delayed.host_delay_ms = Some(60_000.0);
        let mut quick = call("cctxNativeRead", "native");
        quick.host_delay_ms = Some(40.0);
        let insights = analyze(&[delayed, quick], &[], &[], 0, 0);
        let advisory = insights.advisories.iter().find(|a| a.code == "host_dispatch_delay").expect("host delay advisory");
        assert!(advisory.message.starts_with("1/2 timed native calls"), "{}", advisory.message);
    }

    #[test]
    fn percentile_handles_empty_and_tail() {
        assert_eq!(percentile(&[], 0.5), None);
        let values: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&values, 0.95), Some(95.0));
        assert_eq!(percentile(&values, 0.99), Some(99.0));
    }

    #[test]
    fn analysis_reports_failures_fallbacks_and_advisories() {
        let mut failed = call("cctxNativeGrep", "native");
        failed.success = Some(false);
        failed.error_detail = "client_disconnected";
        let mut offloaded = call("code_grep", "mcp");
        offloaded.offloaded = true;
        let mut calls = vec![failed, offloaded];
        calls.extend((0..25).map(|_| call("rg", "builtin_search_read")));
        let sessions = vec![SessionFacts { id: "abcdef0123456789".into(), workspace: "w".into(), turns: 200, used_aic: 100.0 }];
        let models = vec![ModelFacts { name: "M".into(), turns: 30, lookups: 27, cctx_calls: 0, used_aic: 10.0, saved_aic: 1.0, ..ModelFacts::default() }];
        let insights = analyze(&calls, &sessions, &models, 100, 100);
        assert_eq!(insights.tools["cctxNativeGrep"].causes["client_disconnected"], 1);
        assert_eq!(insights.fallbacks["rg"].calls, 25);
        let codes: Vec<&str> = insights.advisories.iter().map(|a| a.code).collect();
        for expected in [
            "long_session",
            "spend_concentration",
            "model_without_cctx",
            "low_adoption_workspace",
            "offload_rate",
            "client_disconnected",
            "reasoning_share",
        ] {
            assert!(codes.contains(&expected), "missing {expected}: {codes:?}");
        }
        let text = insights.render().join("\n");
        assert!(text.contains("cctxNativeGrep") && text.contains("OPTIMIZATION ADVISORIES"));
        assert_eq!(insights.to_json()["builtin_fallbacks"][0]["replace_with"], "cctxNativeGrep");
    }

    #[test]
    fn sequential_slicing_advisory_triggers_above_threshold() {
        let mut sliced = call("cctxNativeRead", "native");
        sliced.sequential_slice = true;
        let mut calls: Vec<CallFacts> = (0..12).map(|_| sliced.clone()).collect();
        calls.extend((0..20).map(|_| call("cctxNativeRead", "native")));
        let insights = analyze(&calls, &[], &[], 0, 0);
        let adv = insights
            .advisories
            .iter()
            .find(|a| a.code == "sequential_slicing")
            .expect("sequential_slicing advisory");
        assert!(adv.message.contains("12/32 (37.5%)"), "{}", adv.message);
    }
}
