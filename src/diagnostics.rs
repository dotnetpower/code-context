use crate::usage_insights::{self, CallFacts, ModelFacts, SessionFacts};
use crate::{Result, catalog, daemon, invalid, outcome};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, Read};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const CAPABILITY_SCHEMA: u32 = 3;

// Savings model assumptions; surfaced in `cctx usage --json` so they can be audited.
const COMPOUND_CACHE_MULTIPLIER: u64 = 35;
const TOKENS_PER_AVOIDED_TURN: f64 = 220_000.0;
const AIC_PER_SAVED_TOKEN: f64 = 0.00055;
const AIC_PER_AVOIDED_TURN: f64 = 4.20;

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
            .and_then(|workspace| workspace.files_limited(&spec.policy))
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
    if name.starts_with("cctxNative") || name.starts_with("code-context-native") {
        return "native";
    }
    if name.starts_with("code-context-")
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

fn is_turn_compressed(name: &str, args: &Value) -> bool {
    if name.ends_with("code_context") || name.ends_with("codeContext") || name.ends_with("NativeContext") {
        return true;
    }
    if name.ends_with("code_read") || name.ends_with("codeRead") || name.ends_with("NativeRead") {
        if let Some(ranges) = args.get("ranges").and_then(Value::as_array) {
            if ranges.len() > 1 {
                return true;
            }
            if ranges.iter().any(|item| {
                item.get("path")
                    .or_else(|| item.get("filePath"))
                    .and_then(Value::as_str)
                    .is_some_and(|p| p.contains('#'))
            }) {
                return true;
            }
        }
        if args
            .get("path")
            .or_else(|| args.get("filePath"))
            .and_then(Value::as_str)
            .is_some_and(|p| p.contains('#'))
        {
            return true;
        }
    }
    false
}

fn estimate_tokens_saved(name: &str, args: &Value, payload_tokens: u64, success: Option<bool>) -> u64 {
    if success != Some(true) || !matches!(classify_tool_group(name), "native" | "mcp") || !is_lookup_tool_name(name) {
        return 0;
    }
    if name.ends_with("code_context") || name.ends_with("codeContext") || name.ends_with("NativeContext") {
        return 1600_u64.max((payload_tokens as f64 * 1.85).round() as u64);
    }
    if name.ends_with("code_read") || name.ends_with("codeRead") || name.ends_with("NativeRead") {
        let compressed = is_turn_compressed(name, args);
        let floor = if compressed { 1400 } else { 900 };
        let mult = if compressed { 1.75 } else { 1.10 };
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

fn avoided_turns_for_call(name: &str, turn_compressed: bool, success: Option<bool>) -> f64 {
    if success != Some(true) || !matches!(classify_tool_group(name), "native" | "mcp") || !is_lookup_tool_name(name) {
        return 0.0;
    }
    if turn_compressed {
        1.0
    } else {
        0.25
    }
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

fn extract_result_chars(val: &Value, depth: usize) -> ( usize, bool ) {
    if depth > 6 {
        return (0, false);
    }
    if let Some(s) = val.as_str() {
        let trimmed = match s.rfind("\nCCTX_TRACE {") {
            Some(idx) => &s[..idx],
            None => s,
        };
        return (trimmed.len(), trimmed.starts_with("Output too large to read at once"));
    }
    if let Some(obj) = val.as_object() {
        for key in ["content", "text", "value", "output"] {
            if let Some(child) = obj.get(key) {
                let (chars, offloaded) = extract_result_chars(child, depth + 1);
                if chars > 0 {
                    return (chars, offloaded);
                }
            }
        }
    }
    if let Some(arr) = val.as_array() {
        let mut total = 0;
        let mut any_offload = false;
        for item in arr {
            let (c, off) = extract_result_chars(item, depth + 1);
            total += c;
            any_offload |= off;
        }
        return (total, any_offload);
    }
    (0, false)
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
    let cutoff = now - hours * 3600.0;

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
        session: String,
        model: String,
        display_name: String,
        effort: String,
        input_tokens: u64,
        cache_read_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: u64,
        duration_ms: f64,
        nano_aiu: u64,
    }

    #[derive(Default, Clone)]
    struct CallRec {
        session: String,
        seq: usize,
        cwd: Option<String>,
        tool: String,
        target: String,
        model: Option<String>,
        started_at: Option<f64>,
        completed_at: Option<f64>,
        success: Option<bool>,
        payload_tokens: u64,
        offloaded: bool,
        cache_miss: bool,
        cache_load_failed: bool,
        error_detail: &'static str,
        trace_total_ms: Option<f64>,
        turn_compressed: bool,
        non_source_host_only: bool,
        pre_edit_view: bool,
        saved_tokens: u64,
    }

    let mut turns: Vec<TurnRec> = Vec::new();
    let mut calls: std::collections::BTreeMap<(String, String), CallRec> = std::collections::BTreeMap::new();
    let mut session_cwds: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut active_sessions: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
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
        let mut last_model: Option<String> = None;
        let mut line = String::with_capacity(4096);
        while {
            line.clear();
            reader.read_line(&mut line).unwrap_or(0) > 0
        } {
            if !line.contains("\"session.usage_record\"")
                && !line.contains("\"tool.execution_")
                && !line.contains("\"session.start\"")
            {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(event_type) = event.get("type").and_then(Value::as_str) else {
                continue;
            };
            if event_type == "session.start"
                && let Some(c) = event
                    .get("data")
                    .and_then(|d| d.get("context"))
                    .and_then(|ctx| ctx.get("cwd"))
                    .and_then(Value::as_str)
            {
                cwd = Some(c.to_owned());
                session_cwds.insert(session_id.clone(), c.to_owned());
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
                    last_model = Some(model.clone());
                    active_sessions.insert(session_id.clone());
                    turns.push(TurnRec {
                        session: session_id.clone(),
                        model,
                        display_name,
                        effort,
                        input_tokens: u.get("inputTokens").and_then(Value::as_u64).unwrap_or(0),
                        cache_read_tokens: u.get("cacheReadTokens").and_then(Value::as_u64).unwrap_or(0),
                        output_tokens: u.get("outputTokens").and_then(Value::as_u64).unwrap_or(0),
                        reasoning_tokens: u.get("reasoningTokens").and_then(Value::as_u64).unwrap_or(0),
                        duration_ms: u.get("duration").and_then(Value::as_f64).unwrap_or(0.0),
                        nano_aiu,
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
                    .map(Value::to_string)
                    .unwrap_or_default();
                let non_source_host_only = classify_tool_group(&tool) == "builtin_search_read"
                    && [
                        "/tmp/",
                        "copilot-tool-output",
                        "/.copilot",
                        "/.github",
                        "/.vscode",
                        ".png",
                        ".jpg",
                        ".pdf",
                        ".vsix",
                    ]
                    .iter()
                    .any(|p| target.contains(p));
                let call_model = data
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| last_model.clone());
                seq_counter += 1;
                let entry = calls.entry((session_id.clone(), call_id.to_owned())).or_default();
                entry.session = session_id.clone();
                entry.seq = seq_counter;
                entry.cwd = cwd.clone();
                entry.tool = tool.clone();
                entry.target = target;
                entry.model = call_model;
                entry.started_at = Some(ts);
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
                if entry.model.is_none() {
                    entry.model = data
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| last_model.clone());
                }
                entry.completed_at = Some(ts);
                entry.success = data.get("success").and_then(Value::as_bool);
                if let Some(res) = data.get("result") {
                    let (chars, offloaded) = extract_result_chars(res, 0);
                    entry.trace_total_ms = extract_trace_total_ms(res, 0);
                    entry.payload_tokens = chars.div_ceil(4) as u64;
                    entry.offloaded = offloaded;
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

    let mut valid_calls: Vec<CallRec> = calls
        .into_values()
        .filter(|c| c.started_at.is_some_and(|s| s >= cutoff))
        .map(|mut c| {
            let synthetic_args = if c.turn_compressed {
                json!({"ranges": [{"path": "a#toc"}, {"path": "b"}]})
            } else {
                json!({})
            };
            c.saved_tokens = estimate_tokens_saved(&c.tool, &synthetic_args, c.payload_tokens, c.success);
            c
        })
        .collect();
    valid_calls.sort_by(|a, b| a.session.cmp(&b.session).then_with(|| a.seq.cmp(&b.seq)));
    for i in 0..valid_calls.len() {
        if valid_calls[i].tool == "view" && !valid_calls[i].non_source_host_only {
            let sess = &valid_calls[i].session;
            let followed_by_edit = valid_calls
                .iter()
                .skip(i + 1)
                .take(4)
                .any(|next| &next.session == sess && next.tool == "edit");
            valid_calls[i].pre_edit_view = followed_by_edit;
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
    let cctx_misses = cctx_calls.iter().filter(|c| c.cache_miss || c.cache_load_failed).count();
    let cctx_warm = cctx_calls.len().saturating_sub(cctx_misses);
    let cctx_warm_pct = (!cctx_calls.is_empty()).then(|| 100.0 * cctx_warm as f64 / cctx_calls.len() as f64);
    let cctx_select_pct = (!lookup_calls.is_empty()).then(|| 100.0 * cctx_calls.len() as f64 / lookup_calls.len() as f64);

    let total_in: u64 = turns.iter().map(|t| t.input_tokens).sum();
    let total_cache: u64 = turns.iter().map(|t| t.cache_read_tokens).sum();
    let total_out: u64 = turns.iter().map(|t| t.output_tokens).sum();
    let total_rsn: u64 = turns.iter().map(|t| t.reasoning_tokens).sum();
    let total_nano_aiu: u64 = turns.iter().map(|t| t.nano_aiu).sum();
    let total_used_aic = total_nano_aiu as f64 / 1_000_000_000.0;
    let prompt_cache_pct = (total_in > 0).then(|| 100.0 * total_cache as f64 / total_in as f64);
    let direct_saved: u64 = cctx_calls.iter().map(|c| c.saved_tokens).sum();
    let compounded_saved: u64 = direct_saved * COMPOUND_CACHE_MULTIPLIER;
    let avoided_turns_total: f64 = cctx_calls
        .iter()
        .map(|c| avoided_turns_for_call(&c.tool, c.turn_compressed, c.success))
        .sum();
    let turn_comp_cnt = avoided_turns_total.round() as u64;
    let turn_saved = (avoided_turns_total * TOKENS_PER_AVOIDED_TURN).round() as u64;
    let total_saved = direct_saved + compounded_saved + turn_saved;
    let total_saved_aic = (direct_saved as f64 * AIC_PER_SAVED_TOKEN) + (avoided_turns_total * AIC_PER_AVOIDED_TURN);
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
        let m_avoided_turns: f64 = m_cctx
            .iter()
            .map(|c| avoided_turns_for_call(&c.tool, c.turn_compressed, c.success))
            .sum();
        let m_total_saved = m_direct_saved * (1 + COMPOUND_CACHE_MULTIPLIER) + (m_avoided_turns * TOKENS_PER_AVOIDED_TURN).round() as u64;
        let m_saved_aic = (m_direct_saved as f64 * AIC_PER_SAVED_TOKEN) + (m_avoided_turns * AIC_PER_AVOIDED_TURN);
        let mut durs: Vec<f64> = m_turns.iter().map(|t| t.duration_ms).filter(|d| *d > 0.0).collect();
        durs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med_turn = (!durs.is_empty()).then(|| durs[durs.len() / 2]);
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
                "turn_compression_calls": turn_comp_cnt,
                "estimated_turn_tokens_saved": turn_saved,
                "estimated_total_tokens_saved": total_saved,
                "saved_aic": total_saved_aic,
                "saved_usd_0001": total_saved_aic * 0.001,
                "saved_usd_api_001": total_saved_aic * 0.01,
            },
            "model_usage": model_entries,
            "insights": insights.to_json(),
            "assumptions": {
                "compound_cache_multiplier": COMPOUND_CACHE_MULTIPLIER,
                "tokens_per_avoided_turn": TOKENS_PER_AVOIDED_TURN,
                "aic_per_saved_token": AIC_PER_SAVED_TOKEN,
                "aic_per_avoided_turn": AIC_PER_AVOIDED_TURN,
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
            "🔥 TOKENS SAVED             :  ~{:<9} tok   (Payload ~{} + 35x Cache Re-read ~{} + ~{} Turns ~{})",
            fmt_tok(total_saved),
            fmt_tok(direct_saved),
            fmt_tok(compounded_saved),
            turn_comp_cnt,
            fmt_tok(turn_saved)
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
        let saved: u64 = g_calls.iter().map(|c| c.saved_tokens).sum();
        let g_avoided: f64 = g_calls
            .iter()
            .map(|c| avoided_turns_for_call(&c.tool, c.turn_compressed, c.success))
            .sum();
        let g_saved_aic = (saved as f64 * AIC_PER_SAVED_TOKEN) + (g_avoided * AIC_PER_AVOIDED_TURN);
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

    let mut session_vec: Vec<(String, usize, usize, u64, Vec<String>)> = Vec::new();
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
        let s_direct: u64 = s_cctx.iter().map(|c| c.saved_tokens).sum();
        let s_avoided: f64 = s_cctx
            .iter()
            .map(|c| avoided_turns_for_call(&c.tool, c.turn_compressed, c.success))
            .sum();
        let s_saved = s_direct * (1 + COMPOUND_CACHE_MULTIPLIER) + (s_avoided * TOKENS_PER_AVOIDED_TURN).round() as u64;
        let s_saved_aic = (s_direct as f64 * AIC_PER_SAVED_TOKEN) + (s_avoided * AIC_PER_AVOIDED_TURN);
        let short_id: String = s_id.chars().take(12).collect();
        let short_ws: String = ws.chars().take(20).collect();
        let short_mod: String = primary_model.chars().take(18).collect();
        session_vec.push((
            s_id.clone(),
            s_turns.len(),
            s_lookup.len(),
            s_saved,
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
    session_vec.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.2.cmp(&a.2)));
    let session_rows: Vec<Vec<String>> = session_vec.into_iter().take(12).map(|item| item.4).collect();
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
    lines.push(format!(
        "\n  Savings model: {COMPOUND_CACHE_MULTIPLIER}x cache re-read, {TOKENS_PER_AVOIDED_TURN:.0} tok/avoided turn (see `cctx usage --json` assumptions)."
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
}
