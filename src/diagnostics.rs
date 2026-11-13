use crate::usage_insights::{self, CallFacts, ModelFacts, SessionFacts};
use crate::{Result, catalog, daemon, invalid, outcome};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, Read};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const CAPABILITY_SCHEMA: u32 = 3;

// Fallback savings assumptions when a session has no `session.usage_record` events;
// when usage records exist, `cctx usage` replays actual per-turn `copilotUsage.tokenDetails`
// rates, pre-compaction re-read horizons, and warm-turn context sizes.
const COMPOUND_CACHE_MULTIPLIER: u64 = 35;
const TOKENS_PER_AVOIDED_TURN: f64 = 220_000.0;
const AIC_PER_SAVED_TOKEN: f64 = 0.00055;
const AIC_PER_AVOIDED_TURN: f64 = 4.20;

#[derive(Clone, Copy, Debug)]
struct TokenRates {
    input: f64,
    cache_read: f64,
    cache_write: f64,
    output: f64,
}

impl Default for TokenRates {
    fn default() -> Self {
        Self {
            input: 200.0e-6,
            cache_read: 10.0e-6,
            cache_write: 0.0,
            output: 1000.0e-6,
        }
    }
}

fn extract_token_rates(usage: &Value) -> TokenRates {
    let mut rates = TokenRates::default();
    if let Some(details) = usage
        .get("copilotUsage")
        .and_then(|cu| cu.get("tokenDetails"))
        .and_then(Value::as_array)
    {
        for item in details {
            let batch = item.get("batchSize").and_then(Value::as_f64).unwrap_or(1_000_000.0);
            let cost_nano = item.get("costPerBatch").and_then(Value::as_f64).unwrap_or(0.0);
            if batch <= 0.0 {
                continue;
            }
            let rate = (cost_nano / 1_000_000_000.0) / batch;
            match item.get("tokenType").and_then(Value::as_str) {
                Some("input") => rates.input = rate,
                Some("cache_read") => rates.cache_read = rate,
                Some("cache_write") => rates.cache_write = rate,
                Some("output") => rates.output = rate,
                _ => {}
            }
        }
    }
    rates
}

fn cold_prefix_rate(cache_write_tokens: u64, rates: TokenRates) -> f64 {
    if cache_write_tokens > 0 && rates.cache_write > 0.0 {
        rates.cache_write
    } else {
        rates.input
    }
}

pub fn executable_hash(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    loop {
        let read = file.read(&mut bytes)?;
        if read == 0 {
            break;
        }
        hash.update(&bytes[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn capabilities() -> Result<Value> {
    static CAPABILITIES: std::sync::OnceLock<std::result::Result<Value, String>> =
        std::sync::OnceLock::new();
    Ok(CAPABILITIES
        .get_or_init(|| {
            let hash = {
                #[cfg(unix)]
                {
                    executable_hash(Path::new("/proc/self/exe")).map_err(|e| e.to_string())?
                }
                #[cfg(not(unix))]
                {
                    let path = std::env::current_exe().map_err(|e| e.to_string())?;
                    executable_hash(&path).map_err(|e| e.to_string())?
                }
            };
            Ok(json!({"schema":CAPABILITY_SCHEMA,"executable_sha256":hash,
                "package_version":env!("CARGO_PKG_VERSION"),"tool_count":7,
                "code_files":true,"context_radius_max":100,"authorized_worktrees":true,
                "registration_migration":true,"read_pages":true,"background_workers":1,"request_tracing":true,
                "cache_peak_admission":"minimum_lru","prewarm_priority":"disabled","trace_clock":"unix_ms",
                "source_cache":"nonblocking_root_cache","cache_pending_roots":8,"cache_miss":"CACHE_MISS",
                "read_policy_cache":true,"adaptive_memory":true,
                "client_trace_outcomes":true,"host_memory_reporting":true,
                "idle_maintenance_interval_ms":1000,
                "active_poll_interval_ms":25,"runtime_identity_guard":true,
                "max_registered_roots":crate::catalog::MAX_REGISTERED_ROOTS,
                "idle_source_lease_seconds":crate::watched::IDLE_ROOT_TIMEOUT.as_secs()}))
        })
        .clone()
        .map_err(invalid)?)
}

pub fn doctor(root: &Path) -> Result<Value> {
    let local = capabilities()?;
    let ping = daemon::request(&daemon::Request {
        root: None,
        name: "ping".into(),
        arguments: json!({}),
        compact: true,
    });
    let (remote, daemon_error) = match ping {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(error.to_string())),
    };
    let specs = catalog::load()?;
    let root_resolution = match catalog::resolve_root(root) {
        Ok(root) => json!({"registered":true,"root":root}),
        Err(error) => json!({"registered":false,"error":error.to_string()}),
    };
    let mut processes = Vec::new();
    let mut inspection_errors: Vec<String> = Vec::new();
    #[cfg(unix)]
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let exe = entry.path().join("exe");
        let target = match std::fs::read_link(&exe) {
            Ok(target) => target,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                continue;
            }
            Err(error) => {
                inspection_errors.push(error.to_string());
                continue;
            }
        };
        match entry.metadata() {
            Ok(metadata) if metadata.uid() == unsafe { libc::geteuid() } => {}
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                inspection_errors.push(error.to_string());
                continue;
            }
        }
        let name = target
            .to_string_lossy()
            .trim_end_matches(" (deleted)")
            .to_owned();
        if Path::new(&name).file_name().is_none_or(|n| n != "cctx") {
            continue;
        }
        let hash = match executable_hash(&exe) {
            Ok(hash) => hash,
            Err(error) => {
                inspection_errors.push(error.to_string());
                continue;
            }
        };
        let command = match std::fs::read(entry.path().join("cmdline")) {
            Ok(command) => command,
            Err(error) => {
                inspection_errors.push(error.to_string());
                continue;
            }
        };
        let role = if command.split(|b| *b == 0).any(|arg| arg == b"shared-mcp") {
            "shared-mcp"
        } else if command.split(|b| *b == 0).any(|arg| arg == b"daemon") {
            "daemon"
        } else {
            "client"
        };
        processes.push(
            json!({"pid":entry.file_name().to_string_lossy(),"role":role,
            "current_build":hash==local["executable_sha256"],"executable_sha256":hash,
            "cwd":std::fs::read_link(entry.path().join("cwd")).ok()}),
        );
    }
    #[cfg(not(unix))]
    {
        let _ = &mut inspection_errors;
        if let Some(ref daemon_info) = remote {
            let daemon_hash = daemon_info
                .get("capabilities")
                .and_then(|c| c.get("executable_sha256"))
                .cloned()
                .unwrap_or(Value::Null);
            processes.push(json!({
                "pid": daemon_info.get("pid").map(|p| p.to_string()).unwrap_or_default(),
                "role": "daemon",
                "current_build": daemon_hash == local["executable_sha256"],
                "executable_sha256": daemon_hash,
                "cwd": Value::Null,
            }));
        }
        processes.push(json!({
            "pid": std::process::id().to_string(),
            "role": "client",
            "current_build": true,
            "executable_sha256": local["executable_sha256"],
            "cwd": std::env::current_dir().ok(),
        }));
    }
    let stale = processes
        .iter()
        .filter(|p| p["role"] == "shared-mcp" && p["current_build"] == false)
        .count();
    let daemon_matches_client = remote
        .as_ref()
        .and_then(|value| value.get("capabilities"))
        .and_then(|value| value.get("executable_sha256"))
        .map(|hash| hash == &local["executable_sha256"]);
    let legacy_glob_roots: Vec<PathBuf> = specs
        .iter()
        .filter(|spec| {
            spec.managed_parent.is_none()
                && !spec.globs.iter().any(|g| g == "*.md")
        })
        .map(|spec| spec.root.clone())
        .collect();
    let missing_managed_worktrees = catalog::missing_managed_roots(&specs).unwrap_or_default();
    let subagent_definitions = inspect_subagent_definitions();
    Ok(
        json!({"client":local,"daemon":remote,"daemon_error":daemon_error,
        "root_resolution":root_resolution,"registrations":specs,"processes":processes,
        "stale_mcp_processes":stale,"daemon_matches_client":daemon_matches_client,
        "legacy_glob_roots":legacy_glob_roots,
        "missing_managed_worktrees":missing_managed_worktrees,
        "subagent_definitions":subagent_definitions,
        "inspection_errors":inspection_errors,
        "chat_exposure":"unverified; executable/cwd is not a session-to-process mapping",
        "recovery":{
            "interrupts_active_sessions":false,
            "steps":["Finish in-flight tool requests before reconnecting the host's code-context MCP server.",
                "For native tools, reload the extension host at a safe boundary, then refresh tools in the affected chat.",
                "Verify seven tools including code_files and radius 100; do not infer readiness from the installed VSIX alone."],
            "automatic_process_termination":false
        }}),
    )
}

pub fn coverage() -> Result<Value> {
    let mut roots = Vec::new();
    let specs = catalog::load()?;
    for saved in &specs {
        let spec = match crate::registration::effective_spec(saved, &specs) {
            Ok(spec) => spec,
            Err(error) => {
                roots.push(json!({"registration":saved,"error":error.to_string()}));
                continue;
            }
        };
        match spec
            .workspace()
            .and_then(|workspace| workspace.files_limited_skip_oversized(&spec.policy))
        {
            Ok(inventory) => {
                let mut files: Vec<_> = inventory
                    .files
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                files.sort();
                roots.push(json!({
                    "registration":spec,
                    "glob_count":spec.globs.len(),
                    "allow_worktrees":spec.allow_worktrees,
                    "files":files
                }));
            }
            Err(error) => roots.push(json!({"registration":spec,"error":error.to_string()})),
        }
    }
    let preparation_candidates = crate::registration::preparation_candidates(&specs);
    Ok(json!({"generated_at_unix":std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?.as_secs(),"roots":roots,
        "preparation_candidates":preparation_candidates,
        "contract":"point-in-time permitted inventory, not historical eligibility or host content-policy authorization"}))
}

fn inspect_subagent_definitions() -> Value {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return json!({"checked": 0, "configured": 0, "unconfigured": []});
    };
    let auto_patched = crate::prompt_formula::ensure_subagents_configured(&home);
    let mut checked = 0usize;
    let mut configured = 0usize;
    let mut unconfigured: Vec<String> = Vec::new();
    let bin_root = home.join(".vscode-server/bin");
    if let Ok(entries) = std::fs::read_dir(&bin_root) {
        for entry in entries.flatten() {
            for rel in [
                "node_modules/@github/copilot-sdk-linux-x64/definitions/explore.agent.yaml",
                "extensions/copilot/node_modules/@github/copilot/sdk/definitions/explore.agent.yaml",
                "extensions/copilot/node_modules/@github/copilot/sdk/definitions/research.agent.yaml",
            ] {
                let candidate = entry.path().join(rel);
                if let Ok(text) = std::fs::read_to_string(&candidate) {
                    checked += 1;
                    if text.contains("cctxNativeContext") && text.contains("code-context/*") {
                        configured += 1;
                    } else if unconfigured.len() < 8 {
                        unconfigured.push(candidate.to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
    json!({
        "checked": checked,
        "configured": configured,
        "auto_patched": auto_patched,
        "unconfigured": unconfigured,
        "remediation": if unconfigured.is_empty() { Value::Null } else { json!("Run: python3 scripts/configure_subagents.py") },
    })
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn parse_iso8601_unix(ts: &str) -> Option<f64> {
    let bytes = ts.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || (bytes[10] != b'T' && bytes[10] != b' ') {
        return None;
    }
    let year: i64 = ts.get(0..4)?.parse().ok()?;
    let month: i64 = ts.get(5..7)?.parse().ok()?;
    let day: i64 = ts.get(8..10)?.parse().ok()?;
    let hour: i64 = ts.get(11..13)?.parse().ok()?;
    let minute: i64 = ts.get(14..16)?.parse().ok()?;
    let second: i64 = ts.get(17..19)?.parse().ok()?;
    let mut rest = &ts[19..];
    let mut frac = 0.0_f64;
    if let Some(after_dot) = rest.strip_prefix('.') {
        let digits_len = after_dot.bytes().take_while(|b| b.is_ascii_digit()).count();
        if digits_len > 0
            && let Ok(val) = after_dot[..digits_len].parse::<f64>()
        {
            frac = val / 10_f64.powi(digits_len as i32);
        }
        rest = &after_dot[digits_len..];
    }
    let tz_offset_secs: i64 = if rest == "Z" || rest == "z" || rest.is_empty() {
        0
    } else if (rest.starts_with('+') || rest.starts_with('-')) && rest.len() >= 6 {
        let sign = if rest.as_bytes()[0] == b'-' { -1 } else { 1 };
        let tz_h: i64 = rest.get(1..3)?.parse().ok()?;
        let tz_m: i64 = rest.get(4..6)?.parse().ok()?;
        sign * (tz_h * 3600 + tz_m * 60)
    } else {
        0
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - tz_offset_secs;
    Some(secs as f64 + frac)
}

fn event_unix_ts(event: &Value) -> Option<f64> {
    if let Some(val) = event
        .get("_ahpLog")
        .and_then(|m| m.get("ts"))
        .or_else(|| event.get("timestamp"))
    {
        if let Some(num) = val.as_f64() {
            return Some(if num > 10_000_000_000.0 { num / 1000.0 } else { num });
        }
        if let Some(s) = val.as_str() {
            return parse_iso8601_unix(s);
        }
    }
    None
}

fn classify_tool_group(name: &str) -> &'static str {
    if matches!(name, "tool_search_tool" | "web_search" | "web_fetch" | "doSearch") {
        return "other";
    }
    if matches!(
        name,
        "tool_search_tool"
            | "tool_search"
            | "ToolSearch"
            | "web_search"
            | "web_fetch"
            | "WebSearch"
            | "WebFetch"
            | "doSearch"
            | "form-github-search-query"
            | "show-github-search-result"
    ) || name.starts_with("github-mcp-server-")
        || name.starts_with("GitHub-")
        || name.starts_with("mcp__github")
        || name.starts_with("session_store")
    {
        return "other";
    }
    if name.starts_with("cctxNative") || name.starts_with("code-context-native") {
        return "native";
    }
    if name.starts_with("code-context-")
        || name.starts_with("mcp__code-context__")
        || name.starts_with("mcp_code_context")
        || matches!(
            name,
            "code_files"
                | "code_grep"
                | "code_context"
                | "code_read"
                | "code_status"
                | "code_sync"
                | "code_workspaces"
        )
    {
        return "mcp";
    }
    let lowered = name.to_ascii_lowercase();
    if matches!(
        name,
        "rg" | "view"
            | "glob"
            | "Read"
            | "Grep"
            | "Glob"
            | "usages"
            | "read_file"
            | "grep_search"
            | "file_search"
            | "find_by_name"
            | "list_dir"
            | "copilot_findFiles"
            | "copilot_findTextInFiles"
            | "copilot_readFile"
    ) || lowered.contains("search")
        || lowered.contains("grep")
        || lowered.contains("readfile")
    {
        return "builtin_search_read";
    }
    "other"
}

fn is_lookup_tool_name(name: &str) -> bool {
    classify_tool_group(name) == "builtin_search_read"
        || [
            "code_files",
            "code_grep",
            "code_context",
            "code_read",
            "codeFiles",
            "codeGrep",
            "codeContext",
            "codeRead",
            "NativeFiles",
            "NativeGrep",
            "NativeContext",
            "NativeRead",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

fn read_call_shape(args: &Value) -> (usize, bool, bool) {
    let mut batch_ranges = 1usize;
    let mut has_anchor = false;
    if let Some(ranges) = args.get("ranges").and_then(Value::as_array) {
        batch_ranges = ranges.len().max(1);
        has_anchor = ranges.iter().any(|item| {
            item.get("path")
                .or_else(|| item.get("filePath"))
                .and_then(Value::as_str)
                .is_some_and(|p| p.contains('#'))
        });
    }
    if !has_anchor {
        has_anchor = args
            .get("path")
            .or_else(|| args.get("filePath"))
            .and_then(Value::as_str)
            .is_some_and(|p| p.contains('#'));
    }
    let around_line = args.get("around_line").is_some();
    (batch_ranges, has_anchor, around_line)
}

fn is_turn_compressed(name: &str, args: &Value) -> bool {
    if name.ends_with("code_context") || name.ends_with("codeContext") || name.ends_with("NativeContext") {
        return true;
    }
    if name.ends_with("code_read") || name.ends_with("codeRead") || name.ends_with("NativeRead") {
        let (batch_ranges, has_anchor, around_line) = read_call_shape(args);
        return batch_ranges > 1 || has_anchor || around_line;
    }
    false
}

fn estimate_tokens_saved(name: &str, turn_compressed: bool, payload_tokens: u64, success: Option<bool>) -> u64 {
    if success != Some(true) || !matches!(classify_tool_group(name), "native" | "mcp") || !is_lookup_tool_name(name) {
        return 0;
    }
    if name.ends_with("code_context") || name.ends_with("codeContext") || name.ends_with("NativeContext") {
        return 1600_u64.max((payload_tokens as f64 * 1.85).round() as u64);
    }
    if name.ends_with("code_read") || name.ends_with("codeRead") || name.ends_with("NativeRead") {
        let floor = if turn_compressed { 1400 } else { 900 };
        let mult = if turn_compressed { 1.75 } else { 1.10 };
        return floor.max((payload_tokens as f64 * mult).round() as u64);
    }
    if [
        "code_grep",
        "codeGrep",
        "NativeGrep",
        "code_files",
        "codeFiles",
        "NativeFiles",
    ]
    .iter()
    .any(|s| name.ends_with(s))
    {
        return 1100_u64.max((payload_tokens as f64 * 1.35).round() as u64);
    }
    0
}

fn avoided_turns_for_call(
    name: &str,
    batch_ranges: usize,
    has_anchor: bool,
    around_line: bool,
    sequential_slice: bool,
    success: Option<bool>,
) -> f64 {
    if success != Some(true) || !matches!(classify_tool_group(name), "native" | "mcp") || !is_lookup_tool_name(name) {
        return 0.0;
    }
    if name.ends_with("code_context") || name.ends_with("codeContext") || name.ends_with("NativeContext") {
        return 1.0;
    }
    if name.ends_with("code_read") || name.ends_with("codeRead") || name.ends_with("NativeRead") {
        if sequential_slice {
            return 0.0;
        }
        if batch_ranges > 1 {
            return (0.65 * (batch_ranges - 1) as f64 + 0.35).min(3.0);
        }
        if has_anchor {
            return 1.0;
        }
        if around_line {
            return 0.5;
        }
        return 0.25;
    }
    0.25
}

/// Reads the `CCTX_TRACE {...}` timing footer that the native extension appends to results.
fn extract_trace_total_ms(val: &Value, depth: usize) -> Option<f64> {
    if depth > 6 {
        return None;
    }
    match val {
        Value::String(s) => {
            let start = if s.starts_with("CCTX_TRACE {") {
                0
            } else {
                s.rfind("\nCCTX_TRACE {")? + 1
            };
            let body = s[start + "CCTX_TRACE ".len()..].lines().next()?;
            serde_json::from_str::<Value>(body).ok()?.get("total_ms")?.as_f64()
        }
        Value::Object(map) => map.values().find_map(|v| extract_trace_total_ms(v, depth + 1)),
        Value::Array(items) => items.iter().find_map(|v| extract_trace_total_ms(v, depth + 1)),
        _ => None,
    }
}

fn extract_result_chars(val: &Value, depth: usize) -> (usize, bool, bool) {
    if depth > 6 {
        return (0, false, false);
    }
    if let Some(s) = val.as_str() {
        let trimmed = match s.rfind("\nCCTX_TRACE {") {
            Some(idx) => &s[..idx],
            None => s,
        };
        return (
            trimmed.len(),
            trimmed.starts_with("Output too large to read at once"),
            trimmed.contains("SEQUENTIAL_SLICE #"),
        );
    }
    if let Some(obj) = val.as_object() {
        for key in ["content", "text", "value", "output"] {
            if let Some(child) = obj.get(key) {
                let (chars, offloaded, seq_slice) = extract_result_chars(child, depth + 1);
                if chars > 0 {
                    return (chars, offloaded, seq_slice);
                }
            }
        }
    }
    if let Some(arr) = val.as_array() {
        let mut total = 0;
        let mut any_offload = false;
        let mut any_seq = false;
        for item in arr {
            let (c, off, seq) = extract_result_chars(item, depth + 1);
            total += c;
            any_offload |= off;
            any_seq |= seq;
        }
        return (total, any_offload, any_seq);
    }
    (0, false, false)
}

fn fmt_tok(val: u64) -> String {
    if val >= 1_000_000_000 {
        format!("{:.2}B", val as f64 / 1_000_000_000.0)
    } else if val >= 1_000_000 {
        format!("{:.2}M", val as f64 / 1_000_000.0)
    } else if val >= 1_000 {
        format!("{:.1}K", val as f64 / 1_000.0)
    } else {
        val.to_string()
    }
}

fn fmt_pct(val: Option<f64>) -> String {
    match val {
        Some(v) => format!("{v:.1}%"),
        None => "-".into(),
    }
}

fn fmt_bar(pct: Option<f64>, width: usize) -> String {
    let Some(v) = pct else {
        return format!("{}     -", "░".repeat(width));
    };
    let clamped = v.clamp(0.0, 100.0);
    let filled = ((clamped / 100.0) * width as f64).round() as usize;
    format!(
        "{}{} {:5.1}%",
        "█".repeat(filled),
        "░".repeat(width.saturating_sub(filled)),
        clamped
    )
}

pub(crate) fn render_box_table(title: &str, headers: &[&str], rows: &[Vec<String>], right_align: &[bool]) -> String {
    let fallback = vec![
        std::iter::once("(none)".to_string())
            .chain((1..headers.len()).map(|_| "-".to_string()))
            .collect::<Vec<_>>(),
    ];
    let active_rows = if rows.is_empty() { &fallback } else { rows };
    let widths: Vec<usize> = (0..headers.len())
        .map(|col| {
            let h_len = headers[col].chars().count();
            let r_max = active_rows
                .iter()
                .map(|r| r.get(col).map_or(0, |s| s.chars().count()))
                .max()
                .unwrap_or(0);
            h_len.max(r_max)
        })
        .collect();
    let pad = |text: &str, width: usize, right: bool| -> String {
        let count = text.chars().count();
        let pad_len = width.saturating_sub(count);
        if right {
            format!("{}{}", " ".repeat(pad_len), text)
        } else {
            format!("{}{}", text, " ".repeat(pad_len))
        }
    };
    let join_border = |left: &str, mid: &str, right: &str| -> String {
        let segs: Vec<String> = widths.iter().map(|w| "─".repeat(*w + 2)).collect();
        format!("{left}{}{right}", segs.join(mid))
    };
    let mut out = Vec::with_capacity(active_rows.len() + 5);
    out.push(format!("  {title}"));
    out.push(join_border("┌", "┬", "┐"));
    let head_cells: Vec<String> = (0..headers.len())
        .map(|i| pad(headers[i], widths[i], *right_align.get(i).unwrap_or(&false)))
        .collect();
    out.push(format!("│ {} │", head_cells.join(" │ ")));
    out.push(join_border("├", "┼", "┤"));
    for row in active_rows {
        let cells: Vec<String> = (0..headers.len())
            .map(|i| {
                let text = row.get(i).map_or("-", String::as_str);
                pad(text, widths[i], *right_align.get(i).unwrap_or(&false))
            })
            .collect();
        out.push(format!("│ {} │", cells.join(" │ ")));
    }
    out.push(join_border("└", "┴", "┘"));
    out.join("\n")
}

pub fn usage(root: &Path, hours: f64, as_json: bool) -> Result<String> {
    if !hours.is_finite() || hours <= 0.0 {
        return Err(invalid("hours must be greater than zero").into());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64();

    let resolved_root = catalog::resolve_root(root).ok();
    let daemon_status = if let Some(ref r) = resolved_root {
        daemon::request(&daemon::Request {
            root: Some(r.clone()),
            name: "code_status".into(),
            arguments: json!({}),
            compact: true,
        })
        .ok()
    } else {
        daemon::request(&daemon::Request {
            root: None,
            name: "ping".into(),
            arguments: json!({}),
            compact: true,
        })
        .ok()
    };

    let state_home = std::env::var_os("XDG_STATE_HOME")
        .or_else(|| std::env::var_os("HOME"))
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let registered_specs = catalog::load().unwrap_or_default();
    usage_from_home(&state_home, hours, now, daemon_status, &registered_specs, as_json)
}

fn first_target_path_str(args: &Value) -> Option<&str> {
    if let Some(s) = args.get("path").or_else(|| args.get("filePath")).and_then(Value::as_str) {
        return Some(s);
    }
    if let Some(p) = args.get("paths") {
        if let Some(s) = p.as_str() {
            return Some(s);
        }
        if let Some(arr) = p.as_array() {
            return arr.iter().find_map(Value::as_str);
        }
    }
    if let Some(arr) = args.get("ranges").and_then(Value::as_array) {
        return arr
            .iter()
            .find_map(|r| r.get("path").or_else(|| r.get("filePath")).and_then(Value::as_str));
    }
    None
}

struct SpecHostFilter {
    globs: Option<globset::GlobSet>,
    excludes: Option<globset::GlobSet>,
}

fn build_spec_host_filter(spec: &crate::catalog::Spec) -> SpecHostFilter {
    let globs = if spec.globs.is_empty() {
        None
    } else {
        let mut builder = globset::GlobSetBuilder::new();
        for g in &spec.globs {
            if let Ok(glob) = globset::Glob::new(g) {
                builder.add(glob);
            }
        }
        builder.build().ok()
    };
    let mut exc_builder = globset::GlobSetBuilder::new();
    let mut has_exc = false;
    for ex in &spec.excludes {
        if let Ok(glob) = globset::Glob::new(ex) {
            exc_builder.add(glob);
            has_exc = true;
        }
    }
    let gitignore_path = spec.root.join(".gitignore");
    if let Ok(body) = std::fs::read_to_string(&gitignore_path)
        && !body.lines().any(|l| l.trim_start().starts_with('!'))
    {
        for raw in body.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let clean = line.trim_start_matches('/').trim_end_matches('/');
            if clean.is_empty() {
                continue;
            }
            for pat in [
                clean.to_string(),
                format!("{clean}/**"),
                format!("**/{clean}"),
                format!("**/{clean}/**"),
            ] {
                if let Ok(glob) = globset::Glob::new(&pat) {
                    exc_builder.add(glob);
                    has_exc = true;
                }
            }
        }
    }
    let excludes = if has_exc { exc_builder.build().ok() } else { None };
    SpecHostFilter { globs, excludes }
}

fn infer_call_workspace(
    tool: &str,
    args: &Value,
    session_cwd: Option<&str>,
    registered_specs: &[crate::catalog::Spec],
    git_root_cache: &mut std::collections::BTreeMap<String, Option<String>>,
    spec_filter_cache: &mut std::collections::BTreeMap<PathBuf, SpecHostFilter>,
) -> (Option<String>, Option<String>, bool) {
    let explicit_root = args.get("root").and_then(Value::as_str);
    let raw_target = first_target_path_str(args);
    let probe_abs: Option<PathBuf> = if let Some(r) = explicit_root.filter(|s| s.starts_with('/')) {
        Some(PathBuf::from(r))
    } else if let Some(t) = raw_target.filter(|s| s.starts_with('/')) {
        let clean = t.split('#').next().unwrap_or(t);
        Some(PathBuf::from(clean))
    } else {
        session_cwd.filter(|s| s.starts_with('/')).map(PathBuf::from)
    };
    let Some(probe) = probe_abs else {
        return (session_cwd.map(str::to_owned), None, false);
    };
    if let Some(spec) = registered_specs
        .iter()
        .filter(|s| probe.starts_with(&s.root))
        .max_by_key(|s| s.root.components().count())
    {
        let mut host_only = false;
        if classify_tool_group(tool) == "builtin_search_read"
            && let Some(t) = raw_target
        {
            let clean = t.split('#').next().unwrap_or(t);
            let candidate = if clean.starts_with('/') {
                PathBuf::from(clean)
            } else {
                spec.root.join(clean)
            };
            let filter = spec_filter_cache
                .entry(spec.root.clone())
                .or_insert_with(|| build_spec_host_filter(spec));
            if let Ok(rel) = candidate.strip_prefix(&spec.root)
                && let Some(ref ex_set) = filter.excludes
                && ex_set.is_match(rel)
            {
                host_only = true;
            }
            if !host_only && let Ok(meta) = std::fs::metadata(&candidate) {
                if meta.is_file() && meta.len() > spec.policy.max_file_bytes as u64 {
                    host_only = true;
                } else if meta.is_file()
                    && let Some(ref g_set) = filter.globs
                    && let Some(fname) = candidate.file_name()
                    && !g_set.is_match(fname)
                {
                    host_only = true;
                }
            }
        }
        return (Some(spec.root.to_string_lossy().into_owned()), None, host_only);
    }
    if registered_specs.is_empty() {
        return (session_cwd.map(str::to_owned), None, false);
    }
    let cache_key = probe
        .parent()
        .unwrap_or(probe.as_path())
        .to_string_lossy()
        .into_owned();
    let git_root = git_root_cache
        .entry(cache_key)
        .or_insert_with(|| {
            probe
                .ancestors()
                .take(8)
                .find(|anc| anc.join(".git").exists())
                .map(|anc| anc.to_string_lossy().into_owned())
        })
        .clone();
    if let Some(repo) = git_root {
        return (Some(repo.clone()), Some(repo), false);
    }
    (session_cwd.map(str::to_owned), None, false)
}

fn usage_from_home(
    state_home: &Path,
    hours: f64,
    now: f64,
    daemon_status: Option<Value>,
    registered_specs: &[crate::catalog::Spec],
    as_json: bool,
) -> Result<String> {
    let cutoff = now - hours * 3600.0;
    let session_dir = state_home.join(".copilot").join("session-state");
    let mut session_files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&session_dir) {
        for entry in entries.flatten() {
            let events_path = entry.path().join("events.jsonl");
            if let Ok(meta) = events_path.metadata() {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0.0, |d| d.as_secs_f64());
                if mtime >= cutoff {
                    session_files.push(events_path);
                }
            }
        }
    }
    session_files.sort();

    #[derive(Default)]
    struct TurnRec {
        ts: f64,
        session: String,
        agent_id: String,
        model: String,
        display_name: String,
        effort: String,
        input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: u64,
        duration_ms: f64,
        nano_aiu: u64,
        rates: TokenRates,
        bpe_saved_aic: f64,
    }

    impl TurnRec {
        fn effective_turn_aic(&self) -> f64 {
            if self.nano_aiu > 0 {
                self.nano_aiu as f64 / 1_000_000_000.0
            } else {
                let uncached = self
                    .input_tokens
                    .saturating_sub(self.cache_read_tokens)
                    .saturating_sub(self.cache_write_tokens);
                (uncached as f64 * self.rates.input)
                    + (self.cache_read_tokens as f64 * self.rates.cache_read)
                    + (self.cache_write_tokens as f64 * self.rates.cache_write)
                    + (self.output_tokens as f64 * self.rates.output)
            }
        }
    }

    #[derive(Default, Clone)]
    struct CallRec {
        session: String,
        agent_id: String,
        seq: usize,
        cwd: Option<String>,
        unregistered_root: Option<String>,
        tool: String,
        target: String,
        model: Option<String>,
        started_at: Option<f64>,
        completed_at: Option<f64>,
        success: Option<bool>,
        payload_tokens: u64,
        offloaded: bool,
        sequential_slice: bool,
        cache_miss: bool,
        cache_load_failed: bool,
        error_detail: &'static str,
        trace_total_ms: Option<f64>,
        batch_ranges: usize,
        has_anchor: bool,
        around_line: bool,
        turn_compressed: bool,
        non_source_host_only: bool,
        pre_edit_view: bool,
        saved_tokens: u64,
        compounded_tokens: u64,
        payload_saved_aic: f64,
        avoided_turns: f64,
        turn_saved_tokens: u64,
        turn_saved_aic: f64,
    }

    let mut turns: Vec<TurnRec> = Vec::new();
    let mut calls: std::collections::BTreeMap<(String, String), CallRec> = std::collections::BTreeMap::new();
    let mut session_cwds: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut active_sessions: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut git_root_cache: std::collections::BTreeMap<String, Option<String>> =
        std::collections::BTreeMap::new();
    let mut spec_filter_cache: std::collections::BTreeMap<PathBuf, SpecHostFilter> =
        std::collections::BTreeMap::new();
    let mut seq_counter: usize = 0;

    for path in &session_files {
        let session_id = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".into());
        let Ok(file) = std::fs::File::open(path) else {
            continue;
        };
        let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
        let mut cwd: Option<String> = None;
        let mut last_model_by_agent: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        let mut line = String::with_capacity(4096);
        while {
            line.clear();
            reader.read_line(&mut line).unwrap_or(0) > 0
        } {
            if !line.contains("\"session.usage_record\"")
                && !line.contains("\"tool.execution_")
                && !line.contains("\"session.start\"")
                && !line.contains("\"subagent.started\"")
            {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(event_type) = event.get("type").and_then(Value::as_str) else {
                continue;
            };
            let agent_id = event
                .get("agentId")
                .and_then(Value::as_str)
                .unwrap_or("main")
                .to_owned();
            if event_type == "session.start"
                && let Some(c) = event
                    .get("data")
                    .and_then(|d| d.get("context"))
                    .and_then(|ctx| ctx.get("cwd"))
                    .and_then(Value::as_str)
            {
                cwd = Some(c.to_owned());
                session_cwds.insert(session_id.clone(), c.to_owned());
            } else if event_type == "subagent.started"
                && let Some(m) = event
                    .get("data")
                    .and_then(|d| d.get("model"))
                    .and_then(Value::as_str)
            {
                last_model_by_agent.insert(agent_id.clone(), m.to_owned());
            } else if event_type == "session.usage_record"
                && let Some(m) = event
                    .get("data")
                    .and_then(|d| d.get("usage"))
                    .and_then(|u| u.get("model"))
                    .and_then(Value::as_str)
            {
                last_model_by_agent.insert(agent_id.clone(), m.to_owned());
            }
            let Some(ts) = event_unix_ts(&event) else {
                continue;
            };
            if ts < cutoff || ts > now {
                continue;
            }
            if event_type == "session.usage_record" {
                if let Some(u) = event.get("data").and_then(|d| d.get("usage")) {
                    let model = u
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned();
                    let display_name = u
                        .get("modelDisplayName")
                        .and_then(Value::as_str)
                        .unwrap_or(&model)
                        .to_owned();
                    let effort = u
                        .get("reasoningEffort")
                        .and_then(Value::as_str)
                        .unwrap_or("-")
                        .to_owned();
                    let nano_aiu = u
                        .get("copilotUsage")
                        .and_then(|cu| cu.get("totalNanoAiu"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let rates = extract_token_rates(u);
                    active_sessions.insert(session_id.clone());
                    turns.push(TurnRec {
                        ts,
                        session: session_id.clone(),
                        agent_id,
                        model,
                        display_name,
                        effort,
                        input_tokens: u.get("inputTokens").and_then(Value::as_u64).unwrap_or(0),
                        cache_read_tokens: u.get("cacheReadTokens").and_then(Value::as_u64).unwrap_or(0),
                        cache_write_tokens: u.get("cacheWriteTokens").and_then(Value::as_u64).unwrap_or(0),
                        output_tokens: u.get("outputTokens").and_then(Value::as_u64).unwrap_or(0),
                        reasoning_tokens: u.get("reasoningTokens").and_then(Value::as_u64).unwrap_or(0),
                        duration_ms: u.get("duration").and_then(Value::as_f64).unwrap_or(0.0),
                        nano_aiu,
                        rates,
                        bpe_saved_aic: 0.0,
                    });
                }
            } else if event_type == "tool.execution_start" {
                let Some(data) = event.get("data") else {
                    continue;
                };
                let Some(call_id) = data.get("toolCallId").and_then(Value::as_str) else {
                    continue;
                };
                active_sessions.insert(session_id.clone());
                let tool = data
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                let args = data.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let target = args
                    .get("path")
                    .or_else(|| args.get("paths"))
                    .or_else(|| args.get("filePath"))
                    .or_else(|| args.get("file_path"))
                    .or_else(|| args.get("input"))
                    .or_else(|| args.get("patch"))
                    .map(Value::to_string)
                    .unwrap_or_default();
                let (effective_cwd, unregistered_root, oversized_host_only) = infer_call_workspace(
                    &tool,
                    &args,
                    cwd.as_deref(),
                    registered_specs,
                    &mut git_root_cache,
                    &mut spec_filter_cache,
                );
                let non_source_host_only = oversized_host_only
                    || (classify_tool_group(&tool) == "builtin_search_read"
                        && [
                            "/tmp/",
                            "/dev/shm/",
                            "/var/tmp/",
                            "copilot-tool-output",
                            "/.copilot",
                            "/.claude",
                            "/.github",
                            "/.vscode",
                            ".png",
                            ".jpg",
                            ".jpeg",
                            ".webp",
                            ".gif",
                            ".pdf",
                            ".vsix",
                        ]
                        .iter()
                        .any(|p| target.contains(p)));
                let call_model = data
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| last_model_by_agent.get(&agent_id).cloned())
                    .or_else(|| last_model_by_agent.get("main").cloned());
                let (batch_ranges, has_anchor, around_line) = read_call_shape(&args);
                seq_counter += 1;
                let entry = calls.entry((session_id.clone(), call_id.to_owned())).or_default();
                entry.session = session_id.clone();
                entry.agent_id = agent_id;
                entry.seq = seq_counter;
                entry.cwd = effective_cwd;
                entry.unregistered_root = unregistered_root;
                entry.tool = tool.clone();
                entry.target = target;
                entry.model = call_model;
                entry.started_at = Some(ts);
                entry.batch_ranges = batch_ranges;
                entry.has_anchor = has_anchor;
                entry.around_line = around_line;
                entry.turn_compressed = is_turn_compressed(&tool, &args);
                entry.non_source_host_only = non_source_host_only;
            } else if event_type == "tool.execution_complete" {
                let Some(data) = event.get("data") else {
                    continue;
                };
                let Some(call_id) = data.get("toolCallId").and_then(Value::as_str) else {
                    continue;
                };
                active_sessions.insert(session_id.clone());
                let entry = calls.entry((session_id.clone(), call_id.to_owned())).or_default();
                if entry.session.is_empty() {
                    entry.session = session_id.clone();
                }
                if entry.agent_id.is_empty() {
                    entry.agent_id = agent_id.clone();
                }
                if entry.model.is_none() {
                    entry.model = data
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| last_model_by_agent.get(&agent_id).cloned())
                        .or_else(|| last_model_by_agent.get("main").cloned());
                }
                entry.completed_at = Some(ts);
                entry.success = data.get("success").and_then(Value::as_bool);
                if let Some(res) = data.get("result") {
                    let (chars, offloaded, seq_slice) = extract_result_chars(res, 0);
                    entry.trace_total_ms = extract_trace_total_ms(res, 0);
                    entry.payload_tokens = chars.div_ceil(4) as u64;
                    entry.offloaded = offloaded;
                    entry.sequential_slice = seq_slice;
                }
                if entry.success == Some(false) {
                    let err_str = data
                        .get("error")
                        .map(Value::to_string)
                        .unwrap_or_default();
                    entry.error_detail = outcome::detail(Some(&err_str));
                    entry.cache_miss = err_str.contains("CACHE_MISS root=");
                    entry.cache_load_failed = err_str.contains("CACHE_LOAD_FAILED root=");
                }
            }
        }
    }

    // Also ingest Claude Code / Claude Agent Host (`~/.claude/projects/**/*.jsonl`) sessions when present.
    let claude_projects_dir = state_home.join(".claude").join("projects");
    if let Ok(proj_entries) = std::fs::read_dir(&claude_projects_dir) {
        let mut claude_files: Vec<PathBuf> = Vec::new();
        for p_entry in proj_entries.flatten() {
            let p_path = p_entry.path();
            if !p_path.is_dir() {
                continue;
            }
            if let Ok(files) = std::fs::read_dir(&p_path) {
                for f_entry in files.flatten() {
                    let f_path = f_entry.path();
                    if f_path.extension().and_then(|e| e.to_str()) == Some("jsonl")
                        && let Ok(meta) = f_path.metadata()
                    {
                        let mtime = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map_or(0.0, |d| d.as_secs_f64());
                        if mtime >= cutoff {
                            claude_files.push(f_path);
                        }
                    }
                }
            }
        }
        claude_files.sort();
        for c_path in &claude_files {
            let fallback_sid = c_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "claude".into());
            let Ok(file) = std::fs::File::open(c_path) else {
                continue;
            };
            let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
            let mut cwd: Option<String> = None;
            let mut last_model: Option<String> = None;
            let mut line = String::with_capacity(4096);
            while {
                line.clear();
                reader.read_line(&mut line).unwrap_or(0) > 0
            } {
                let Ok(event) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(c) = event.get("cwd").and_then(Value::as_str) {
                    cwd = Some(c.to_owned());
                }
                let sid = event
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or(&fallback_sid)
                    .to_owned();
                if let Some(ref c) = cwd {
                    session_cwds.entry(sid.clone()).or_insert_with(|| c.clone());
                }
                let agent_id = event
                    .get("agentId")
                    .or_else(|| event.get("parentUuid").filter(|_| event.get("isSidechain") == Some(&Value::Bool(true))))
                    .and_then(Value::as_str)
                    .unwrap_or("main")
                    .to_owned();
                if let Some(m) = event
                    .get("message")
                    .and_then(|msg| msg.get("model"))
                    .and_then(Value::as_str)
                {
                    last_model = Some(m.to_owned());
                }
                let Some(ts) = event_unix_ts(&event) else {
                    continue;
                };
                if ts < cutoff || ts > now {
                    continue;
                }
                let ev_type = event.get("type").and_then(Value::as_str).unwrap_or("");
                if ev_type == "assistant"
                    && let Some(msg) = event.get("message")
                {
                    if let Some(u) = msg.get("usage") {
                        let model = msg
                            .get("model")
                            .and_then(Value::as_str)
                            .or(last_model.as_deref())
                            .unwrap_or("claude-opus")
                            .to_owned();
                        let uncached_in = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                        let cw_in = u
                            .get("cache_creation_input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let cr_in = u
                            .get("cache_read_input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let total_in_turn = uncached_in + cw_in + cr_in;
                        let out_turn = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                        let rates = if model.contains("opus") {
                            TokenRates {
                                input: 500.0e-6,
                                cache_read: 50.0e-6,
                                cache_write: 625.0e-6,
                                output: 2500.0e-6,
                            }
                        } else if model.contains("haiku") {
                            TokenRates {
                                input: 100.0e-6,
                                cache_read: 10.0e-6,
                                cache_write: 125.0e-6,
                                output: 500.0e-6,
                            }
                        } else {
                            TokenRates {
                                input: 300.0e-6,
                                cache_read: 30.0e-6,
                                cache_write: 375.0e-6,
                                output: 1500.0e-6,
                            }
                        };
                        let est_aic = (uncached_in as f64) * rates.input
                            + (cw_in as f64) * rates.cache_write
                            + (cr_in as f64) * rates.cache_read
                            + (out_turn as f64) * rates.output;
                        active_sessions.insert(sid.clone());
                        turns.push(TurnRec {
                            ts,
                            session: sid.clone(),
                            agent_id: agent_id.clone(),
                            display_name: model.clone(),
                            model,
                            effort: "claude-agent".into(),
                            input_tokens: total_in_turn,
                            cache_read_tokens: cr_in,
                            cache_write_tokens: cw_in,
                            output_tokens: out_turn,
                            reasoning_tokens: 0,
                            duration_ms: 0.0,
                            nano_aiu: (est_aic * 1_000_000_000.0).round() as u64,
                            rates,
                            bpe_saved_aic: 0.0,
                        });
                    }
                    if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
                        for block in blocks {
                            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                                continue;
                            }
                            let Some(call_id) = block.get("id").and_then(Value::as_str) else {
                                continue;
                            };
                            let tool = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown")
                                .to_owned();
                            let args = block.get("input").cloned().unwrap_or_else(|| json!({}));
                            let target = args
                                .get("path")
                                .or_else(|| args.get("paths"))
                                .or_else(|| args.get("filePath"))
                                .or_else(|| args.get("file_path"))
                                .map(Value::to_string)
                                .unwrap_or_default();
                            let (effective_cwd, unregistered_root, host_only) = infer_call_workspace(
                                &tool,
                                &args,
                                cwd.as_deref(),
                                registered_specs,
                                &mut git_root_cache,
                                &mut spec_filter_cache,
                            );
                            let non_source_host_only = host_only
                                || (classify_tool_group(&tool) == "builtin_search_read"
                                    && [
                                        "/tmp/",
                                        "/dev/shm/",
                                        "/var/tmp/",
                                        "/.copilot",
                                        "/.claude",
                                        "/.github",
                                        "/.vscode",
                                        ".png",
                                        ".jpg",
                                        ".pdf",
                                        ".vsix",
                                    ]
                                    .iter()
                                    .any(|p| target.contains(p)));
                            let (batch_ranges, has_anchor, around_line) = read_call_shape(&args);
                            seq_counter += 1;
                            active_sessions.insert(sid.clone());
                            let entry = calls.entry((sid.clone(), call_id.to_owned())).or_default();
                            entry.session = sid.clone();
                            entry.agent_id = agent_id.clone();
                            entry.seq = seq_counter;
                            entry.cwd = effective_cwd;
                            entry.unregistered_root = unregistered_root;
                            entry.tool = tool.clone();
                            entry.target = target;
                            entry.model = last_model.clone();
                            entry.started_at = Some(ts);
                            entry.completed_at = Some(ts);
                            entry.success = Some(true);
                            entry.batch_ranges = batch_ranges;
                            entry.has_anchor = has_anchor;
                            entry.around_line = around_line;
                            entry.turn_compressed = is_turn_compressed(&tool, &args);
                            entry.non_source_host_only = non_source_host_only;
                        }
                    }
                } else if ev_type == "user"
                    && let Some(blocks) = event
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(Value::as_array)
                {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                            continue;
                        }
                        let Some(call_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                            continue;
                        };
                        if let Some(entry) = calls.get_mut(&(sid.clone(), call_id.to_owned())) {
                            entry.completed_at = Some(ts);
                            let is_err = block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                            entry.success = Some(!is_err);
                            if let Some(content) = block.get("content") {
                                let (chars, offloaded, seq_slice) = extract_result_chars(content, 0);
                                entry.payload_tokens = chars.div_ceil(4) as u64;
                                entry.offloaded = offloaded;
                                entry.sequential_slice = seq_slice;
                                if is_err {
                                    let err_str = content.to_string();
                                    entry.error_detail = outcome::detail(Some(&err_str));
                                    entry.cache_miss = err_str.contains("CACHE_MISS root=");
                                    entry.cache_load_failed = err_str.contains("CACHE_LOAD_FAILED root=");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    turns.sort_by(|a, b| {
        a.session
            .cmp(&b.session)
            .then_with(|| a.agent_id.cmp(&b.agent_id))
            .then_with(|| a.ts.total_cmp(&b.ts))
    });

    let bpe_status = crate::prompt_formula::inspect_bpe_surfaces(state_home);
    let bpe_turn_tok = bpe_status.standing_tokens_saved_per_turn;
    for t in &mut turns {
        let prefix_rate = if t.cache_read_tokens > 0 {
            t.rates.cache_read
        } else {
            cold_prefix_rate(t.cache_write_tokens, t.rates)
        };
        t.bpe_saved_aic = (bpe_turn_tok as f64) * prefix_rate;
    }

    struct StreamMeta {
        idxs: Vec<usize>,
        resets: Vec<usize>,
        mean_warm_aic: f64,
        mean_warm_in: f64,
    }

    let mut stream_idxs: std::collections::BTreeMap<(String, String), Vec<usize>> =
        std::collections::BTreeMap::new();
    for (idx, t) in turns.iter().enumerate() {
        stream_idxs
            .entry((t.session.clone(), t.agent_id.clone()))
            .or_default()
            .push(idx);
    }
    let streams: std::collections::BTreeMap<(String, String), StreamMeta> = stream_idxs
        .into_iter()
        .map(|(key, idxs)| {
            let mut resets = Vec::new();
            for k in 1..idxs.len() {
                let prev_in = turns[idxs[k - 1]].input_tokens;
                let cur_in = turns[idxs[k]].input_tokens;
                if prev_in > 60_000 && (cur_in as f64) < (prev_in as f64) * 0.65 {
                    resets.push(k);
                }
            }
            let warm: Vec<usize> = idxs
                .iter()
                .copied()
                .filter(|&i| turns[i].cache_read_tokens > 0)
                .collect();
            let sample = if warm.is_empty() { &idxs } else { &warm };
            let denom = sample.len().max(1) as f64;
            let mean_warm_aic = sample.iter().map(|&i| turns[i].effective_turn_aic()).sum::<f64>() / denom;
            let mean_warm_in = sample.iter().map(|&i| turns[i].input_tokens as f64).sum::<f64>() / denom;
            (
                key,
                StreamMeta {
                    idxs,
                    resets,
                    mean_warm_aic,
                    mean_warm_in,
                },
            )
        })
        .collect();

    let mut valid_calls: Vec<CallRec> = calls
        .into_values()
        .filter(|c| c.started_at.is_some_and(|s| s >= cutoff))
        .map(|mut c| {
            c.saved_tokens = estimate_tokens_saved(&c.tool, c.turn_compressed, c.payload_tokens, c.success);
            c.avoided_turns = avoided_turns_for_call(
                &c.tool,
                c.batch_ranges.max(1),
                c.has_anchor,
                c.around_line,
                c.sequential_slice,
                c.success,
            );
            c
        })
        .collect();
    valid_calls.sort_by(|a, b| a.session.cmp(&b.session).then_with(|| a.seq.cmp(&b.seq)));
    for i in 0..valid_calls.len() {
        if matches!(valid_calls[i].tool.as_str(), "view" | "Read") && !valid_calls[i].non_source_host_only {
            let sess = &valid_calls[i].session;
            let raw_view_path = valid_calls[i]
                .target
                .trim_matches('"')
                .split('#')
                .next()
                .unwrap_or("")
                .to_owned();
            let view_basename = Path::new(&raw_view_path)
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| n.len() >= 5 && n.contains('.'))
                .unwrap_or("");
            let immediate_edit = valid_calls
                .iter()
                .skip(i + 1)
                .take(4)
                .any(|next| {
                    &next.session == sess
                        && matches!(next.tool.as_str(), "edit" | "Edit" | "MultiEdit" | "apply_patch")
                });
            let same_file_batched_edit = !raw_view_path.is_empty()
                && valid_calls
                    .iter()
                    .skip(i + 1)
                    .take(28)
                    .take_while(|next| &next.session == sess)
                    .any(|next| {
                        matches!(next.tool.as_str(), "edit" | "Edit" | "MultiEdit" | "apply_patch")
                            && (next.target.contains(&raw_view_path)
                                || (!view_basename.is_empty() && next.target.contains(view_basename)))
                    });
            valid_calls[i].pre_edit_view = immediate_edit || same_file_batched_edit;
        }
    }
    for c in &mut valid_calls {
        let eligible_cctx = is_lookup_tool_name(&c.tool)
            && !c.non_source_host_only
            && !c.pre_edit_view
            && matches!(classify_tool_group(&c.tool), "native" | "mcp")
            && c.success == Some(true);
        if !eligible_cctx {
            continue;
        }
        let stream_opt = streams
            .get(&(c.session.clone(), c.agent_id.clone()))
            .or_else(|| streams.get(&(c.session.clone(), "main".into())));
        if let Some(stream) = stream_opt
            && !stream.idxs.is_empty()
        {
            let t_ref = c.completed_at.or(c.started_at).unwrap_or(0.0);
            if let Some(pos) = (0..stream.idxs.len()).find(|&k| turns[stream.idxs[k]].ts >= t_ref) {
                let end_pos = stream
                    .resets
                    .iter()
                    .copied()
                    .find(|&r| r > pos)
                    .unwrap_or(stream.idxs.len());
                let t0 = &turns[stream.idxs[pos]];
                c.payload_saved_aic =
                    (c.saved_tokens as f64) * cold_prefix_rate(t0.cache_write_tokens, t0.rates);
                for j in (pos + 1)..end_pos {
                    let tj = &turns[stream.idxs[j]];
                    let prev_in = turns[stream.idxs[j - 1]].input_tokens.max(1) as f64;
                    let hit_frac = (tj.cache_read_tokens as f64 / prev_in).clamp(0.0, 1.0);
                    let rate_j = hit_frac * tj.rates.cache_read
                        + (1.0 - hit_frac) * cold_prefix_rate(tj.cache_write_tokens, tj.rates);
                    c.payload_saved_aic += (c.saved_tokens as f64) * rate_j;
                    c.compounded_tokens += c.saved_tokens;
                }
            } else {
                let t_last = &turns[*stream.idxs.last().unwrap()];
                c.payload_saved_aic =
                    (c.saved_tokens as f64) * cold_prefix_rate(t_last.cache_write_tokens, t_last.rates);
            }
            c.turn_saved_aic = c.avoided_turns * stream.mean_warm_aic;
            c.turn_saved_tokens = (c.avoided_turns * stream.mean_warm_in).round() as u64;
        } else {
            c.compounded_tokens = c.saved_tokens * COMPOUND_CACHE_MULTIPLIER;
            c.payload_saved_aic = (c.saved_tokens as f64) * AIC_PER_SAVED_TOKEN;
            c.turn_saved_aic = c.avoided_turns * AIC_PER_AVOIDED_TURN;
            c.turn_saved_tokens = (c.avoided_turns * TOKENS_PER_AVOIDED_TURN).round() as u64;
        }
    }

    let is_eligible_lookup = |c: &&CallRec| -> bool {
        is_lookup_tool_name(&c.tool) && !c.non_source_host_only && !c.pre_edit_view
    };
    let lookup_calls: Vec<&CallRec> = valid_calls.iter().filter(is_eligible_lookup).collect();
    let cctx_calls: Vec<&CallRec> = lookup_calls
        .iter()
        .copied()
        .filter(|c| matches!(classify_tool_group(&c.tool), "native" | "mcp"))
        .collect();
    let non_source_cnt = valid_calls.iter().filter(|c| c.non_source_host_only).count();
    let pre_edit_cnt = valid_calls.iter().filter(|c| c.pre_edit_view).count();
    let seq_slice_cnt = cctx_calls.iter().filter(|c| c.sequential_slice).count();
    let cctx_misses = cctx_calls.iter().filter(|c| c.cache_miss || c.cache_load_failed).count();
    let cctx_warm = cctx_calls.len().saturating_sub(cctx_misses);
    let cctx_warm_pct = (!cctx_calls.is_empty()).then(|| 100.0 * cctx_warm as f64 / cctx_calls.len() as f64);
    let cctx_select_pct = (!lookup_calls.is_empty()).then(|| 100.0 * cctx_calls.len() as f64 / lookup_calls.len() as f64);

    let bpe_saved_tokens: u64 = (turns.len() as u64) * bpe_turn_tok;
    let bpe_saved_aic: f64 = turns.iter().map(|t| t.bpe_saved_aic).sum();
    let bpe_surface_pct = (bpe_status.total_surfaces > 0)
        .then(|| 100.0 * bpe_status.active_surfaces as f64 / bpe_status.total_surfaces as f64);

    let total_in: u64 = turns.iter().map(|t| t.input_tokens).sum();
    let total_cache: u64 = turns.iter().map(|t| t.cache_read_tokens).sum();
    let total_out: u64 = turns.iter().map(|t| t.output_tokens).sum();
    let total_rsn: u64 = turns.iter().map(|t| t.reasoning_tokens).sum();
    let total_nano_aiu: u64 = turns.iter().map(|t| t.nano_aiu).sum();
    let total_used_aic = total_nano_aiu as f64 / 1_000_000_000.0;
    let prompt_cache_pct = (total_in > 0).then(|| 100.0 * total_cache as f64 / total_in as f64);
    let direct_saved: u64 = cctx_calls.iter().map(|c| c.saved_tokens).sum();
    let compounded_saved: u64 = cctx_calls.iter().map(|c| c.compounded_tokens).sum();
    let effective_reread_mult = if direct_saved > 0 {
        compounded_saved as f64 / direct_saved as f64
    } else if turns.is_empty() {
        COMPOUND_CACHE_MULTIPLIER as f64
    } else {
        0.0
    };
    let avoided_turns_total: f64 = cctx_calls.iter().map(|c| c.avoided_turns).sum();
    let turn_comp_cnt = avoided_turns_total.round() as u64;
    let turn_saved: u64 = cctx_calls.iter().map(|c| c.turn_saved_tokens).sum();
    let total_saved = direct_saved + compounded_saved + turn_saved + bpe_saved_tokens;
    let payload_saved_aic_total: f64 = cctx_calls.iter().map(|c| c.payload_saved_aic).sum();
    let turn_saved_aic_total: f64 = cctx_calls.iter().map(|c| c.turn_saved_aic).sum();
    let total_saved_aic = payload_saved_aic_total + turn_saved_aic_total + bpe_saved_aic;
    let savings_pct = if total_used_aic + total_saved_aic > 0.0 {
        100.0 * total_saved_aic / (total_used_aic + total_saved_aic)
    } else {
        0.0
    };

    let mut model_ids: std::collections::BTreeSet<String> = turns.iter().map(|t| t.model.clone()).collect();
    for c in &valid_calls {
        if let Some(ref m) = c.model {
            model_ids.insert(m.clone());
        }
    }
    let mut model_entries = Vec::new();
    for m_id in model_ids {
        let m_turns: Vec<&TurnRec> = turns.iter().filter(|t| t.model == m_id).collect();
        let m_calls: Vec<&CallRec> = valid_calls
            .iter()
            .filter(|c| c.model.as_deref() == Some(m_id.as_str()))
            .collect();
        let m_lookup: Vec<&CallRec> = m_calls.iter().copied().filter(is_eligible_lookup).collect();
        let m_cctx: Vec<&CallRec> = m_lookup
            .iter()
            .copied()
            .filter(|c| matches!(classify_tool_group(&c.tool), "native" | "mcp"))
            .collect();
        let m_misses = m_cctx.iter().filter(|c| c.cache_miss || c.cache_load_failed).count();
        let m_warm = m_cctx.len().saturating_sub(m_misses);
        let m_warm_pct = (!m_cctx.is_empty()).then(|| 100.0 * m_warm as f64 / m_cctx.len() as f64);
        let m_adopt_pct = (!m_lookup.is_empty()).then(|| 100.0 * m_cctx.len() as f64 / m_lookup.len() as f64);
        let display = m_turns
            .first()
            .map(|t| t.display_name.clone())
            .unwrap_or_else(|| m_id.clone());
        let effort = m_turns
            .first()
            .map(|t| t.effort.clone())
            .unwrap_or_else(|| "-".into());
        let mut sess_set: std::collections::BTreeSet<&str> = m_turns.iter().map(|t| t.session.as_str()).collect();
        for c in &m_calls {
            sess_set.insert(c.session.as_str());
        }
        let in_t: u64 = m_turns.iter().map(|t| t.input_tokens).sum();
        let cr_t: u64 = m_turns.iter().map(|t| t.cache_read_tokens).sum();
        let out_t: u64 = m_turns.iter().map(|t| t.output_tokens).sum();
        let rsn_t: u64 = m_turns.iter().map(|t| t.reasoning_tokens).sum();
        let m_nano_aiu: u64 = m_turns.iter().map(|t| t.nano_aiu).sum();
        let m_used_aic = m_nano_aiu as f64 / 1_000_000_000.0;
        let p_cache_pct = (in_t > 0).then(|| 100.0 * cr_t as f64 / in_t as f64);
        let m_direct_saved: u64 = m_cctx.iter().map(|c| c.saved_tokens).sum();
        let m_compounded_saved: u64 = m_cctx.iter().map(|c| c.compounded_tokens).sum();
        let m_turn_saved: u64 = m_cctx.iter().map(|c| c.turn_saved_tokens).sum();
        let m_bpe_saved: u64 = (m_turns.len() as u64) * bpe_turn_tok;
        let m_total_saved = m_direct_saved + m_compounded_saved + m_turn_saved + m_bpe_saved;
        let m_saved_aic: f64 = m_cctx
            .iter()
            .map(|c| c.payload_saved_aic + c.turn_saved_aic)
            .sum::<f64>()
            + m_turns.iter().map(|t| t.bpe_saved_aic).sum::<f64>();
        let mut durs: Vec<f64> = m_turns.iter().map(|t| t.duration_ms).filter(|d| *d > 0.0).collect();
        durs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med_turn = (!durs.is_empty()).then(|| durs[durs.len() / 2]);
        let mut m_cold_turns = 0usize;
        let mut m_cold_excess_aic = 0.0_f64;
        for t in &m_turns {
            if t.input_tokens >= 100_000
                && (t.cache_read_tokens as f64) < (t.input_tokens as f64) * 0.50
                && t.rates.input > t.rates.cache_read
            {
                m_cold_turns += 1;
                let warm_est = (t.input_tokens as f64)
                    * (0.95 * t.rates.cache_read
                        + 0.05 * cold_prefix_rate(t.cache_write_tokens, t.rates))
                    + (t.output_tokens as f64) * t.rates.output;
                m_cold_excess_aic += (t.effective_turn_aic() - warm_est).max(0.0);
            }
        }
        model_entries.push(json!({
            "model": m_id,
            "display_name": display,
            "effort": effort,
            "sessions": sess_set.len(),
            "turns": m_turns.len(),
            "input_tokens": in_t,
            "cache_read_tokens": cr_t,
            "prompt_cache_hit_percent": p_cache_pct,
            "output_tokens": out_t,
            "reasoning_tokens": rsn_t,
            "used_aic": m_used_aic,
            "used_usd_0001": m_used_aic * 0.001,
            "used_usd_api_001": m_used_aic * 0.01,
            "cold_cache_turns": m_cold_turns,
            "cold_cache_excess_aic": m_cold_excess_aic,
            "cctx_calls": m_cctx.len(),
            "lookup_calls": m_lookup.len(),
            "cctx_cache_hit_percent": m_warm_pct,
            "cctx_adoption_percent": m_adopt_pct,
            "estimated_total_tokens_saved": m_total_saved,
            "saved_aic": m_saved_aic,
            "saved_usd_0001": m_saved_aic * 0.001,
            "saved_usd_api_001": m_saved_aic * 0.01,
            "median_turn_ms": med_turn,
        }));
    }
    model_entries.sort_by(|a, b| {
        b["turns"]
            .as_u64()
            .cmp(&a["turns"].as_u64())
            .then_with(|| b["cctx_calls"].as_u64().cmp(&a["cctx_calls"].as_u64()))
    });

    let call_facts: Vec<CallFacts> = valid_calls
        .iter()
        .map(|c| CallFacts {
            workspace: c
                .cwd
                .as_deref()
                .and_then(|p| Path::new(p).file_name())
                .map_or_else(|| "-".into(), |n| n.to_string_lossy().into_owned()),
            session: c.session.clone(),
            completed_at: c.completed_at,
            host_delay_ms: match (c.started_at, c.completed_at, c.trace_total_ms) {
                (Some(s), Some(e), Some(total)) if e >= s => Some(((e - s) * 1000.0 - total).max(0.0)),
                _ => None,
            },
            tool: c.tool.clone(),
            group: classify_tool_group(&c.tool),
            success: c.success,
            offloaded: c.offloaded,
            error_detail: c.error_detail,
            duration_ms: match (c.started_at, c.completed_at) {
                (Some(s), Some(e)) if e >= s => Some((e - s) * 1000.0),
                _ => None,
            },
            payload_tokens: c.payload_tokens,
            eligible_lookup: is_eligible_lookup(&c),
            sequential_slice: c.sequential_slice,
            unregistered_root: c.unregistered_root.clone(),
        })
        .collect();
    let session_facts: Vec<SessionFacts> = active_sessions
        .iter()
        .map(|id| SessionFacts {
            id: id.clone(),
            workspace: session_cwds
                .get(id)
                .and_then(|c| Path::new(c).file_name())
                .map_or_else(|| "-".into(), |n| n.to_string_lossy().into_owned()),
            turns: turns.iter().filter(|t| &t.session == id).count(),
            used_aic: turns.iter().filter(|t| &t.session == id).map(|t| t.nano_aiu).sum::<u64>() as f64
                / 1_000_000_000.0,
        })
        .collect();
    let model_facts: Vec<ModelFacts> = model_entries
        .iter()
        .map(|m| ModelFacts {
            name: format!(
                "{} ({})",
                m["display_name"].as_str().unwrap_or("-"),
                m["effort"].as_str().unwrap_or("-")
            ),
            turns: m["turns"].as_u64().unwrap_or(0) as usize,
            lookups: m["lookup_calls"].as_u64().unwrap_or(0) as usize,
            cctx_calls: m["cctx_calls"].as_u64().unwrap_or(0) as usize,
            used_aic: m["used_aic"].as_f64().unwrap_or(0.0),
            saved_aic: m["saved_aic"].as_f64().unwrap_or(0.0),
            cold_turns: m["cold_cache_turns"].as_u64().unwrap_or(0) as usize,
            cold_excess_aic: m["cold_cache_excess_aic"].as_f64().unwrap_or(0.0),
            output_tokens: m["output_tokens"].as_u64().unwrap_or(0),
            reasoning_tokens: m["reasoning_tokens"].as_u64().unwrap_or(0),
        })
        .collect();
    let insights = usage_insights::analyze(&call_facts, &session_facts, &model_facts, total_out, total_rsn);

    if as_json {
        let payload = json!({
            "hours": hours,
            "sessions": active_sessions.len(),
            "daemon": daemon_status,
            "token_summary": {
                "total_turns": turns.len(),
                "input_tokens": total_in,
                "cache_read_tokens": total_cache,
                "prompt_cache_hit_percent": prompt_cache_pct,
                "output_tokens": total_out,
                "reasoning_tokens": total_rsn,
                "total_llm_tokens": total_in + total_out + total_rsn,
                "used_aic": total_used_aic,
                "used_usd_0001": total_used_aic * 0.001,
                "used_usd_api_001": total_used_aic * 0.01,
                "cctx_selection_percent": cctx_select_pct,
                "cctx_warm_cache_hit_percent": cctx_warm_pct,
                "excluded_non_source_calls": non_source_cnt,
                "excluded_pre_edit_views": pre_edit_cnt,
                "direct_tokens_saved": direct_saved,
                "compounded_history_tokens_saved": compounded_saved,
                "effective_cache_reread_multiplier": (effective_reread_mult * 100.0).round() / 100.0,
                "turn_compression_calls": turn_comp_cnt,
                "sequential_slice_calls": seq_slice_cnt,
                "estimated_turn_tokens_saved": turn_saved,
                "bpe_formula_tokens_saved": bpe_saved_tokens,
                "bpe_formula_saved_aic": bpe_saved_aic,
                "bpe_formula_status": bpe_status,
                "estimated_total_tokens_saved": total_saved,
                "payload_saved_aic": payload_saved_aic_total,
                "avoided_turn_saved_aic": turn_saved_aic_total,
                "saved_aic": total_saved_aic,
                "cost_savings_percent": savings_pct,
                "saved_usd_0001": total_saved_aic * 0.001,
                "saved_usd_api_001": total_saved_aic * 0.01,
            },
            "model_usage": model_entries,
            "insights": insights.to_json(),
            "assumptions": {
                "mode": "trace_replayed_with_fallback",
                "compound_cache_multiplier": COMPOUND_CACHE_MULTIPLIER,
                "effective_cache_reread_multiplier": (effective_reread_mult * 100.0).round() / 100.0,
                "tokens_per_avoided_turn": TOKENS_PER_AVOIDED_TURN,
                "aic_per_saved_token": AIC_PER_SAVED_TOKEN,
                "aic_per_avoided_turn": AIC_PER_AVOIDED_TURN,
                "bpe_standing_tokens_saved_per_turn": bpe_turn_tok,
                "long_session_turns": usage_insights::LONG_SESSION_TURNS,
            },
        });
        return serde_json::to_string_pretty(&payload).map_err(Into::into);
    }

    let mut daemon_line = String::from("  • Shared Daemon   : offline");
    if let Some(ref d) = daemon_status {
        let state = d.get("index_state").and_then(Value::as_str).unwrap_or("online");
        let cache = d.get("cache");
        let daemon_adm = d.get("daemon_admission");
        let reg_adm = d.get("registry_admission");
        let stats = d.get("stats");
        let loaded = daemon_adm
            .and_then(|c| c.get("loaded_roots"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let registered = reg_adm
            .and_then(|c| c.get("registered_roots"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let res_bytes = daemon_adm
            .and_then(|c| c.get("estimated_bytes"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let root_limit = daemon_adm
            .and_then(|c| c.get("resident_root_limit"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let files = stats.and_then(|s| s.get("files")).and_then(Value::as_u64).unwrap_or(0);
        let source_bytes = stats
            .and_then(|s| s.get("source_bytes"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let hits = cache.and_then(|c| c.get("hits")).and_then(Value::as_u64).unwrap_or(0);
        let misses = cache.and_then(|c| c.get("misses")).and_then(Value::as_u64).unwrap_or(0);
        let d_hit_pct = (hits + misses > 0).then(|| 100.0 * hits as f64 / (hits + misses) as f64);
        daemon_line = format!(
            "  • Shared Daemon   : {state} ({files} files, {:.2} MiB) · {loaded}/{registered} roots ({:.1} MiB, limit {root_limit}) · Hit {} ({hits}/{})",
            source_bytes as f64 / (1024.0 * 1024.0),
            res_bytes as f64 / (1024.0 * 1024.0),
            fmt_pct(d_hit_pct),
            hits + misses
        );
    }

    let visual_width = |s: &str| -> usize {
        s.chars()
            .map(|c| {
                if matches!(c, '⚡' | '💰' | '💎' | '🔥' | '🎯' | '🚀') || (c as u32) >= 0x1F300 {
                    2
                } else {
                    1
                }
            })
            .sum()
    };
    let title_row = format!(
        "⚡ CODE CONTEXT (cctx) — MODEL, TOKEN & COST SAVINGS DASHBOARD (Window: {hours:.1}h)"
    );
    let hero_rows = [
        format!(
            "💰 COST SAVED (@$0.001/AIC) :  +${:<8.2} USD   (-{savings_pct:.1}% of baseline bill · ~{total_saved_aic:.1} AIC saved)",
            total_saved_aic * 0.001
        ),
        format!(
            "💎 API LIST VALUE (@$0.01)  :  +${:<8.2} USD   (Frontier API list price: $2/$0.10/$10 per 1M)",
            total_saved_aic * 0.01
        ),
        format!(
            "🔥 TOKENS SAVED             :  ~{:<9} tok   (Payload ~{} + {:.1}x Cache ~{} + ~{} Turns ~{} + BPE ~{})",
            fmt_tok(total_saved),
            fmt_tok(direct_saved),
            effective_reread_mult,
            fmt_tok(compounded_saved),
            turn_comp_cnt,
            fmt_tok(turn_saved),
            fmt_tok(bpe_saved_tokens)
        ),
        format!(
            "🧬 BPE FORMULA COMPRESSION  :  {}  ({}/{} surfaces: ext·mcp·subagent·global · ~{} tok/turn · +${:.2})",
            fmt_bar(bpe_surface_pct, 10),
            bpe_status.active_surfaces,
            bpe_status.total_surfaces,
            bpe_turn_tok,
            bpe_saved_aic * 0.001
        ),
        format!(
            "🎯 WARM CACHE HIT RATE      :  {}  ({} warm hits / {} CCTX lookups)",
            fmt_bar(cctx_warm_pct, 10),
            cctx_warm,
            cctx_calls.len()
        ),
        format!(
            "🚀 ELIGIBLE SOURCE ADOPTION :  {}  ({}/{} · excl. {} /tmp & {} pre-edit)",
            fmt_bar(cctx_select_pct, 10),
            cctx_calls.len(),
            lookup_calls.len(),
            non_source_cnt,
            pre_edit_cnt
        ),
    ];
    let meta_rows = [
        format!(
            "• Total LLM Spend : ${:.2} (@$0.001/AIC · {:.1} AIC)  |  ${:.2} (API List @$0.01/AIC)",
            total_used_aic * 0.001,
            total_used_aic,
            total_used_aic * 0.01
        ),
        format!(
            "• Total LLM Tokens: {} across {} sessions / {} turns (In {} · Cache {} · Out {} · Rsn {})",
            fmt_tok(total_in + total_out + total_rsn),
            active_sessions.len(),
            turns.len(),
            fmt_tok(total_in),
            fmt_pct(prompt_cache_pct),
            fmt_tok(total_out),
            fmt_tok(total_rsn)
        ),
        daemon_line.trim_start().to_string(),
    ];
    let box_width = std::iter::once(&title_row)
        .chain(hero_rows.iter())
        .chain(meta_rows.iter())
        .map(|s| visual_width(s))
        .max()
        .unwrap_or(92)
        .max(92);
    let frame_row = |content: &str| -> String {
        let pad = box_width.saturating_sub(visual_width(content));
        format!("║ {}{} ║", content, " ".repeat(pad))
    };
    let top_border = format!("╔{}╗", "═".repeat(box_width + 2));
    let mid_border = format!("╠{}╣", "═".repeat(box_width + 2));
    let bot_border = format!("╚{}╝", "═".repeat(box_width + 2));

    let mut lines = vec![top_border, frame_row(&title_row), mid_border.clone()];
    for row in &hero_rows {
        lines.push(frame_row(row));
    }
    lines.push(mid_border);
    for row in &meta_rows {
        lines.push(frame_row(row));
    }
    lines.push(bot_border);
    lines.push(String::new());

    let mut model_rows: Vec<Vec<String>> = model_entries
        .iter()
        .map(|m| {
            let u_aic = m["used_aic"].as_f64().unwrap_or(0.0);
            let s_aic = m["saved_aic"].as_f64().unwrap_or(0.0);
            vec![
                format!(
                    "{} ({})",
                    m["display_name"].as_str().unwrap_or("-"),
                    m["effort"].as_str().unwrap_or("-")
                ),
                format!("{}/{}", m["sessions"].as_u64().unwrap_or(0), m["turns"].as_u64().unwrap_or(0)),
                format!(
                    "{} ({})",
                    fmt_tok(m["input_tokens"].as_u64().unwrap_or(0)),
                    fmt_pct(m["prompt_cache_hit_percent"].as_f64())
                ),
                format!(
                    "{}+{}",
                    fmt_tok(m["output_tokens"].as_u64().unwrap_or(0)),
                    fmt_tok(m["reasoning_tokens"].as_u64().unwrap_or(0))
                ),
                format!(
                    "{} ({})",
                    m["cctx_calls"].as_u64().unwrap_or(0),
                    fmt_pct(m["cctx_cache_hit_percent"].as_f64())
                ),
                fmt_pct(m["cctx_adoption_percent"].as_f64()),
                format!("${:.2} ({u_aic:.1} AIC)", u_aic * 0.001),
                format!("+${:.2} / API +${:.2} (~{s_aic:.1} AIC)", s_aic * 0.001, s_aic * 0.01),
            ]
        })
        .collect();
    model_rows.push(vec![
        "TOTAL (All Models)".into(),
        format!("{}/{}", active_sessions.len(), turns.len()),
        format!("{} ({})", fmt_tok(total_in), fmt_pct(prompt_cache_pct)),
        format!("{}+{}", fmt_tok(total_out), fmt_tok(total_rsn)),
        format!("{} ({})", cctx_calls.len(), fmt_pct(cctx_warm_pct)),
        fmt_pct(cctx_select_pct),
        format!("${:.2} ({total_used_aic:.1} AIC)", total_used_aic * 0.001),
        format!("+${:.2} / API +${:.2} (~{total_saved_aic:.1} AIC)", total_saved_aic * 0.001, total_saved_aic * 0.01),
    ]);
    lines.push(render_box_table(
        "1. LLM MODEL USAGE, CACHE HIT & COST SAVINGS",
        &[
            "Model (Effort)",
            "Sess/Turns",
            "Input (Cache%)",
            "Out+Rsn Tok",
            "CCTX (Warm%)",
            "Src Adopt",
            "LLM Spend (@$0.001)",
            "COST SAVED (@$0.001 / API @$0.01)",
        ],
        &model_rows,
        &[false, true, true, true, true, true, true, true],
    ));
    lines.push(String::new());

    let mut surface_rows = Vec::new();
    for (key, label) in [
        ("native", "Native (#cctxNative*)"),
        ("mcp", "Shared MCP (code_*)"),
        ("builtin_source", "Built-in Source (rg/view/glob)"),
        ("builtin_pre_edit", "Built-in Pre-Edit (view->edit)"),
        ("host_only", "Host-Only (/tmp & .hidden)"),
        ("other", "Other Agent Tools"),
    ] {
        let g_calls: Vec<&CallRec> = valid_calls
            .iter()
            .filter(|c| match key {
                "native" => classify_tool_group(&c.tool) == "native",
                "mcp" => classify_tool_group(&c.tool) == "mcp",
                "builtin_source" => {
                    classify_tool_group(&c.tool) == "builtin_search_read"
                        && !c.non_source_host_only
                        && !c.pre_edit_view
                }
                "builtin_pre_edit" => c.pre_edit_view,
                "host_only" => c.non_source_host_only,
                _ => classify_tool_group(&c.tool) == "other",
            })
            .collect();
        if g_calls.is_empty() {
            continue;
        }
        let g_misses = g_calls.iter().filter(|c| c.cache_miss || c.cache_load_failed).count();
        let g_warm = g_calls.len().saturating_sub(g_misses);
        let warm_pct = matches!(key, "native" | "mcp").then(|| 100.0 * g_warm as f64 / g_calls.len() as f64);
        let mut durs: Vec<f64> = g_calls
            .iter()
            .filter_map(|c| match (c.started_at, c.completed_at) {
                (Some(s), Some(e)) if e >= s => Some((e - s) * 1000.0),
                _ => None,
            })
            .collect();
        durs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med_ms = (!durs.is_empty()).then(|| durs[durs.len() / 2]);
        let p95_ms = (!durs.is_empty()).then(|| {
            let idx = ((durs.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
            durs[idx.min(durs.len() - 1)]
        });
        let p_toks: Vec<u64> = g_calls.iter().map(|c| c.payload_tokens).filter(|t| *t > 0).collect();
        let avg_tok = if p_toks.is_empty() {
            0
        } else {
            p_toks.iter().sum::<u64>() / p_toks.len() as u64
        };
        let offloads = g_calls.iter().filter(|c| c.offloaded).count();
        let g_saved_aic: f64 = g_calls
            .iter()
            .map(|c| c.payload_saved_aic + c.turn_saved_aic)
            .sum();
        surface_rows.push(vec![
            label.to_string(),
            g_calls.len().to_string(),
            g_calls.iter().filter(|c| c.success == Some(true)).count().to_string(),
            fmt_pct(warm_pct),
            med_ms.map_or_else(|| "-".into(), |m| format!("{m:.1}ms")),
            p95_ms.map_or_else(|| "-".into(), |m| format!("{m:.1}ms")),
            format!("{avg_tok} tok"),
            offloads.to_string(),
            if matches!(key, "native" | "mcp") {
                format!("+${:.2} / API +${:.2} (~{g_saved_aic:.1} AIC)", g_saved_aic * 0.001, g_saved_aic * 0.01)
            } else {
                "-".into()
            },
        ]);
    }
    lines.push(render_box_table(
        "2. TOOL SURFACE PERFORMANCE, ELIGIBILITY & COST SAVINGS",
        &[
            "Surface",
            "Calls",
            "Success",
            "Warm Hit%",
            "Median",
            "P95",
            "Avg Payload",
            ">8KiB Offload",
            "COST SAVED (@$0.001 / API @$0.01)",
        ],
        &surface_rows,
        &[false, true, true, true, true, true, true, true, true],
    ));
    lines.push(String::new());

    let mut session_vec: Vec<(String, f64, usize, usize, Vec<String>)> = Vec::new();
    for s_id in &active_sessions {
        let s_turns: Vec<&TurnRec> = turns.iter().filter(|t| &t.session == s_id).collect();
        let s_calls: Vec<&CallRec> = valid_calls.iter().filter(|c| &c.session == s_id).collect();
        let s_lookup: Vec<&CallRec> = s_calls.iter().copied().filter(is_eligible_lookup).collect();
        let s_cctx: Vec<&CallRec> = s_lookup
            .iter()
            .copied()
            .filter(|c| matches!(classify_tool_group(&c.tool), "native" | "mcp"))
            .collect();
        let s_misses = s_cctx.iter().filter(|c| c.cache_miss || c.cache_load_failed).count();
        let s_warm = s_cctx.len().saturating_sub(s_misses);
        let s_warm_pct = (!s_cctx.is_empty()).then(|| 100.0 * s_warm as f64 / s_cctx.len() as f64);
        let s_adopt_pct = (!s_lookup.is_empty()).then(|| 100.0 * s_cctx.len() as f64 / s_lookup.len() as f64);
        let ws = session_cwds
            .get(s_id)
            .and_then(|c| Path::new(c).file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "-".into());
        let primary_model = s_turns
            .first()
            .map(|t| t.display_name.clone())
            .or_else(|| s_calls.iter().find_map(|c| c.model.clone()))
            .unwrap_or_else(|| "-".into());
        let s_nano: u64 = s_turns.iter().map(|t| t.nano_aiu).sum();
        let s_used_aic = s_nano as f64 / 1_000_000_000.0;
        let s_saved_aic: f64 = s_cctx
            .iter()
            .map(|c| c.payload_saved_aic + c.turn_saved_aic)
            .sum::<f64>()
            + s_turns.iter().map(|t| t.bpe_saved_aic).sum::<f64>();
        let short_id: String = s_id.chars().take(12).collect();
        let short_ws: String = ws.chars().take(20).collect();
        let short_mod: String = primary_model.chars().take(18).collect();
        session_vec.push((
            s_id.clone(),
            s_used_aic,
            s_turns.len(),
            s_lookup.len(),
            vec![
                short_id,
                short_ws,
                short_mod,
                s_turns.len().to_string(),
                format!("${:.2} ({s_used_aic:.1} AIC)", s_used_aic * 0.001),
                fmt_bar(s_warm_pct, 8),
                format!("{} ({}/{})", fmt_pct(s_adopt_pct), s_cctx.len(), s_lookup.len()),
                format!("+${:.2} / API +${:.2} (~{s_saved_aic:.1} AIC)", s_saved_aic * 0.001, s_saved_aic * 0.01),
            ],
        ));
    }
    session_vec.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| b.3.cmp(&a.3))
    });
    let non_trivial_cnt = session_vec
        .iter()
        .filter(|s| !(s.2 <= 1 && s.3 == 0 && s.1 < 10.0))
        .count();
    let session_rows: Vec<Vec<String>> = session_vec
        .into_iter()
        .filter(|s| non_trivial_cnt < 6 || !(s.2 <= 1 && s.3 == 0 && s.1 < 10.0))
        .take(12)
        .map(|item| item.4)
        .collect();
    lines.push(render_box_table(
        "3. TOP SESSIONS BY LLM SPEND, CACHE HIT & COST SAVINGS (Top 12)",
        &[
            "Session",
            "Workspace",
            "Primary Model",
            "Turns",
            "LLM Spend (@$0.001)",
            "Warm Hit Rate",
            "Src Adopt (CCTX/Elig)",
            "COST SAVED (@$0.001 / API @$0.01)",
        ],
        &session_rows,
        &[false, false, false, true, true, false, true, true],
    ));
    lines.extend(insights.render());
    let eff_turn_tok = if avoided_turns_total > 0.0 {
        turn_saved as f64 / avoided_turns_total
    } else {
        TOKENS_PER_AVOIDED_TURN
    };
    let eff_turn_aic = if avoided_turns_total > 0.0 {
        turn_saved_aic_total / avoided_turns_total
    } else {
        AIC_PER_AVOIDED_TURN
    };
    lines.push(format!(
        "\n  Savings model: trace-replayed ({effective_reread_mult:.1}x avg pre-compaction re-read · ~{} tok / ~{eff_turn_aic:.1} AIC per avoided warm turn; see `cctx usage --json`).",
        fmt_tok(eff_turn_tok.round() as u64)
    ));
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod trace_timing_tests {
    use super::extract_trace_total_ms;
    use serde_json::json;

    #[test]
    fn reads_total_ms_from_trace_footer_in_any_result_shape() {
        let footer = "CCTX_TRACE {\"trace_id\":\"t\",\"total_ms\":1234,\"started_unix_ms\":1}";
        let text = format!("matches=1\n{footer}");
        assert_eq!(extract_trace_total_ms(&json!({"content": text}), 0), Some(1234.0));
        assert_eq!(extract_trace_total_ms(&json!([footer]), 0), Some(1234.0));
        assert_eq!(extract_trace_total_ms(&json!(footer), 0), Some(1234.0));
    }

    #[test]
    fn missing_or_untimed_footers_yield_none() {
        assert_eq!(extract_trace_total_ms(&json!("plain output"), 0), None);
        assert_eq!(extract_trace_total_ms(&json!("x\nCCTX_TRACE {\"trace_id\":\"t\"}"), 0), None);
        assert_eq!(extract_trace_total_ms(&json!("x\nCCTX_TRACE {broken"), 0), None);
    }

    #[test]
    fn trace_replayed_savings_use_model_rates_compaction_and_subagent_isolation() {
        let home = tempfile::tempdir().unwrap();
        let sess_dir = home
            .path()
            .join(".copilot")
            .join("session-state")
            .join("sess-1");
        std::fs::create_dir_all(&sess_dir).unwrap();
        let details_sol = json!([
            {"batchSize": 1_000_000, "costPerBatch": 800_000_000_000_u64, "tokenType": "input"},
            {"batchSize": 1_000_000, "costPerBatch": 80_000_000_000_u64, "tokenType": "cache_read"},
            {"batchSize": 1_000_000, "costPerBatch": 1_000_000_000_000_u64, "tokenType": "cache_write"},
            {"batchSize": 1_000_000, "costPerBatch": 3_000_000_000_000_u64, "tokenType": "output"}
        ]);
        let events = vec![
            json!({
                "type": "session.start",
                "timestamp": "2026-11-12T20:00:00Z",
                "data": {"context": {"cwd": "/path/to/project"}}
            }),
            json!({
                "type": "tool.execution_start",
                "timestamp": "2026-11-12T20:00:01Z",
                "data": {"toolName": "cctxNativeContext", "toolCallId": "c1", "model": "gpt-5.6-sol", "arguments": {"pattern": "Planner"}}
            }),
            json!({
                "type": "tool.execution_complete",
                "timestamp": "2026-11-12T20:00:02Z",
                "data": {"toolCallId": "c1", "model": "gpt-5.6-sol", "success": true, "result": {"content": "matches=1\n1: struct Planner;"}}
            }),
            json!({
                "type": "session.usage_record",
                "timestamp": "2026-11-12T20:00:03Z",
                "data": {"usage": {
                    "model": "gpt-5.6-sol",
                    "modelDisplayName": "GPT-5.6 Sol",
                    "reasoningEffort": "high",
                    "inputTokens": 300_000,
                    "cacheReadTokens": 280_000,
                    "cacheWriteTokens": 20_000,
                    "outputTokens": 1_000,
                    "copilotUsage": {"totalNanoAiu": 45_400_000_000_u64, "tokenDetails": details_sol}
                }}
            }),
            // Interleaved small subagent turn must NOT look like a compaction reset on main.
            json!({
                "type": "session.usage_record",
                "agentId": "sub-explore",
                "timestamp": "2026-11-12T20:00:04Z",
                "data": {"usage": {
                    "model": "gpt-6.1-sol",
                    "modelDisplayName": "GPT-6.1 Sol",
                    "reasoningEffort": "medium",
                    "inputTokens": 10_000,
                    "cacheReadTokens": 9_000,
                    "cacheWriteTokens": 1_000,
                    "outputTokens": 200,
                    "copilotUsage": {"totalNanoAiu": 540_000_000_u64}
                }}
            }),
            // Warm turn 2 on main: re-reads c1 before compaction.
            json!({
                "type": "session.usage_record",
                "timestamp": "2026-11-12T20:00:05Z",
                "data": {"usage": {
                    "model": "gpt-5.6-sol",
                    "modelDisplayName": "GPT-5.6 Sol",
                    "reasoningEffort": "high",
                    "inputTokens": 310_000,
                    "cacheReadTokens": 300_000,
                    "cacheWriteTokens": 10_000,
                    "outputTokens": 1_000,
                    "copilotUsage": {"totalNanoAiu": 37_000_000_000_u64, "tokenDetails": details_sol}
                }}
            }),
            // Compaction reset on main (310k -> 80k): stops further compounding of c1!
            json!({
                "type": "session.usage_record",
                "timestamp": "2026-11-12T20:00:06Z",
                "data": {"usage": {
                    "model": "gpt-5.6-sol",
                    "modelDisplayName": "GPT-5.6 Sol",
                    "reasoningEffort": "high",
                    "inputTokens": 80_000,
                    "cacheReadTokens": 75_000,
                    "cacheWriteTokens": 5_000,
                    "outputTokens": 500,
                    "copilotUsage": {"totalNanoAiu": 12_500_000_000_u64, "tokenDetails": details_sol}
                }}
            }),
            // Sequential slice read must receive 0 avoided-turn credit.
            json!({
                "type": "tool.execution_start",
                "timestamp": "2026-11-12T20:00:07Z",
                "data": {"toolName": "cctxNativeRead", "toolCallId": "c2", "model": "gpt-5.6-sol", "arguments": {"ranges": [{"path": "a.rs", "start": 151, "end": 300}]}}
            }),
            json!({
                "type": "tool.execution_complete",
                "timestamp": "2026-11-12T20:00:08Z",
                "data": {"toolCallId": "c2", "model": "gpt-5.6-sol", "success": true, "result": {"content": "NOTICE SEQUENTIAL_SLICE #2 (151..300 of 500)\n151: fn x() {}"}}
            }),
        ];
        let body = events
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(sess_dir.join("events.jsonl"), body).unwrap();

        let now = super::event_unix_ts(&json!({"timestamp": "2026-11-12T20:05:00Z"})).unwrap();
        let raw = super::usage_from_home(home.path(), 24.0, now, None, &[], true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let ts = &parsed["token_summary"];
        // c1 (1600 direct) was re-read on turn 2 (despite interleaved subagent!) and stopped at turn 3 compaction => 1600 compounded!
        assert_eq!(ts["compounded_history_tokens_saved"].as_u64(), Some(1600));
        assert_eq!(ts["sequential_slice_calls"].as_u64(), Some(1));
        // Only c1 gets 1.0 avoided turn; c2 (SEQUENTIAL_SLICE) gets 0.0 => rounded turn_compression_calls == 1.
        assert_eq!(ts["turn_compression_calls"].as_u64(), Some(1));
        // Avoided turn AIC uses GPT-5.6 Sol's warm turn mean ((45.4 + 37.0 + 12.5)/3 = 31.63 AIC), far above legacy hardcoded 4.20 AIC.
        assert!(ts["avoided_turn_saved_aic"].as_f64().unwrap() > 30.0);
        // GPT-6.1 Sol subagent had 0 CCTX calls: its saved_aic is strictly single-turn BPE prefix (~0.02 AIC), not 35x inflated.
        let sub_entry = parsed["model_usage"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["model"] == "gpt-6.1-sol")
            .unwrap();
        assert!(sub_entry["saved_aic"].as_f64().unwrap() < 1.0);
    }

    #[test]
    fn cross_model_classifiers_exclude_tool_search_batch_pre_edit_and_track_claude_opus() {
        assert_eq!(super::classify_tool_group("tool_search_tool"), "other");
        assert_eq!(super::classify_tool_group("web_search"), "other");
        assert_eq!(super::classify_tool_group("github-mcp-server-search_code"), "other");
        assert_eq!(super::classify_tool_group("mcp__code-context__code_context"), "mcp");
        assert_eq!(super::classify_tool_group("mcp__code-context__code_grep"), "mcp");
        assert_eq!(super::classify_tool_group("Read"), "builtin_search_read");
        assert_eq!(super::classify_tool_group("Glob"), "builtin_search_read");
        assert!(super::is_lookup_tool_name("mcp__code-context__code_context"));
        assert!(!super::is_lookup_tool_name("tool_search_tool"));

        let home = tempfile::tempdir().unwrap();
        let sess_dir = home.path().join(".copilot/session-state/s-carbon");
        std::fs::create_dir_all(&sess_dir).unwrap();
        let rates = json!([
            {"tokenType": "input", "tokenCount": 400_000, "batchSize": 1_000_000, "costPerBatch": 200_000_000_000_u64},
            {"tokenType": "cache_read", "tokenCount": 0, "batchSize": 1_000_000, "costPerBatch": 10_000_000_000_u64},
            {"tokenType": "output", "tokenCount": 1_000, "batchSize": 1_000_000, "costPerBatch": 1_000_000_000_000_u64}
        ]);
        let mut events = vec![
            json!({"type": "session.start", "timestamp": "2026-11-13T05:00:00Z", "data": {"context": {"cwd": "/repo"}}}),
            // tool_search_tool + /dev/shm view + batched view (followed 6 calls later by same-file edit) must NOT penalize adoption.
            json!({"type": "tool.execution_start", "timestamp": "2026-11-13T05:00:01Z", "data": {"toolName": "tool_search_tool", "toolCallId": "ts1", "model": "gemini-4-carbon", "arguments": {"query": "cctxNativeContext"}}}),
            json!({"type": "tool.execution_complete", "timestamp": "2026-11-13T05:00:01Z", "data": {"toolCallId": "ts1", "model": "gemini-4-carbon", "success": true, "result": {"content": "[\"cctxNativeContext\"]"}}}),
            json!({"type": "tool.execution_start", "timestamp": "2026-11-13T05:00:02Z", "data": {"toolName": "view", "toolCallId": "v_shm", "model": "gemini-4-carbon", "arguments": {"path": "/dev/shm/probe.txt"}}}),
            json!({"type": "tool.execution_complete", "timestamp": "2026-11-13T05:00:02Z", "data": {"toolCallId": "v_shm", "model": "gemini-4-carbon", "success": true, "result": {"content": "scratch"}}}),
            json!({"type": "tool.execution_start", "timestamp": "2026-11-13T05:00:03Z", "data": {"toolName": "view", "toolCallId": "v_pre", "model": "gemini-4-carbon", "arguments": {"path": "/repo/src/gate_kernels.rs", "view_range": [10, 40]}}}),
            json!({"type": "tool.execution_complete", "timestamp": "2026-11-13T05:00:03Z", "data": {"toolCallId": "v_pre", "model": "gemini-4-carbon", "success": true, "result": {"content": "fn a() {}"}}}),
        ];
        for k in 0..6 {
            let cid = format!("cctx_{k}");
            events.push(json!({"type": "tool.execution_start", "timestamp": "2026-11-13T05:00:04Z", "data": {"toolName": "cctxNativeContext", "toolCallId": cid, "model": "gemini-4-carbon", "arguments": {"pattern": "gate"}}}));
            events.push(json!({"type": "tool.execution_complete", "timestamp": "2026-11-13T05:00:04Z", "data": {"toolCallId": cid, "model": "gemini-4-carbon", "success": true, "result": {"content": "matches=1"}}}));
        }
        events.push(json!({"type": "tool.execution_start", "timestamp": "2026-11-13T05:00:05Z", "data": {"toolName": "edit", "toolCallId": "ed1", "model": "gemini-4-carbon", "arguments": {"path": "/repo/src/gate_kernels.rs"}}}));
        events.push(json!({"type": "tool.execution_complete", "timestamp": "2026-11-13T05:00:05Z", "data": {"toolCallId": "ed1", "model": "gemini-4-carbon", "success": true, "result": {"content": "updated"}}}));
        for k in 0..3 {
            events.push(json!({
                "type": "session.usage_record",
                "timestamp": format!("2026-11-13T05:01:0{k}Z"),
                "data": {"usage": {
                    "model": "gemini-4-carbon",
                    "modelDisplayName": "Gemini 4 Carbon",
                    "reasoningEffort": "high",
                    "inputTokens": 400_000,
                    "cacheReadTokens": 0,
                    "cacheWriteTokens": 0,
                    "outputTokens": 1_000,
                    "copilotUsage": {"totalNanoAiu": 81_000_000_000_u64, "tokenDetails": rates}
                }}
            }));
        }
        std::fs::write(
            sess_dir.join("events.jsonl"),
            events
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        // Claude Agent Host Opus JSONL session
        let claude_dir = home.path().join(".claude/projects/repo");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let claude_events = [
            json!({
                "type": "assistant",
                "sessionId": "opus-sess-1",
                "cwd": "/repo",
                "timestamp": "2026-11-13T05:02:00Z",
                "message": {
                    "model": "claude-opus-4-7",
                    "usage": {"input_tokens": 1_000, "cache_creation_input_tokens": 2_000, "cache_read_input_tokens": 97_000, "output_tokens": 500},
                    "content": [{"type": "tool_use", "id": "tu_1", "name": "mcp__code-context__code_context", "input": {"root": "/repo", "pattern": "Planner"}}]
                }
            }),
            json!({
                "type": "user",
                "sessionId": "opus-sess-1",
                "cwd": "/repo",
                "timestamp": "2026-11-13T05:02:01Z",
                "message": {
                    "content": [{"type": "tool_result", "tool_use_id": "tu_1", "is_error": false, "content": "matches=1\n1: struct Planner;"}]
                }
            }),
        ];
        std::fs::write(
            claude_dir.join("opus-sess-1.jsonl"),
            claude_events
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let now = super::event_unix_ts(&json!({"timestamp": "2026-11-13T05:10:00Z"})).unwrap();
        let raw = super::usage_from_home(home.path(), 1.0, now, None, &[], true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let models = parsed["model_usage"].as_array().unwrap();
        let carbon = models.iter().find(|m| m["model"] == "gemini-4-carbon").unwrap();
        assert_eq!(carbon["cctx_adoption_percent"].as_f64(), Some(100.0));
        assert_eq!(carbon["cold_cache_turns"].as_u64(), Some(3));
        assert!(carbon["cold_cache_excess_aic"].as_f64().unwrap() > 150.0);
        let opus = models.iter().find(|m| m["model"] == "claude-opus-4-7").unwrap();
        assert_eq!(opus["cctx_adoption_percent"].as_f64(), Some(100.0));
        let adv_codes: Vec<&str> = parsed["insights"]["advisories"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a["code"].as_str())
            .collect();
        assert!(adv_codes.contains(&"cold_cache_spike"), "{adv_codes:?}");
    }
}
