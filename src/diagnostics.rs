use crate::usage_insights::{self, CacheLifecycleFacts, CallFacts, ModelFacts, SessionFacts};
use crate::{Result, catalog, daemon, invalid, outcome};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub(crate) mod codex;
pub(crate) mod rates;
pub(crate) mod shell_lookup;
pub(crate) mod usage_display;
use rates::{
    TokenRates, cold_prefix_rate, extract_token_rates, premium_request_multiplier,
    rates_for_claude_turn,
};
use usage_display::{Style, UsageDisplay, UsageSummary};

pub const CAPABILITY_SCHEMA: u32 = 3;

// Fallback savings assumptions when a session has no `session.usage_record` events;
// when usage records exist, `cctx usage` replays actual per-turn `copilotUsage.tokenDetails`
// rates, pre-compaction re-read horizons, and warm-turn context sizes.
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
                "cold_direct_read":true,"session_warm_hint":true,"caller_session_token":true,
                "code_usage":true,
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
    let stale_managed_worktrees: Vec<PathBuf> = specs
        .iter()
        .filter(|spec| {
            spec.managed_parent.is_some()
                && !missing_managed_worktrees.contains(&spec.root)
                && crate::registration::registered_policy(spec, &specs)
                    .is_ok_and(|fresh| &fresh != *spec)
        })
        .map(|spec| spec.root.clone())
        .collect();
    let windows_companion = inspect_windows_companion_binary();
    let subagent_definitions = inspect_subagent_definitions();
    let bpe_audit = crate::bpe_builder::audit_bpe_profile(&crate::bpe_builder::BpeBuildConfig::default())
        .ok()
        .and_then(|rep| serde_json::to_value(rep).ok())
        .unwrap_or(Value::Null);
    Ok(
        json!({"client":local,"daemon":remote,"daemon_error":daemon_error,
        "root_resolution":root_resolution,"registrations":specs,"processes":processes,
        "stale_mcp_processes":stale,"daemon_matches_client":daemon_matches_client,
        "legacy_glob_roots":legacy_glob_roots,
        "missing_managed_worktrees":missing_managed_worktrees,
        "stale_managed_worktrees":stale_managed_worktrees,
        "windows_companion":windows_companion,
        "subagent_definitions":subagent_definitions,
        "bpe_audit":bpe_audit,
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

fn inspect_windows_companion_binary() -> Value {
    if std::env::var_os("WSL_DISTRO_NAME").is_none() {
        return Value::Null;
    }
    let linux_mtime = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok());
    let users_dir = Path::new("/mnt/c/Users");
    let Ok(entries) = std::fs::read_dir(users_dir) else {
        return Value::Null;
    };
    for entry in entries.flatten() {
        let candidate = entry.path().join(".local").join("bin").join("cctx.exe");
        if let Ok(meta) = std::fs::metadata(&candidate)
            && meta.is_file()
        {
            let win_mtime = meta.modified().ok();
            let lag_seconds = match (linux_mtime, win_mtime) {
                (Some(lm), Some(wm)) => lm.duration_since(wm).ok().map(|d| d.as_secs()),
                _ => None,
            };
            return json!({
                "path": candidate,
                "size_bytes": meta.len(),
                "lag_behind_wsl_seconds": lag_seconds.unwrap_or(0),
                "stale": lag_seconds.is_some_and(|s| s > 3600),
            });
        }
    }
    Value::Null
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
        return json!({"checked": 0, "configured": 0, "auto_patched": 0, "unconfigured": [], "invalid_yaml": []});
    };
    let mut checked = 0usize;
    let mut configured = 0usize;
    let mut unconfigured: Vec<String> = Vec::new();
    let mut invalid_yaml: Vec<String> = Vec::new();
    for def_dir in crate::prompt_formula::discover_subagent_definition_dirs(&home) {
        for agent in crate::prompt_formula::TARGET_YAML_AGENTS {
            let candidate = def_dir.join(agent);
            if let Ok(text) = std::fs::read_to_string(&candidate) {
                checked += 1;
                if crate::prompt_formula::yaml_subagent_is_valid(&text) {
                    configured += 1;
                } else {
                    let path_str = candidate.to_string_lossy().into_owned();
                    if text.contains("# [cctx-subagent-instructions]") && invalid_yaml.len() < 8 {
                        invalid_yaml.push(path_str.clone());
                    }
                    if unconfigured.len() < 8 {
                        unconfigured.push(path_str);
                    }
                }
            }
        }
    }
    json!({
        "checked": checked,
        "configured": configured,
        "auto_patched": 0,
        "unconfigured": unconfigured,
        "invalid_yaml": invalid_yaml,
        "remediation": if unconfigured.is_empty() { Value::Null } else { json!("Run: cctx init --patch-subagents or python3 scripts/configure_subagents.py") },
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

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

pub fn resolve_usage_window(
    hours: f64,
    window: Option<&str>,
    now_unix: f64,
) -> Result<(f64, Option<String>)> {
    let Some(raw) = window.map(str::trim).filter(|s| !s.is_empty()) else {
        if !hours.is_finite() || hours <= 0.0 {
            return Err(invalid("hours must be greater than zero").into());
        }
        return Ok((hours, None));
    };
    let lower = raw.to_ascii_lowercase();
    match lower.as_str() {
        "5h" | "rolling-5h" | "burst" => Ok((5.0, Some("Last 5h (rolling burst)".into()))),
        "24h" | "1d" | "day" | "daily" => Ok((24.0, Some("Last 24h".into()))),
        "7d" | "weekly" | "week" | "168h" => Ok((168.0, Some("Last 7d (weekly quota)".into()))),
        "30d" => Ok((720.0, Some("Last 30d".into()))),
        "month" | "mtd" | "monthly" => {
            let safe_now = now_unix.max(0.0);
            let cur_days = (safe_now / 86_400.0).floor() as i64;
            let (year, month, _) = civil_from_days(cur_days);
            let month_start_unix = (days_from_civil(year, month, 1) as f64) * 86_400.0;
            let elapsed_hours = ((safe_now - month_start_unix) / 3600.0).max(0.1);
            let rounded = ((elapsed_hours * 100.0).round() / 100.0).max(0.1);
            Ok((
                rounded,
                Some(format!("UTC Month-to-Date ({year:04}-{month:02}, {elapsed_hours:.1}h)")),
            ))
        }
        _ => Err(invalid(
            "unsupported --window; expected one of: 5h, 24h, 7d (weekly), 30d, month (mtd)",
        )
        .into()),
    }
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

fn find_trailing_trace_footer(s: &str) -> Option<(usize, &str)> {
    let pos = s.rfind("CCTX_TRACE {")?;
    let tail = s[pos..].trim_end();
    if tail.contains('\n') {
        return None;
    }
    let json_body = tail.strip_prefix("CCTX_TRACE ")?;
    if !json_body.starts_with('{') || !json_body.ends_with('}') {
        return None;
    }
    let trim_idx = if pos > 0 && s.as_bytes()[pos - 1] == b'\n' {
        pos - 1
    } else {
        pos
    };
    Some((trim_idx, json_body))
}

/// Reads the `CCTX_TRACE {...}` timing footer that native/MCP surfaces append to results.
fn extract_trace_total_ms(val: &Value, depth: usize) -> Option<f64> {
    if depth > 6 {
        return None;
    }
    match val {
        Value::String(s) => {
            if let Some((_, body)) = find_trailing_trace_footer(s)
                && let Some(ms) = serde_json::from_str::<Value>(body)
                    .ok()
                    .and_then(|v| v.get("total_ms")?.as_f64())
            {
                return Some(ms);
            }
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
        let trimmed = if let Some((idx, body)) = find_trailing_trace_footer(s)
            && serde_json::from_str::<Value>(body).is_ok()
        {
            &s[..idx]
        } else {
            match s.rfind("\nCCTX_TRACE {") {
                Some(idx) => &s[..idx],
                None => s,
            }
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

pub(crate) fn fmt_dur(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 {
        return "0s".into();
    }
    if secs < 0.95 {
        return format!("{secs:.1}s");
    }
    let total = secs.round() as u64;
    if total < 60 {
        return format!("{total}s");
    }
    if total >= 3600 {
        let h = total / 3600;
        let m = (total % 3600) / 60;
        if m > 0 {
            format!("{h}h {m}m")
        } else {
            format!("{h}h")
        }
    } else {
        let m = total / 60;
        let s = total % 60;
        if m >= 15 || s == 0 {
            format!("{}m", (secs / 60.0).round() as u64)
        } else {
            format!("{m}m {s}s")
        }
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

pub fn usage(root: &Path, hours: f64, as_json: bool, details: bool) -> Result<String> {
    usage_with_window(root, hours, None, as_json, details)
}

fn discover_usage_state_homes() -> Vec<PathBuf> {
    let mut homes: Vec<PathBuf> = Vec::with_capacity(4);
    for var in ["HOME", "USERPROFILE", "XDG_STATE_HOME"] {
        if let Some(val) = std::env::var_os(var)
            && !val.is_empty()
        {
            let p = PathBuf::from(val);
            if !homes.contains(&p) {
                homes.push(p);
            }
        }
    }
    #[cfg(windows)]
    if let (Some(drive), Some(path)) =
        (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH"))
    {
        let mut combined = drive;
        combined.push(path);
        let p = PathBuf::from(combined);
        if !p.as_os_str().is_empty() && !homes.contains(&p) {
            homes.push(p);
        }
    }
    if homes.is_empty() {
        homes.push(PathBuf::from("."));
    }
    // Avoid touching WSL DrvFS (`/mnt/c/Users`) unless explicitly opted in via `CCTX_SCAN_WINDOWS_HOMES=1`,
    // preventing 9P filesystem stalls on daemon worker threads.
    if std::env::var_os("CCTX_SCAN_WINDOWS_HOMES").is_some_and(|v| v == "1")
        && homes.first().is_some_and(|h| h.starts_with("/home/"))
        && let Ok(users) = std::fs::read_dir("/mnt/c/Users")
    {
        for entry in users.flatten().take(8) {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if matches!(
                name_str.as_ref(),
                "Public" | "Default" | "Default User" | "All Users" | "desktop.ini"
            ) {
                continue;
            }
            let win_home = entry.path();
            if win_home.is_dir() && !homes.contains(&win_home) {
                homes.push(win_home);
            }
        }
    }
    homes
}

pub fn usage_compact_json_str(
    hours: f64,
    window: Option<&str>,
    daemon_status: Option<Value>,
) -> Result<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64();
    let (effective_hours, window_label) = resolve_usage_window(hours, window, now)?;
    let homes = discover_usage_state_homes();
    let registered_specs = catalog::load().unwrap_or_default();
    usage_from_homes_with_label(
        &homes,
        effective_hours,
        window_label.as_deref(),
        now,
        daemon_status,
        &registered_specs,
        true,
        false,
        false,
    )
}

pub fn usage_with_window(
    root: &Path,
    hours: f64,
    window: Option<&str>,
    as_json: bool,
    details: bool,
) -> Result<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64();
    let (effective_hours, window_label) = resolve_usage_window(hours, window, now)?;

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

    let homes = discover_usage_state_homes();
    let registered_specs = catalog::load().unwrap_or_default();
    usage_from_homes_with_label(
        &homes,
        effective_hours,
        window_label.as_deref(),
        now,
        daemon_status,
        &registered_specs,
        as_json,
        details,
        true,
    )
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
    override_target: Option<&str>,
    session_cwd: Option<&str>,
    registered_specs: &[crate::catalog::Spec],
    git_root_cache: &mut std::collections::BTreeMap<String, Option<String>>,
    spec_filter_cache: &mut std::collections::BTreeMap<PathBuf, SpecHostFilter>,
) -> (Option<String>, Option<String>, bool) {
    let explicit_root = args.get("root").and_then(Value::as_str);
    let raw_target = override_target.or_else(|| first_target_path_str(args));
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
        let is_builtin_or_shell =
            classify_tool_group(tool) == "builtin_search_read" || override_target.is_some();
        if is_builtin_or_shell && let Some(t) = raw_target {
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

#[cfg(test)]
fn usage_from_home(
    state_home: &Path,
    hours: f64,
    now: f64,
    daemon_status: Option<Value>,
    registered_specs: &[crate::catalog::Spec],
    as_json: bool,
    details: bool,
) -> Result<String> {
    usage_from_home_with_label(
        state_home,
        hours,
        None,
        now,
        daemon_status,
        registered_specs,
        as_json,
        details,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn usage_from_home_with_label(
    state_home: &Path,
    hours: f64,
    window_label: Option<&str>,
    now: f64,
    daemon_status: Option<Value>,
    registered_specs: &[crate::catalog::Spec],
    as_json: bool,
    details: bool,
) -> Result<String> {
    usage_from_homes_with_label(
        &[state_home.to_path_buf()],
        hours,
        window_label,
        now,
        daemon_status,
        registered_specs,
        as_json,
        details,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn usage_from_homes_with_label(
    state_homes: &[PathBuf],
    hours: f64,
    window_label: Option<&str>,
    now: f64,
    daemon_status: Option<Value>,
    registered_specs: &[crate::catalog::Spec],
    as_json: bool,
    details: bool,
    pretty_json: bool,
) -> Result<String> {
    let primary_home = state_homes
        .first()
        .map(PathBuf::as_path)
        .unwrap_or_else(|| Path::new("."));
    let cutoff = (now - hours * 3600.0).max(0.0);
    let mut session_files_by_id: std::collections::BTreeMap<String, (f64, PathBuf)> =
        std::collections::BTreeMap::new();
    for home_candidate in state_homes {
        let session_dir = home_candidate.join(".copilot").join("session-state");
        if let Ok(entries) = std::fs::read_dir(&session_dir) {
            for entry in entries.flatten() {
                let sid = entry.file_name().to_string_lossy().into_owned();
                let events_path = entry.path().join("events.jsonl");
                if let Ok(meta) = events_path.metadata() {
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0.0, |d| d.as_secs_f64());
                    if mtime >= cutoff {
                        match session_files_by_id.get(&sid) {
                            Some((prev_mtime, _)) if *prev_mtime >= mtime => {}
                            _ => {
                                session_files_by_id.insert(sid, (mtime, events_path));
                            }
                        }
                    }
                }
            }
        }
    }
    let session_files: Vec<PathBuf> = session_files_by_id.into_values().map(|(_, p)| p).collect();

    #[derive(Default)]
    struct TurnRec {
        ts: f64,
        harness: &'static str,
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
        is_shell_lookup: bool,
        non_source_host_only: bool,
        pre_edit_view: bool,
        saved_tokens: u64,
        compounded_tokens: u64,
        payload_saved_aic: f64,
        avoided_turns: f64,
        turn_saved_tokens: u64,
        turn_saved_aic: f64,
        turn_bucket_id: i64,
        effective_avoided_turns: f64,
        turn_saved_secs: f64,
        io_saved_secs: f64,
        prefill_saved_secs: f64,
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
        let mut last_usage_sig_by_agent: std::collections::BTreeMap<
            String,
            (String, f64, String, u64, u64),
        > = std::collections::BTreeMap::new();
        let mut line = String::with_capacity(4096);
        while let Ok(Some(valid_line)) =
            codex::read_bounded_jsonl_line(&mut reader, &mut line, codex::MAX_JSONL_LINE_BYTES)
        {
            if !valid_line {
                continue;
            }
            if !line.contains("\"session.usage_record\"")
                && !line.contains("\"assistant.usage\"")
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
            } else if (event_type == "session.usage_record" || event_type == "assistant.usage")
                && let Some(m) = event
                    .get("data")
                    .and_then(|d| d.get("usage").or(Some(d)))
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
            if event_type == "session.usage_record" || event_type == "assistant.usage" {
                let usage_obj = event.get("data").and_then(|d| {
                    if event_type == "session.usage_record" {
                        d.get("usage")
                    } else {
                        d.get("usage").or(Some(d))
                    }
                });
                if let Some(u) = usage_obj {
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
                    let rates = extract_token_rates(u, Some(&model));
                    active_sessions.insert(session_id.clone());
                    let in_tok = u.get("inputTokens").and_then(Value::as_u64).unwrap_or(0);
                    let out_tok = u.get("outputTokens").and_then(Value::as_u64).unwrap_or(0);
                    let is_dup_pair = last_usage_sig_by_agent.get(&agent_id).is_some_and(
                        |(prev_type, prev_ts, prev_m, prev_in, prev_out)| {
                            prev_type != event_type
                                && (ts - *prev_ts).abs() <= 2.0
                                && *prev_m == model
                                && *prev_in == in_tok
                                && *prev_out == out_tok
                        },
                    );
                    if !is_dup_pair {
                        last_usage_sig_by_agent.insert(
                            agent_id.clone(),
                            (event_type.to_owned(), ts, model.clone(), in_tok, out_tok),
                        );
                        turns.push(TurnRec {
                            ts,
                            harness: "copilot",
                            session: session_id.clone(),
                            agent_id,
                            model,
                            display_name,
                            effort,
                            input_tokens: in_tok,
                            cache_read_tokens: u
                                .get("cacheReadTokens")
                                .and_then(Value::as_u64)
                                .unwrap_or(0),
                            cache_write_tokens: u
                                .get("cacheWriteTokens")
                                .and_then(Value::as_u64)
                                .unwrap_or(0),
                            output_tokens: out_tok,
                            reasoning_tokens: u
                                .get("reasoningTokens")
                                .and_then(Value::as_u64)
                                .unwrap_or(0),
                            duration_ms: u.get("duration").and_then(Value::as_f64).unwrap_or(0.0),
                            nano_aiu,
                            rates,
                            bpe_saved_aic: 0.0,
                        });
                    }
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
                let shell_info = shell_lookup::inspect_shell_call(&tool, &args);
                let target = shell_info
                    .inferred_target
                    .clone()
                    .or_else(|| {
                        args.get("path")
                            .or_else(|| args.get("paths"))
                            .or_else(|| args.get("filePath"))
                            .or_else(|| args.get("file_path"))
                            .or_else(|| args.get("input"))
                            .or_else(|| args.get("patch"))
                            .map(Value::to_string)
                    })
                    .unwrap_or_default();
                let effective_session_cwd = shell_info.inferred_cwd.as_deref().or(cwd.as_deref());
                let (effective_cwd, unregistered_root, oversized_host_only) = infer_call_workspace(
                    &tool,
                    &args,
                    shell_info
                        .is_source_lookup
                        .then_some(shell_info.inferred_target.as_deref())
                        .flatten(),
                    effective_session_cwd,
                    registered_specs,
                    &mut git_root_cache,
                    &mut spec_filter_cache,
                );
                let non_source_host_only = oversized_host_only
                    || shell_info.non_source_host_only
                    || (classify_tool_group(&tool) == "builtin_search_read"
                        && shell_lookup::is_non_source_target(&target));
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
                entry.is_shell_lookup = shell_info.is_source_lookup;
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
    let mut claude_files_by_stem: std::collections::BTreeMap<String, (f64, PathBuf)> =
        std::collections::BTreeMap::new();
    for home_candidate in state_homes {
        let claude_projects_dir = home_candidate.join(".claude").join("projects");
        if let Ok(proj_entries) = std::fs::read_dir(&claude_projects_dir) {
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
                                let key = f_path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| f_path.to_string_lossy().into_owned());
                                match claude_files_by_stem.get(&key) {
                                    Some((prev_m, _)) if *prev_m >= mtime => {}
                                    _ => {
                                        claude_files_by_stem.insert(key, (mtime, f_path));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let claude_files: Vec<PathBuf> = claude_files_by_stem.into_values().map(|(_, p)| p).collect();
    if !claude_files.is_empty() {
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
            while let Ok(Some(valid_line)) =
                codex::read_bounded_jsonl_line(&mut reader, &mut line, codex::MAX_JSONL_LINE_BYTES)
            {
                if !valid_line {
                    continue;
                }
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
                        let cw_sub = u.get("cache_creation");
                        let cw_5m = cw_sub
                            .and_then(|c| c.get("ephemeral_5m_input_tokens"))
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let cw_1h = cw_sub
                            .and_then(|c| c.get("ephemeral_1h_input_tokens"))
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let total_in_turn = uncached_in + cw_in + cr_in;
                        let out_turn = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                        let explicit_rsn = u
                            .get("thinking_tokens")
                            .or_else(|| u.get("reasoning_tokens"))
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let block_rsn: u64 = if explicit_rsn > 0 {
                            explicit_rsn
                        } else {
                            let prof = crate::prompt_formula::TokenizerProfile::for_model(&model);
                            msg.get("content")
                                .and_then(Value::as_array)
                                .map(|arr| {
                                    arr.iter()
                                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("thinking"))
                                        .filter_map(|b| {
                                            b.get("thinking")
                                                .or_else(|| b.get("text"))
                                                .and_then(Value::as_str)
                                        })
                                        .map(|txt| crate::prompt_formula::estimate_bpe_tokens_for(txt, prof) as u64)
                                        .sum()
                                })
                                .unwrap_or(0)
                        };
                        let rates = rates_for_claude_turn(
                            &model,
                            cw_in,
                            cw_5m,
                            cw_1h,
                            agent_id != "main",
                        );
                        // Anthropic `output_tokens` already bills thinking + text at output rate;
                        // clamp estimated `reasoning_tokens` to `out_turn` when inferred from thinking blocks.
                        let rsn_turn = if explicit_rsn > 0 {
                            explicit_rsn
                        } else {
                            block_rsn.min(out_turn.max(block_rsn))
                        };
                        let est_aic = (uncached_in as f64) * rates.input
                            + (cw_in as f64) * rates.cache_write
                            + (cr_in as f64) * rates.cache_read
                            + (out_turn as f64) * rates.output;
                        active_sessions.insert(sid.clone());
                        turns.push(TurnRec {
                            ts,
                            harness: "claude-code",
                            session: sid.clone(),
                            agent_id: agent_id.clone(),
                            display_name: model.clone(),
                            model,
                            effort: "claude-agent".into(),
                            input_tokens: total_in_turn,
                            cache_read_tokens: cr_in,
                            cache_write_tokens: cw_in,
                            output_tokens: out_turn,
                            reasoning_tokens: rsn_turn,
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
                            let shell_info = shell_lookup::inspect_shell_call(&tool, &args);
                            let target = shell_info
                                .inferred_target
                                .clone()
                                .or_else(|| {
                                    args.get("path")
                                        .or_else(|| args.get("paths"))
                                        .or_else(|| args.get("filePath"))
                                        .or_else(|| args.get("file_path"))
                                        .map(Value::to_string)
                                })
                                .unwrap_or_default();
                            let effective_session_cwd =
                                shell_info.inferred_cwd.as_deref().or(cwd.as_deref());
                            let (effective_cwd, unregistered_root, host_only) = infer_call_workspace(
                                &tool,
                                &args,
                                shell_info
                                    .is_source_lookup
                                    .then_some(shell_info.inferred_target.as_deref())
                                    .flatten(),
                                effective_session_cwd,
                                registered_specs,
                                &mut git_root_cache,
                                &mut spec_filter_cache,
                            );
                            let non_source_host_only = host_only
                                || shell_info.non_source_host_only
                                || (classify_tool_group(&tool) == "builtin_search_read"
                                    && shell_lookup::is_non_source_target(&target));
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
                            entry.is_shell_lookup = shell_info.is_source_lookup;
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
                                entry.trace_total_ms = extract_trace_total_ms(content, 0);
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

    // OpenAI Codex (`~/.codex/sessions` and `~/.codex/archived_sessions`, `.jsonl` + `.jsonl.zst`)
    // Select the home with the newest observed activity per session so duplicate homes never drop newer turns.
    let codex_ingests: Vec<codex::CodexIngestResult> = state_homes
        .iter()
        .map(|home_candidate| codex::ingest_codex_home(home_candidate, cutoff, now, event_unix_ts))
        .collect();
    let mut best_codex_home_by_session: std::collections::BTreeMap<String, (usize, f64, usize)> =
        std::collections::BTreeMap::new();
    for (home_idx, ci) in codex_ingests.iter().enumerate() {
        let mut sess_stats: std::collections::BTreeMap<&str, (f64, usize)> =
            std::collections::BTreeMap::new();
        for sid in &ci.active_sessions {
            sess_stats.entry(sid.as_str()).or_insert((0.0, 0));
        }
        for ct in &ci.turns {
            let e = sess_stats.entry(ct.session.as_str()).or_insert((0.0, 0));
            e.0 = e.0.max(ct.ts);
            e.1 += 1;
        }
        for cc in &ci.calls {
            let e = sess_stats.entry(cc.session.as_str()).or_insert((0.0, 0));
            e.0 = e.0.max(cc.completed_at.unwrap_or(cc.started_at));
            e.1 += 1;
        }
        for (sid, _, cts) in &ci.explicit_compactions {
            let e = sess_stats.entry(sid.as_str()).or_insert((0.0, 0));
            e.0 = e.0.max(*cts);
            e.1 += 1;
        }
        for (sid, (max_ts, ev_count)) in sess_stats {
            match best_codex_home_by_session.get(sid) {
                Some((_, prev_ts, prev_cnt))
                    if (*prev_ts > max_ts) || (*prev_ts == max_ts && *prev_cnt >= ev_count) => {}
                _ => {
                    best_codex_home_by_session.insert(sid.to_owned(), (home_idx, max_ts, ev_count));
                }
            }
        }
    }
    let mut codex_quota = codex::CodexQuotaSummary::default();
    let mut codex_explicit_compactions: Vec<(String, String, f64)> = Vec::new();
    for (home_idx, codex_ingest) in codex_ingests.into_iter().enumerate() {
        let mut accepted_in_home = 0usize;
        for sid in codex_ingest.active_sessions {
            if best_codex_home_by_session
                .get(&sid)
                .is_some_and(|(idx, _, _)| *idx == home_idx)
            {
                accepted_in_home += 1;
                active_sessions.insert(sid);
            }
        }
        if accepted_in_home > 0 || (codex_quota.sessions_with_limits == 0 && best_codex_home_by_session.is_empty()) {
            let q = codex_ingest.quota;
            if q.latest_ts >= codex_quota.latest_ts
                && (q.primary_5h_used_pct.is_some() || q.secondary_7d_used_pct.is_some())
            {
                codex_quota = q;
            }
        }
        for comp in codex_ingest.explicit_compactions {
            if best_codex_home_by_session
                .get(&comp.0)
                .is_some_and(|(idx, _, _)| *idx == home_idx)
            {
                codex_explicit_compactions.push(comp);
            }
        }
        for (sid, c) in codex_ingest.session_cwds {
            if best_codex_home_by_session
                .get(&sid)
                .is_none_or(|(idx, _, _)| *idx == home_idx)
            {
                session_cwds.insert(sid, c);
            }
        }
        for ct in codex_ingest.turns {
            if best_codex_home_by_session
                .get(&ct.session)
                .is_some_and(|(idx, _, _)| *idx != home_idx)
            {
                continue;
            }
            turns.push(TurnRec {
                ts: ct.ts,
                harness: "codex",
                session: ct.session,
                agent_id: ct.agent_id,
                display_name: ct.model.clone(),
                model: ct.model,
                effort: ct.effort,
                input_tokens: ct.input_tokens,
                cache_read_tokens: ct.cache_read_tokens,
                cache_write_tokens: ct.cache_write_tokens,
                output_tokens: ct.output_tokens,
                reasoning_tokens: ct.reasoning_tokens,
                duration_ms: 0.0,
                nano_aiu: ct.est_nano_aiu,
                rates: ct.rates,
                bpe_saved_aic: 0.0,
            });
        }
        for cc in codex_ingest.calls {
            if best_codex_home_by_session
                .get(&cc.session)
                .is_some_and(|(idx, _, _)| *idx != home_idx)
            {
                continue;
            }
            let shell_info = shell_lookup::inspect_shell_call(&cc.tool, &cc.args);
            let target = shell_info
                .inferred_target
                .clone()
                .or_else(|| {
                    cc.args
                        .get("path")
                        .or_else(|| cc.args.get("paths"))
                        .or_else(|| cc.args.get("filePath"))
                        .or_else(|| cc.args.get("file_path"))
                        .or_else(|| cc.args.get("patch"))
                        .map(Value::to_string)
                })
                .unwrap_or_default();
            let effective_session_cwd = shell_info.inferred_cwd.as_deref().or(cc.cwd.as_deref());
            let (effective_cwd, unregistered_root, host_only) = infer_call_workspace(
                &cc.tool,
                &cc.args,
                shell_info
                    .is_source_lookup
                    .then_some(shell_info.inferred_target.as_deref())
                    .flatten(),
                effective_session_cwd,
                registered_specs,
                &mut git_root_cache,
                &mut spec_filter_cache,
            );
            let non_source_host_only = host_only
                || shell_info.non_source_host_only
                || (classify_tool_group(&cc.tool) == "builtin_search_read"
                    && shell_lookup::is_non_source_target(&target));
            let (batch_ranges, has_anchor, around_line) = read_call_shape(&cc.args);
            seq_counter += 1;
            let entry = calls.entry((cc.session.clone(), cc.call_id)).or_default();
            entry.session = cc.session;
            entry.agent_id = cc.agent_id;
            entry.seq = seq_counter;
            entry.cwd = effective_cwd;
            entry.unregistered_root = unregistered_root;
            entry.tool = cc.tool.clone();
            entry.target = target;
            entry.model = cc.model;
            entry.started_at = Some(cc.started_at);
            entry.completed_at = cc.completed_at;
            entry.success = cc.success;
            entry.batch_ranges = batch_ranges;
            entry.has_anchor = has_anchor;
            entry.around_line = around_line;
            entry.turn_compressed = is_turn_compressed(&cc.tool, &cc.args);
            entry.is_shell_lookup = shell_info.is_source_lookup;
            entry.non_source_host_only = non_source_host_only;
            if let Some(ref out_str) = cc.output_text {
                let val = Value::String(out_str.clone());
                let (chars, offloaded, seq_slice) = extract_result_chars(&val, 0);
                entry.payload_tokens = chars.div_ceil(4) as u64;
                entry.offloaded = offloaded;
                entry.sequential_slice = seq_slice;
                entry.trace_total_ms = extract_trace_total_ms(&val, 0);
                if cc.success == Some(false) {
                    entry.error_detail = outcome::detail(Some(out_str));
                    entry.cache_miss = out_str.contains("CACHE_MISS root=");
                    entry.cache_load_failed = out_str.contains("CACHE_LOAD_FAILED root=");
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

    let bpe_status = crate::prompt_formula::inspect_bpe_surfaces(primary_home);
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

    let mut lifecycle = CacheLifecycleFacts::default();
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
            let is_subagent = key.1 != "main";
            let mut resets = Vec::new();
            for k in 1..idxs.len() {
                let prev = &turns[idxs[k - 1]];
                let cur = &turns[idxs[k]];
                let prev_in = prev.input_tokens;
                let cur_in = cur.input_tokens;
                let explicit_compact = codex_explicit_compactions.iter().any(|(s, a, cts)| {
                    s == &key.0
                        && a == &key.1
                        && ((k == 1 && *cts >= prev.ts && *cts <= cur.ts)
                            || (*cts > prev.ts && *cts <= cur.ts))
                });
                if explicit_compact || (prev_in > 60_000 && (cur_in as f64) < (prev_in as f64) * 0.65) {
                    resets.push(k);
                }
                let low_cache_hit = cur_in >= 50_000
                    && (cur.cache_read_tokens as f64) < (cur_in as f64) * 0.50
                    && cur.rates.input > cur.rates.cache_read;
                if low_cache_hit {
                    let config_switched = prev.model != cur.model
                        || (prev.effort != cur.effort && prev.effort != "-" && cur.effort != "-");
                    let dt = (cur.ts - prev.ts).max(0.0);
                    if config_switched {
                        lifecycle.config_bust_turns += 1;
                        let warm_est = (cur_in as f64)
                            * (0.95 * cur.rates.cache_read
                                + 0.05 * cold_prefix_rate(cur.cache_write_tokens, cur.rates))
                            + (cur.output_tokens as f64) * cur.rates.output;
                        lifecycle.config_bust_aic += (cur.effective_turn_aic() - warm_est).max(0.0);
                    } else if is_subagent && dt >= 300.0 {
                        lifecycle.ttl_5m_subagent_turns += 1;
                    } else if !is_subagent && dt >= 1800.0 {
                        lifecycle.ttl_long_main_turns += 1;
                    }
                }
            }
            // Detect premature or excessive compactions in this stream.
            if resets.len() >= 3 && idxs.len() < 30 {
                lifecycle.premature_compactions += resets.len();
                for &r in &resets {
                    let rt = &turns[idxs[r]];
                    lifecycle.premature_compaction_write_tokens += rt.cache_write_tokens.max(
                        rt.input_tokens.saturating_sub(rt.cache_read_tokens),
                    );
                }
            } else {
                let stream_last_ts = idxs.last().map(|&i| turns[i].ts).unwrap_or(0.0);
                let stream_is_idle_or_closed = now - stream_last_ts >= 900.0;
                for (ri, &r) in resets.iter().enumerate() {
                    let has_subsequent_reset = ri + 1 < resets.len();
                    let next_bound = resets.get(ri + 1).copied().unwrap_or(idxs.len());
                    let followup_turns = next_bound.saturating_sub(r + 1);
                    let rt = &turns[idxs[r]];
                    let fresh_rebuilt = rt.cache_write_tokens.max(
                        rt.input_tokens.saturating_sub(rt.cache_read_tokens),
                    );
                    if followup_turns < 5
                        && fresh_rebuilt >= 25_000
                        && (has_subsequent_reset || stream_is_idle_or_closed)
                    {
                        lifecycle.premature_compactions += 1;
                        lifecycle.premature_compaction_write_tokens += fresh_rebuilt;
                    }
                }
            }
            // Detect short subagent spawns paying heavy fresh 5m cache creation.
            if is_subagent && idxs.len() <= 4 {
                let sub_fresh: u64 = idxs
                    .iter()
                    .map(|&i| {
                        let t = &turns[i];
                        if t.cache_write_tokens > 0 {
                            t.cache_write_tokens
                        } else {
                            t.input_tokens.saturating_sub(t.cache_read_tokens)
                        }
                    })
                    .sum();
                if sub_fresh >= 25_000 {
                    lifecycle.subagent_short_cold_spawns += 1;
                    lifecycle.subagent_cold_write_tokens += sub_fresh;
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
    let mut all_turn_durs: Vec<f64> = turns.iter().map(|t| t.duration_ms).filter(|d| *d > 0.0).collect();
    all_turn_durs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let global_median_turn_ms = if all_turn_durs.is_empty() {
        4_500.0
    } else {
        all_turn_durs[all_turn_durs.len() / 2]
    };
    let mut model_durs_map: std::collections::BTreeMap<&str, Vec<f64>> = std::collections::BTreeMap::new();
    for t in &turns {
        if t.duration_ms > 0.0 {
            model_durs_map.entry(t.model.as_str()).or_default().push(t.duration_ms);
        }
    }
    let model_median_turn_ms: std::collections::BTreeMap<String, f64> = model_durs_map
        .into_iter()
        .map(|(m, mut ds)| {
            ds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            (m.to_owned(), ds[ds.len() / 2])
        })
        .collect();
    let mut fallback_durs: Vec<f64> = valid_calls
        .iter()
        .filter(|c| {
            !c.non_source_host_only
                && !c.pre_edit_view
                && (c.is_shell_lookup || classify_tool_group(&c.tool) == "builtin_search_read")
        })
        .filter_map(|c| match (c.started_at, c.completed_at) {
            (Some(s), Some(e)) if e > s => Some((e - s) * 1000.0),
            _ => None,
        })
        .collect();
    fallback_durs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let observed_fallback_ms = (fallback_durs.len() >= 5)
        .then(|| fallback_durs[fallback_durs.len() / 2].clamp(45.0, 250.0));

    for c in &mut valid_calls {
        let eligible_cctx = is_lookup_tool_name(&c.tool)
            && !c.non_source_host_only
            && !c.pre_edit_view
            && matches!(classify_tool_group(&c.tool), "native" | "mcp")
            && c.success == Some(true);
        if !eligible_cctx {
            continue;
        }
        let t_ref = c.completed_at.or(c.started_at).unwrap_or(0.0);
        let stream_opt = streams
            .get(&(c.session.clone(), c.agent_id.clone()))
            .or_else(|| streams.get(&(c.session.clone(), "main".into())));
        if let Some(stream) = stream_opt
            && !stream.idxs.is_empty()
        {
            if let Some(pos) = (0..stream.idxs.len()).find(|&k| turns[stream.idxs[k]].ts >= t_ref) {
                c.turn_bucket_id = pos as i64;
                let end_pos = stream
                    .resets
                    .iter()
                    .copied()
                    .find(|&r| r > pos)
                    .unwrap_or(stream.idxs.len());
                let t0 = &turns[stream.idxs[pos]];
                if c.model.is_none() {
                    c.model = Some(t0.model.clone());
                }
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
                c.turn_bucket_id = -1_000_000_000_i64.saturating_sub((t_ref / 2.0).floor() as i64);
                let t_last = &turns[*stream.idxs.last().unwrap()];
                if c.model.is_none() {
                    c.model = Some(t_last.model.clone());
                }
                c.payload_saved_aic =
                    (c.saved_tokens as f64) * cold_prefix_rate(t_last.cache_write_tokens, t_last.rates);
            }
            c.turn_saved_aic = c.avoided_turns * stream.mean_warm_aic;
            c.turn_saved_tokens = (c.avoided_turns * stream.mean_warm_in).round() as u64;
        } else {
            c.turn_bucket_id = -1_000_000_000_i64.saturating_sub((t_ref / 2.0).floor() as i64);
            c.compounded_tokens = c.saved_tokens * COMPOUND_CACHE_MULTIPLIER;
            c.payload_saved_aic = (c.saved_tokens as f64) * AIC_PER_SAVED_TOKEN;
            c.turn_saved_aic = c.avoided_turns * AIC_PER_AVOIDED_TURN;
            c.turn_saved_tokens = (c.avoided_turns * TOKENS_PER_AVOIDED_TURN).round() as u64;
        }
    }

    // Group eligible successful CCTX calls by turn bucket so parallel tool bursts within a single turn
    // never multiply sequential LLM round-trip or I/O latency, while preserving single-call multi-range batch credit.
    let mut bucket_totals: std::collections::BTreeMap<(String, String, i64), (f64, f64, usize)> =
        std::collections::BTreeMap::new();
    for c in &valid_calls {
        let eligible_cctx = is_lookup_tool_name(&c.tool)
            && !c.non_source_host_only
            && !c.pre_edit_view
            && matches!(classify_tool_group(&c.tool), "native" | "mcp")
            && c.success == Some(true);
        if eligible_cctx {
            let entry = bucket_totals
                .entry((c.session.clone(), c.agent_id.clone(), c.turn_bucket_id))
                .or_insert((0.0, 0.0, 0));
            entry.0 += c.avoided_turns;
            entry.1 = entry.1.max(c.avoided_turns);
            entry.2 += 1;
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
        let (raw_sum, max_single, concurrency) = bucket_totals
            .get(&(c.session.clone(), c.agent_id.clone(), c.turn_bucket_id))
            .copied()
            .unwrap_or((c.avoided_turns, c.avoided_turns, 1));
        let capped_credit = raw_sum.min(max_single.max(1.5));
        let wall_avoided = if raw_sum > 0.0 {
            c.avoided_turns * (capped_credit / raw_sum)
        } else {
            0.0
        };
        c.effective_avoided_turns = wall_avoided;
        let model_ms = c
            .model
            .as_deref()
            .and_then(|m| model_median_turn_ms.get(m).copied())
            .unwrap_or(global_median_turn_ms);
        let lookup_turn_secs = (model_ms / 1000.0).clamp(2.5, 10.0);
        c.turn_saved_secs = wall_avoided * lookup_turn_secs;
        let is_read_op = c.tool.ends_with("code_read")
            || c.tool.ends_with("codeRead")
            || c.tool.ends_with("NativeRead");
        let default_baseline_ms = if is_read_op { 35.0 } else { 110.0 };
        let baseline_ms = if is_read_op {
            default_baseline_ms
        } else {
            observed_fallback_ms.unwrap_or(default_baseline_ms)
        };
        let actual_ms = c.trace_total_ms.unwrap_or(2.0);
        c.io_saved_secs = ((baseline_ms - actual_ms).max(0.0) / 1000.0) / (concurrency.max(1) as f64);
        c.prefill_saved_secs = ((c.saved_tokens + c.compounded_tokens) as f64) / 200_000.0;
    }

    let is_eligible_lookup = |c: &&CallRec| -> bool {
        (is_lookup_tool_name(&c.tool) || c.is_shell_lookup) && !c.non_source_host_only && !c.pre_edit_view
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
    let total_cache_write: u64 = turns.iter().map(|t| t.cache_write_tokens).sum();
    let total_uncached_in: u64 = turns
        .iter()
        .map(|t| {
            t.input_tokens
                .saturating_sub(t.cache_read_tokens)
                .saturating_sub(t.cache_write_tokens)
        })
        .sum();
    let total_fresh_and_write_in = total_uncached_in + total_cache_write;
    let total_out: u64 = turns.iter().map(|t| t.output_tokens).sum();
    let total_rsn: u64 = turns.iter().map(|t| t.reasoning_tokens).sum();
    let total_nano_aiu: u64 = turns.iter().map(|t| t.nano_aiu).sum();
    let total_used_aic = total_nano_aiu as f64 / 1_000_000_000.0;
    let uncached_list_illusion_aic: f64 = turns
        .iter()
        .map(|t| (t.input_tokens as f64) * t.rates.input + (t.output_tokens as f64) * t.rates.output)
        .sum();
    let cache_illusion_ratio = if total_used_aic > 0.001 {
        (100.0 * uncached_list_illusion_aic / total_used_aic).round() / 100.0
    } else {
        1.0
    };
    let weighted_cw_equiv: f64 = turns
        .iter()
        .map(|t| {
            let mult = if t.rates.input > 0.0 && t.cache_write_tokens > 0 {
                (cold_prefix_rate(t.cache_write_tokens, t.rates) / t.rates.input).clamp(1.0, 2.5)
            } else {
                1.25
            };
            (t.cache_write_tokens as f64) * mult
        })
        .sum();
    let quota_weighted_conservative: u64 = (total_uncached_in as f64
        + weighted_cw_equiv
        + (total_cache as f64) * 0.10
        + (total_out as f64) * 5.0)
        .round() as u64;
    let quota_weighted_strict: u64 = (total_uncached_in as f64
        + weighted_cw_equiv
        + (total_cache as f64) * 1.00
        + (total_out as f64) * 5.0)
        .round() as u64;
    let shell_lookup_cnt = lookup_calls.iter().filter(|c| c.is_shell_lookup).count();
    let avoided_premium_requests_est: f64 = cctx_calls
        .iter()
        .map(|c| {
            let mult = c
                .model
                .as_deref()
                .map(premium_request_multiplier)
                .unwrap_or(1.0);
            c.avoided_turns * 0.25 * mult
        })
        .sum();
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
    let effective_avoided_turns_total: f64 = cctx_calls.iter().map(|c| c.effective_avoided_turns).sum();
    let turn_comp_cnt = avoided_turns_total.round() as u64;
    let turn_saved: u64 = cctx_calls.iter().map(|c| c.turn_saved_tokens).sum();
    let total_saved = direct_saved + compounded_saved + turn_saved + bpe_saved_tokens;
    let payload_saved_aic_total: f64 = cctx_calls.iter().map(|c| c.payload_saved_aic).sum();
    let turn_saved_aic_total: f64 = cctx_calls.iter().map(|c| c.turn_saved_aic).sum();
    let total_saved_aic = payload_saved_aic_total + turn_saved_aic_total + bpe_saved_aic;
    let bpe_saved_secs_per_turn = (bpe_turn_tok as f64) / 200_000.0;
    let turn_time_saved_secs: f64 = cctx_calls.iter().map(|c| c.turn_saved_secs).sum();
    let io_time_saved_secs: f64 = cctx_calls.iter().map(|c| c.io_saved_secs).sum();
    let prefill_time_saved_secs: f64 = cctx_calls.iter().map(|c| c.prefill_saved_secs).sum::<f64>()
        + (turns.len() as f64) * bpe_saved_secs_per_turn;
    let total_time_saved_secs = turn_time_saved_secs + io_time_saved_secs + prefill_time_saved_secs;
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
        let cw_t: u64 = m_turns.iter().map(|t| t.cache_write_tokens).sum();
        let uncached_t: u64 = m_turns
            .iter()
            .map(|t| {
                t.input_tokens
                    .saturating_sub(t.cache_read_tokens)
                    .saturating_sub(t.cache_write_tokens)
            })
            .sum();
        let in_t: u64 = m_turns.iter().map(|t| t.input_tokens).sum();
        let pr_mult = premium_request_multiplier(&m_id);
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
        let m_saved_secs: f64 = m_cctx
            .iter()
            .map(|c| c.turn_saved_secs + c.io_saved_secs + c.prefill_saved_secs)
            .sum::<f64>()
            + (m_turns.len() as f64) * bpe_saved_secs_per_turn;
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
            "uncached_input_tokens": uncached_t,
            "cache_write_tokens": cw_t,
            "cache_read_tokens": cr_t,
            "prompt_cache_hit_percent": p_cache_pct,
            "output_tokens": out_t,
            "reasoning_tokens": rsn_t,
            "premium_request_multiplier": pr_mult,
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
            "saved_secs": (m_saved_secs * 10.0).round() / 10.0,
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
            group: if c.is_shell_lookup {
                "shell_search_read"
            } else {
                classify_tool_group(&c.tool)
            },
            success: c.success,
            offloaded: c.offloaded,
            error_detail: c.error_detail,
            duration_ms: match (c.started_at, c.completed_at) {
                (Some(s), Some(e)) if e >= s => Some((e - s) * 1000.0),
                _ => None,
            },
            trace_total_ms: c.trace_total_ms,
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
    let insights = usage_insights::analyze_with_lifecycle(
        &call_facts,
        &session_facts,
        &model_facts,
        total_out,
        total_rsn,
        &lifecycle,
    );

    let mut harness_entries = Vec::new();
    for h_key in ["copilot", "claude-code", "codex"] {
        let h_turns: Vec<&TurnRec> = turns.iter().filter(|t| t.harness == h_key).collect();
        if h_turns.is_empty() {
            continue;
        }
        let h_sess: std::collections::BTreeSet<&str> =
            h_turns.iter().map(|t| t.session.as_str()).collect();
        let h_in: u64 = h_turns.iter().map(|t| t.input_tokens).sum();
        let h_cr: u64 = h_turns.iter().map(|t| t.cache_read_tokens).sum();
        let h_cw: u64 = h_turns.iter().map(|t| t.cache_write_tokens).sum();
        let h_uncached: u64 = h_turns
            .iter()
            .map(|t| {
                t.input_tokens
                    .saturating_sub(t.cache_read_tokens)
                    .saturating_sub(t.cache_write_tokens)
            })
            .sum();
        let h_out: u64 = h_turns.iter().map(|t| t.output_tokens).sum();
        let h_rsn: u64 = h_turns.iter().map(|t| t.reasoning_tokens).sum();
        let h_aic: f64 = h_turns.iter().map(|t| t.nano_aiu).sum::<u64>() as f64 / 1_000_000_000.0;
        harness_entries.push(json!({
            "harness": h_key,
            "sessions": h_sess.len(),
            "turns": h_turns.len(),
            "input_tokens": h_in,
            "uncached_input_tokens": h_uncached,
            "cache_write_tokens": h_cw,
            "cache_read_tokens": h_cr,
            "prompt_cache_hit_percent": (h_in > 0).then(|| 100.0 * h_cr as f64 / h_in as f64),
            "output_tokens": h_out,
            "reasoning_tokens": h_rsn,
            "used_aic": h_aic,
        }));
    }

    // OpenAI and Anthropic `output_tokens` already include reasoning/thinking tokens;
    // avoid double-counting reasoning in 3-axis load (`billed_output_tokens = max(total_out, total_rsn)`).
    let billed_output_tokens = total_out.max(total_rsn);
    let codex_quota_json = codex_quota.to_json();
    let three_axis_json = json!({
        "fresh_and_cache_write_tokens": total_fresh_and_write_in,
        "uncached_input_tokens": total_uncached_in,
        "cache_write_tokens": total_cache_write,
        "cache_read_tokens": total_cache,
        "output_and_reasoning_tokens": billed_output_tokens,
        "billed_output_tokens": billed_output_tokens,
        "reasoning_subset_tokens": total_rsn.min(billed_output_tokens),
        "cache_illusion_inflation_ratio": cache_illusion_ratio,
        "quota_sensitivity_band": {
            "conservative_cache_discounted_tokens": quota_weighted_conservative,
            "strict_full_cache_counted_tokens": quota_weighted_strict,
        },
        "copilot_billing_reference": {
            "internal_aiu_units": total_used_aic,
            "discounted_usd_at_0001_per_aiu": total_used_aic * 0.001,
            "official_github_ai_credits_usd_at_001": total_used_aic * 0.01,
            "avoided_legacy_premium_requests_est": (avoided_premium_requests_est * 100.0).round() / 100.0,
        },
    });

    if as_json {
        let mut token_summary_map = serde_json::Map::with_capacity(40);
        if let Value::Object(part1) = json!({
            "total_turns": turns.len(),
            "input_tokens": total_in,
            "uncached_input_tokens": total_uncached_in,
            "cache_write_tokens": total_cache_write,
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
            "shell_lookup_bypass_calls": shell_lookup_cnt,
            "excluded_non_source_calls": non_source_cnt,
            "excluded_pre_edit_views": pre_edit_cnt,
            "direct_tokens_saved": direct_saved,
            "compounded_history_tokens_saved": compounded_saved,
            "effective_cache_reread_multiplier": (effective_reread_mult * 100.0).round() / 100.0,
        }) {
            token_summary_map.extend(part1);
        }
        if let Value::Object(part2) = json!({
            "turn_compression_calls": turn_comp_cnt,
            "sequential_slice_calls": seq_slice_cnt,
            "estimated_turn_tokens_saved": turn_saved,
            "bpe_formula_tokens_saved": bpe_saved_tokens,
            "bpe_formula_saved_aic": bpe_saved_aic,
            "bpe_formula_status": bpe_status,
            "bpe_profile": crate::bpe_builder::load_active_profile()
                .as_ref()
                .map(crate::bpe_builder::BpeProfileSummary::from),
            "estimated_total_tokens_saved": total_saved,
            "payload_saved_aic": payload_saved_aic_total,
            "avoided_turn_saved_aic": turn_saved_aic_total,
            "saved_aic": total_saved_aic,
            "cost_savings_percent": savings_pct,
            "saved_usd_0001": total_saved_aic * 0.001,
            "saved_usd_api_001": total_saved_aic * 0.01,
            "estimated_time_saved_secs": (total_time_saved_secs * 10.0).round() / 10.0,
            "avoided_turn_time_saved_secs": (turn_time_saved_secs * 10.0).round() / 10.0,
            "lookup_io_time_saved_secs": (io_time_saved_secs * 10.0).round() / 10.0,
            "prefill_time_saved_secs": (prefill_time_saved_secs * 10.0).round() / 10.0,
            "effective_avoided_turns": (effective_avoided_turns_total * 10.0).round() / 10.0,
            "avoided_premium_requests_est": (avoided_premium_requests_est * 100.0).round() / 100.0,
        }) {
            token_summary_map.extend(part2);
        }
        let payload = json!({
            "computed_at_unix_ms": (now * 1000.0).round() as u64,
            "hours": hours,
            "window": window_label,
            "sessions": active_sessions.len(),
            "daemon": daemon_status,
            "token_summary": Value::Object(token_summary_map),
            "three_axis_load": three_axis_json,
            "harness_usage": harness_entries,
            "codex_quota": codex_quota_json,
            "cache_lifecycle": {
                "mid_session_config_cache_bust_turns": lifecycle.config_bust_turns,
                "mid_session_config_cache_bust_aic": lifecycle.config_bust_aic,
                "ttl_5m_subagent_expirations": lifecycle.ttl_5m_subagent_turns,
                "ttl_long_main_expirations": lifecycle.ttl_long_main_turns,
                "premature_compactions": lifecycle.premature_compactions,
                "premature_compaction_write_tokens": lifecycle.premature_compaction_write_tokens,
                "subagent_short_cold_spawns": lifecycle.subagent_short_cold_spawns,
                "subagent_cold_write_tokens": lifecycle.subagent_cold_write_tokens,
            },
            "model_usage": if pretty_json {
                Value::Array(model_entries)
            } else {
                Value::Array(model_entries.into_iter().take(24).collect())
            },
            "insights": insights.to_json_bounded(!pretty_json),
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
        if pretty_json {
            return serde_json::to_string_pretty(&payload).map_err(Into::into);
        }
        return serde_json::to_string(&payload).map_err(Into::into);
    }

    if !details {
        return Ok(UsageDisplay::terminal().summary(&UsageSummary {
            hours,
            window_label,
            used_aic: total_used_aic,
            saved_aic: total_saved_aic,
            tokens_saved: total_saved,
            time_saved_secs: total_time_saved_secs,
            warm_pct: cctx_warm_pct,
            adoption_pct: cctx_select_pct,
            models: &model_entries,
            advisories: &insights.advisories,
        }));
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

    let display = UsageDisplay::terminal();
    let details_window = match window_label {
        Some(lbl) => lbl.to_owned(),
        None => format!("Last {hours} hours"),
    };
    let mut lines = vec![
        display.text(&format!("CCTX USAGE / {details_window}"), Style::Title),
        String::new(),
        display.section("ESTIMATED SAVINGS", Style::Title),
        display.text(&format!("${:.2} USD saved", total_saved_aic * 0.001), Style::Savings),
        display.text(&format!("{savings_pct:.1}% less than estimated baseline"), Style::Savings),
        String::new(),
        display.field("Without cctx (est.)", &format!("${:.2}", (total_used_aic + total_saved_aic) * 0.001), Style::Normal),
        display.field("With cctx", &format!("${:.2}", total_used_aic * 0.001), Style::Normal),
        String::new(),
        display.text("USD equivalents @ $0.001/AIC.", Style::Muted),
        display.text("Savings are modeled estimates, not verified billing reductions.", Style::Muted),
        String::new(),
        display.section("EFFICIENCY", Style::Title),
        display.field("Tokens avoided (est.)", &fmt_tok(total_saved), Style::Normal),
        display.field(
            "Time saved (est.)",
            &format!(
                "~{} (turns ~{} + prefill ~{} + index ~{})",
                fmt_dur(total_time_saved_secs),
                fmt_dur(turn_time_saved_secs),
                fmt_dur(prefill_time_saved_secs),
                fmt_dur(io_time_saved_secs)
            ),
            Style::Normal,
        ),
        display.field("Warm cache hits", &fmt_pct(cctx_warm_pct), Style::Normal),
        display.text(&format!("  {cctx_warm} / {} CCTX lookups", cctx_calls.len()), Style::Muted),
        display.field("Source adoption", &fmt_pct(cctx_select_pct), Style::Normal),
        display.text(&format!("  {} / {} eligible; excludes {non_source_cnt} /tmp and {pre_edit_cnt} pre-edit calls", cctx_calls.len(), lookup_calls.len()), Style::Muted),
        display.field("Formula coverage", &fmt_pct(bpe_surface_pct), Style::Normal),
        display.text(&format!("  {}/{} surfaces: ext, mcp, subagent, global; ~{bpe_turn_tok} tokens/turn", bpe_status.active_surfaces, bpe_status.total_surfaces), Style::Muted),
        String::new(),
        display.section("SAVINGS BREAKDOWN / Estimated tokens", Style::Title),
        display.field("Smaller payloads", &fmt_tok(direct_saved), Style::Normal),
        display.field("Cache re-reads", &fmt_tok(compounded_saved), Style::Normal),
        display.text(&format!("  {effective_reread_mult:.1}x cache multiplier"), Style::Muted),
        display.field("Avoided turns", &fmt_tok(turn_saved), Style::Normal),
        display.text(&format!("  ~{turn_comp_cnt} turns avoided"), Style::Muted),
        display.field("Formula compression", &fmt_tok(bpe_saved_tokens), Style::Normal),
        display.text(&format!("  ${:.2} estimated savings (included above)", bpe_saved_aic * 0.001), Style::Muted),
        String::new(),
        display.section("LLM USAGE & 3-AXIS QUOTA LOAD (Cache-Illusion Guard)", Style::Title),
        display.field("Sessions / turns", &format!("{} / {}", active_sessions.len(), turns.len()), Style::Normal),
        display.field("Total tokens", &fmt_tok(total_in + total_out + total_rsn), Style::Normal),
        display.field(
            "1. Fresh + CacheWrite",
            &format!("{} ({} uncached + {} write)", fmt_tok(total_fresh_and_write_in), fmt_tok(total_uncached_in), fmt_tok(total_cache_write)),
            Style::Normal,
        ),
        display.field(
            "2. Cache Read",
            &format!("{} ({} of input)", fmt_tok(total_cache), fmt_pct(prompt_cache_pct)),
            Style::Normal,
        ),
        display.field(
            "3. Billed Output",
            &format!("{} (incl. {} reasoning)", fmt_tok(billed_output_tokens), fmt_tok(total_rsn)),
            Style::Normal,
        ),
        display.field(
            "Quota load band",
            &format!("{} (0.1x cache) .. {} (1.0x strict)", fmt_tok(quota_weighted_conservative), fmt_tok(quota_weighted_strict)),
            Style::Muted,
        ),
        display.text(
            &format!("Cache-illusion check: raw uncached valuation would inflate spend by {cache_illusion_ratio:.1}x."),
            Style::Muted,
        ),
        display.text(&format!("Usage: {total_used_aic:.1} AIU; estimated savings: {total_saved_aic:.1} AIU"), Style::Muted),
        display.text(daemon_line.trim(), Style::Muted),
        String::new(),
        display.section("REFERENCE ONLY / Copilot Billing & Codex Quota", Style::Title),
        display.field("GitHub AI Credits ($0.01)", &format!("${:.2} saved / ${:.2} used (API & metered credit ref.)", total_saved_aic * 0.01, total_used_aic * 0.01), Style::Muted),
        display.field("Legacy Premium Reqs", &format!("~{avoided_premium_requests_est:.1} avoided (multiplier-weighted)"), Style::Muted),
        String::new(),
    ];
    if let Some(p5) = codex_quota.primary_5h_used_pct {
        let s7 = codex_quota.secondary_7d_used_pct.unwrap_or(0.0);
        lines.insert(
            lines.len() - 1,
            display.field(
                "Codex live quota",
                &format!(
                    "5h {p5:.1}% (+{:.1}%) · 7d {s7:.1}% (+{:.1}%)",
                    codex_quota.primary_5h_delta_pct, codex_quota.secondary_7d_delta_pct
                ),
                Style::Muted,
            ),
        );
    }

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
                format!("${:.2} (~{s_aic:.1} AIC)", s_aic * 0.001),
                format!("${:.2}", s_aic * 0.01),
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
        format!("${:.2} (~{total_saved_aic:.1} AIC)", total_saved_aic * 0.001),
        format!("${:.2}", total_saved_aic * 0.01),
    ]);
    lines.push(display.cards(
        "1. LLM MODEL USAGE, CACHE HIT & COST SAVINGS",
        &[
            "Model (Effort)",
            "Sess/Turns",
            "Input (Cache%)",
            "Out+Rsn Tok",
            "CCTX (Warm%)",
            "Src Adopt",
            "Used equivalent",
            "Est. savings",
            "Reference @$0.01/AIC",
        ],
        &model_rows,
    ));
    lines.push(String::new());

    let mut surface_rows = Vec::new();
    for (key, label) in [
        ("native", "Native (#cctxNative*)"),
        ("mcp", "Shared MCP (code_*)"),
        ("builtin_source", "Built-in Source (rg/view/glob)"),
        ("shell_source", "Shell Source (Bash/exec rg/sed/cat)"),
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
                "shell_source" => c.is_shell_lookup && !c.non_source_host_only,
                "builtin_pre_edit" => c.pre_edit_view,
                "host_only" => c.non_source_host_only,
                _ => classify_tool_group(&c.tool) == "other" && !c.is_shell_lookup,
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
                format!("${:.2} (~{g_saved_aic:.1} AIC)", g_saved_aic * 0.001)
            } else {
                "-".into()
            },
            if matches!(key, "native" | "mcp") {
                format!("${:.2}", g_saved_aic * 0.01)
            } else {
                "-".into()
            },
        ]);
    }
    lines.push(display.cards(
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
            "Est. savings",
            "Reference @$0.01/AIC",
        ],
        &surface_rows,
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
        session_vec.push((
            s_id.clone(),
            s_used_aic,
            s_turns.len(),
            s_lookup.len(),
            vec![
                short_id,
                ws,
                primary_model,
                s_turns.len().to_string(),
                format!("${:.2} ({s_used_aic:.1} AIC)", s_used_aic * 0.001),
                fmt_bar(s_warm_pct, 8),
                format!("{} ({}/{})", fmt_pct(s_adopt_pct), s_cctx.len(), s_lookup.len()),
                format!("${:.2} (~{s_saved_aic:.1} AIC)", s_saved_aic * 0.001),
                format!("${:.2}", s_saved_aic * 0.01),
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
    lines.push(display.cards(
        "3. TOP SESSIONS BY LLM SPEND, CACHE HIT & COST SAVINGS (Top 12)",
        &[
            "Session",
            "Workspace",
            "Primary Model",
            "Turns",
            "Used equivalent",
            "Warm Hit Rate",
            "Src Adopt (CCTX/Elig)",
            "Est. savings",
            "Reference @$0.01/AIC",
        ],
        &session_rows,
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
    lines.push(display.text(&format!(
        "\n  Savings model: trace-replayed ({effective_reread_mult:.1}x avg pre-compaction re-read · ~{} tok / ~{eff_turn_aic:.1} AIC per avoided warm turn; see `cctx usage --json`).",
        fmt_tok(eff_turn_tok.round() as u64)
    ), Style::Muted));
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
        let glued = format!("{{\"ok\":true}}{footer}");
        assert_eq!(extract_trace_total_ms(&json!({"content": glued}), 0), Some(1234.0));
        assert_eq!(
            super::extract_result_chars(&json!(glued), 0),
            ("{\"ok\":true}".len(), false, false)
        );
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
        let raw = super::usage_from_home(home.path(), 24.0, now, None, &[], true, false).unwrap();
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

        let text = super::usage_from_home(home.path(), 24.0, now, None, &[], false, true).unwrap();
        assert!(text.contains("ESTIMATED SAVINGS"));
        assert!(text.contains(&format!("${:.2} USD saved", ts["saved_usd_0001"].as_f64().unwrap())));
        assert!(text.contains("Without cctx (est.)"));
        assert!(text.contains("With cctx"));
        assert!(text.contains("not verified billing reductions"));
        assert!(text.contains("OPTIMIZATION ADVISORIES"));
        assert!(text.contains("REFERENCE ONLY"));
        let compact = super::usage_from_home(home.path(), 24.0, now, None, &[], false, false).unwrap();
        assert!(compact.contains("OPTIMIZATION ADVISORIES"));
        assert!(compact.contains("--details"));
        assert!(!compact.contains("TOOL SURFACE PERFORMANCE"));
        assert!(text.contains("TOOL SURFACE PERFORMANCE"));
        let detailed_json = super::usage_from_home(home.path(), 24.0, now, None, &[], true, true).unwrap();
        assert_eq!(parsed, serde_json::from_str::<serde_json::Value>(&detailed_json).unwrap());
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
        let raw = super::usage_from_home(home.path(), 1.0, now, None, &[], true, false).unwrap();
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

    fn make_raw_zstd_frame(payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() < 128 * 1024);
        let mut out = Vec::with_capacity(11 + payload.len());
        out.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD]); // Zstd magic
        out.push(0xA0); // FHD: FCS_Flag=2 (4B), Single_Segment=1, no checksum/dict
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        let block_hdr = ((payload.len() as u32) << 3) | 0x01; // Raw_Block + Last_Block
        out.extend_from_slice(&block_hdr.to_le_bytes()[..3]);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn codex_zstd_rollout_shell_bypass_claude_1h_thinking_and_lifecycle_advisories_work() {
        let home = tempfile::tempdir().unwrap();

        // 1. Codex `.jsonl.zst` rollout with cumulative token_count heartbeats, 5h/7d rate_limits,
        //    3x shell `exec_command` source lookups (`rg`/`sed -n`), 1 legitimate `cargo test` non-lookup,
        //    and 1 `/tmp` shell inspection (excluded as host-only).
        let codex_day = home.path().join(".codex/sessions/2026/11/18");
        std::fs::create_dir_all(&codex_day).unwrap();
        let codex_lines = [
            json!({
                "timestamp": "2026-11-18T06:00:00Z",
                "type": "session_meta",
                "payload": {"id": "codex-s1", "cwd": "/repo", "model": "gpt-6.1-sol"}
            }),
            json!({
                "timestamp": "2026-11-18T06:00:01Z",
                "type": "turn_context",
                "payload": {"cwd": "/repo", "model": "gpt-6.1-sol", "effort": "medium"}
            }),
            json!({
                "timestamp": "2026-11-18T06:00:02Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {"input_tokens": 100_000, "cached_input_tokens": 90_000, "cache_write_input_tokens": 5_000, "output_tokens": 800, "reasoning_output_tokens": 400, "total_tokens": 100_800},
                        "last_token_usage": {"input_tokens": 100_000, "cached_input_tokens": 90_000, "cache_write_input_tokens": 5_000, "output_tokens": 800, "reasoning_output_tokens": 400, "total_tokens": 100_800}
                    },
                    "rate_limits": {
                        "plan_type": "pro",
                        "primary": {"used_percent": 22.0, "window_minutes": 300, "resets_at": 1_795_000_000_i64},
                        "secondary": {"used_percent": 11.5, "window_minutes": 10080, "resets_at": 1_795_500_000_i64}
                    }
                }
            }),
            // Duplicate heartbeat with identical cumulative totals must NOT double-count tokens!
            json!({
                "timestamp": "2026-11-18T06:00:03Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {"input_tokens": 100_000, "cached_input_tokens": 90_000, "cache_write_input_tokens": 5_000, "output_tokens": 800, "reasoning_output_tokens": 400, "total_tokens": 100_800}
                    },
                    "rate_limits": {
                        "plan_type": "pro",
                        "primary": {"used_percent": 25.5, "window_minutes": 300, "resets_at": 1_795_000_000_i64},
                        "secondary": {"used_percent": 12.0, "window_minutes": 10080, "resets_at": 1_795_500_000_i64}
                    }
                }
            }),
            // Mid-session effort switch (`medium` -> `high`) dropping cache hit (<50%) -> config_bust!
            json!({
                "timestamp": "2026-11-18T06:01:00Z",
                "type": "turn_context",
                "payload": {"cwd": "/repo", "model": "gpt-6.1-sol", "effort": "high"}
            }),
            json!({
                "timestamp": "2026-11-18T06:01:05Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {"input_tokens": 180_000, "cached_input_tokens": 100_000, "cache_write_input_tokens": 75_000, "output_tokens": 1_300, "reasoning_output_tokens": 700, "total_tokens": 181_300}
                    }
                }
            }),
            // Premature compaction right before session ends (<5 follow-up turns, >=25k fresh write)
            json!({
                "timestamp": "2026-11-18T06:01:20Z",
                "type": "compacted",
                "payload": {"message": "summary"}
            }),
            json!({
                "timestamp": "2026-11-18T06:01:25Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {"input_tokens": 220_000, "cached_input_tokens": 105_000, "cache_write_input_tokens": 110_000, "output_tokens": 1_500, "reasoning_output_tokens": 800, "total_tokens": 221_500}
                    }
                }
            }),
            // 3 shell source lookup bypasses + 1 legitimate `cargo test` + 1 `/tmp` read
            json!({"timestamp": "2026-11-18T06:00:10Z", "type": "response_item", "payload": {"type": "function_call", "call_id": "sh1", "name": "exec_command", "arguments": "{\"cmd\":\"rg Planner src\"}"}}),
            json!({"timestamp": "2026-11-18T06:00:11Z", "type": "response_item", "payload": {"type": "function_call_output", "call_id": "sh1", "output": "src/planner.rs:10:pub struct Planner;"}}),
            json!({"timestamp": "2026-11-18T06:00:12Z", "type": "response_item", "payload": {"type": "function_call", "call_id": "sh2", "name": "exec_command", "arguments": "{\"cmd\":\"sed -n '1,80p' src/planner.rs\"}"}}),
            json!({"timestamp": "2026-11-18T06:00:13Z", "type": "response_item", "payload": {"type": "function_call_output", "call_id": "sh2", "output": "pub struct Planner {}"}}),
            json!({"timestamp": "2026-11-18T06:00:14Z", "type": "response_item", "payload": {"type": "function_call", "call_id": "sh3", "name": "exec_command", "arguments": "{\"cmd\":\"cat src/lib.rs | head -n 40\"}"}}),
            json!({"timestamp": "2026-11-18T06:00:15Z", "type": "response_item", "payload": {"type": "function_call_output", "call_id": "sh3", "output": "pub mod planner;"}}),
            json!({"timestamp": "2026-11-18T06:00:16Z", "type": "response_item", "payload": {"type": "function_call", "call_id": "sh_build", "name": "exec_command", "arguments": "{\"cmd\":\"cargo test --locked\"}"}}),
            json!({"timestamp": "2026-11-18T06:00:17Z", "type": "response_item", "payload": {"type": "function_call_output", "call_id": "sh_build", "output": "test result: ok"}}),
            json!({"timestamp": "2026-11-18T06:00:18Z", "type": "response_item", "payload": {"type": "function_call", "call_id": "sh_tmp", "name": "exec_command", "arguments": "{\"cmd\":\"cat /tmp/build.log\"}"}}),
            json!({"timestamp": "2026-11-18T06:00:19Z", "type": "response_item", "payload": {"type": "function_call_output", "call_id": "sh_tmp", "output": "log line"}}),
            json!({"timestamp": "2026-11-18T06:00:20Z", "type": "response_item", "payload": {"type": "function_call", "call_id": "mcp1", "name": "mcp__code-context__code_context", "arguments": "{\"root\":\"/repo\",\"pattern\":\"Planner\"}"}}),
            json!({"timestamp": "2026-11-18T06:00:21Z", "type": "response_item", "payload": {"type": "function_call_output", "call_id": "mcp1", "output": "matches=1"}}),
        ];
        let raw_jsonl = codex_lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            codex_day.join("rollout-codex-s1.jsonl.zst"),
            make_raw_zstd_frame(raw_jsonl.as_bytes()),
        )
        .unwrap();

        // 2. Claude Opus 4.7 session with `ephemeral_1h_input_tokens` (2.0x write) + `thinking` block
        //    + 2 short cold-cache subagents (`<=4` turns, `>=25k` fresh write) and >5m subagent TTL gap.
        let claude_dir = home.path().join(".claude/projects/repo");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let claude_events = [
            json!({
                "type": "assistant",
                "sessionId": "claude-s1",
                "cwd": "/repo",
                "timestamp": "2026-11-18T06:02:00Z",
                "message": {
                    "model": "claude-opus-4-7",
                    "usage": {
                        "input_tokens": 2_000,
                        "cache_creation_input_tokens": 50_000,
                        "cache_creation": {"ephemeral_1h_input_tokens": 50_000, "ephemeral_5m_input_tokens": 0},
                        "cache_read_input_tokens": 0,
                        "output_tokens": 1_000
                    },
                    "content": [
                        {"type": "thinking", "thinking": "Let us carefully analyze the module graph and cache prefix boundaries before editing."},
                        {"type": "text", "text": "Done."}
                    ]
                }
            }),
            // Short subagent #1 with >5m TTL gap on turn 2
            json!({
                "type": "assistant",
                "sessionId": "claude-s1",
                "agentId": "sub-1",
                "cwd": "/repo",
                "timestamp": "2026-11-18T06:02:10Z",
                "message": {
                    "model": "claude-sonnet-4-6",
                    "usage": {"input_tokens": 1_000, "cache_creation_input_tokens": 30_000, "cache_read_input_tokens": 0, "output_tokens": 300},
                    "content": []
                }
            }),
            json!({
                "type": "assistant",
                "sessionId": "claude-s1",
                "agentId": "sub-1",
                "cwd": "/repo",
                "timestamp": "2026-11-18T06:08:30Z",
                "message": {
                    "model": "claude-sonnet-4-6",
                    "usage": {"input_tokens": 1_000, "cache_creation_input_tokens": 55_000, "cache_read_input_tokens": 0, "output_tokens": 300},
                    "content": []
                }
            }),
            // Short subagent #2 + >30m main session gap
            json!({
                "type": "assistant",
                "sessionId": "claude-s1",
                "agentId": "sub-2",
                "cwd": "/repo",
                "timestamp": "2026-11-18T06:09:00Z",
                "message": {
                    "model": "claude-sonnet-4-6",
                    "usage": {"input_tokens": 1_000, "cache_creation_input_tokens": 32_000, "cache_read_input_tokens": 0, "output_tokens": 200},
                    "content": []
                }
            }),
            json!({
                "type": "assistant",
                "sessionId": "claude-s1",
                "cwd": "/repo",
                "timestamp": "2026-11-18T06:45:00Z",
                "message": {
                    "model": "claude-opus-4-7",
                    "usage": {
                        "input_tokens": 2_000,
                        "cache_creation_input_tokens": 60_000,
                        "cache_creation": {"ephemeral_1h_input_tokens": 60_000},
                        "cache_read_input_tokens": 0,
                        "output_tokens": 400
                    },
                    "content": []
                }
            }),
        ];
        std::fs::write(
            claude_dir.join("claude-s1.jsonl"),
            claude_events
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let now = super::event_unix_ts(&json!({"timestamp": "2026-11-18T07:00:00Z"})).unwrap();
        let (mtd_hours, mtd_label) = super::resolve_usage_window(24.0, Some("mtd"), now).unwrap();
        assert!(mtd_hours > 17.0 * 24.0 && mtd_hours < 18.0 * 24.0);
        assert!(mtd_label.as_deref().unwrap().contains("2026-11"));

        let raw = super::usage_from_home_with_label(
            home.path(),
            5.0,
            Some("Last 5h (rolling burst)"),
            now,
            None,
            &[],
            true,
            false,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();

        // Codex cumulative total was 220k input (not double-counted on heartbeat!)
        let harnesses = parsed["harness_usage"].as_array().unwrap();
        let codex_h = harnesses.iter().find(|h| h["harness"] == "codex").unwrap();
        assert_eq!(codex_h["turns"].as_u64(), Some(3));
        assert_eq!(codex_h["input_tokens"].as_u64(), Some(220_000));
        assert_eq!(codex_h["cache_read_tokens"].as_u64(), Some(105_000));

        // Codex live 5h & 7d quota gauges captured
        assert_eq!(parsed["codex_quota"]["primary_5h"]["used_percent"].as_f64(), Some(25.5));
        assert_eq!(parsed["codex_quota"]["primary_5h"]["window_delta_percent"].as_f64(), Some(3.5));
        assert_eq!(parsed["codex_quota"]["secondary_7d"]["used_percent"].as_f64(), Some(12.0));

        // Shell bypass calls: 3 eligible (`rg`, `sed -n`, `cat|head`), `cargo test` ignored, `/tmp` host-only!
        assert_eq!(parsed["token_summary"]["shell_lookup_bypass_calls"].as_u64(), Some(3));
        // 1 MCP call out of 4 eligible lookups (1 MCP + 3 shell) => 25.0% adoption!
        assert_eq!(parsed["token_summary"]["cctx_selection_percent"].as_f64(), Some(25.0));

        // Claude 1h cache write (2.0x = 1000e-6 on Opus) + thinking tokens > 0 + 3.0x Premium Request multiplier
        let models = parsed["model_usage"].as_array().unwrap();
        let opus = models.iter().find(|m| m["model"] == "claude-opus-4-7").unwrap();
        assert!(opus["reasoning_tokens"].as_u64().unwrap() > 10);
        assert_eq!(opus["premium_request_multiplier"].as_f64(), Some(3.0));
        // Turn 1: 2k*500e-6 (1.0) + 50k*1000e-6 (50.0, 2.0x 1h!) + 1k*2500e-6 (2.5) = 53.5 AIC;
        // Turn 2: 2k*500e-6 (1.0) + 60k*1000e-6 (60.0) + 0.4k*2500e-6 (1.0) = 62.0 AIC => Total 115.5 AIC!
        assert!((opus["used_aic"].as_f64().unwrap() - 115.5).abs() < 0.01);

        // Lifecycle advisories all triggered accurately
        let adv_codes: Vec<&str> = parsed["insights"]["advisories"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a["code"].as_str())
            .collect();
        for expected in [
            "shell_lookup_bypass",
            "mid_session_config_cache_bust",
            "ttl_cache_expiry",
            "premature_compaction",
            "subagent_cold_cache",
        ] {
            assert!(adv_codes.contains(&expected), "missing {expected} in {adv_codes:?}");
        }
        // Reasoning tokens must NEVER be double-counted on top of `output_tokens` in 3-axis load.
        assert_eq!(
            parsed["three_axis_load"]["output_and_reasoning_tokens"].as_u64(),
            parsed["token_summary"]["output_tokens"].as_u64()
        );
    }

    #[test]
    fn multi_frame_zstd_quota_reset_xargs_rm_and_relative_hidden_paths_are_hardened() {
        // 1. Shell classifier: `xargs rm` rejected; relative `.github/...` & `target/...` marked host-only!
        let xargs_rm = super::shell_lookup::classify_shell_command_str("find src -name '*.bak' | xargs rm -f", None);
        assert!(!xargs_rm.is_source_lookup);
        let xargs_rg = super::shell_lookup::classify_shell_command_str("find src -name '*.rs' | xargs rg fn", None);
        assert!(xargs_rg.is_source_lookup && !xargs_rg.non_source_host_only);
        let rel_gh = super::shell_lookup::classify_shell_command_str("cat .github/workflows/ci.yml", None);
        assert!(rel_gh.is_source_lookup && rel_gh.non_source_host_only);
        let rel_target = super::shell_lookup::classify_shell_command_str("rg foo target/debug/build", None);
        assert!(rel_target.is_source_lookup && rel_target.non_source_host_only);

        // 2. Multi-frame `.jsonl.zst` (2 concatenated Zstd frames!) + mid-session 5h reset (92% -> 15%)
        //    + self-hosting `"CACHE_MISS root="` inside source code lines must remain `success = true`!
        let home = tempfile::tempdir().unwrap();
        let codex_day = home.path().join(".codex/archived_sessions/2026/11/18");
        std::fs::create_dir_all(&codex_day).unwrap();
        let frame1 = format!(
            "{}\n{}\n",
            json!({"timestamp": "2026-11-18T06:10:00Z", "type": "session_meta", "payload": {"id": "codex-mf", "cwd": "/repo", "model": "gpt-5.6-sol"}}),
            json!({"timestamp": "2026-11-18T06:10:05Z", "type": "event_msg", "payload": {
                "type": "token_count",
                "info": {"total_token_usage": {"input_tokens": 40_000, "cached_input_tokens": 30_000, "cache_write_input_tokens": 10_000, "output_tokens": 500, "reasoning_output_tokens": 200, "total_tokens": 40_500}},
                "rate_limits": {"primary": {"used_percent": 92.0, "window_minutes": 300, "resets_at": 1_795_000_000_i64}}
            }})
        );
        let frame2 = format!(
            "{}\n{}\n{}\n",
            json!({"timestamp": "2026-11-18T06:15:00Z", "type": "event_msg", "payload": {
                "type": "token_count",
                "info": {"total_token_usage": {"input_tokens": 90_000, "cached_input_tokens": 75_000, "cache_write_input_tokens": 15_000, "output_tokens": 900, "reasoning_output_tokens": 400, "total_tokens": 90_900}},
                "rate_limits": {"primary": {"used_percent": 15.0, "window_minutes": 300, "resets_at": 1_795_018_000_i64}}
            }}),
            json!({"timestamp": "2026-11-18T06:15:01Z", "type": "response_item", "payload": {"type": "function_call", "call_id": "self1", "name": "mcp__code-context__code_read", "arguments": "{\"ranges\":[{\"path\":\"src/diagnostics.rs\",\"start\":1,\"end\":20}]}"}}),
            json!({"timestamp": "2026-11-18T06:15:02Z", "type": "response_item", "payload": {"type": "function_call_output", "call_id": "self1", "output": "[{\"lines\":[\"let m = s.contains(\\\"CACHE_MISS root=\\\");\"]}]"}})
        );
        let mut concat_zst = make_raw_zstd_frame(frame1.as_bytes());
        concat_zst.extend_from_slice(&make_raw_zstd_frame(frame2.as_bytes()));
        std::fs::write(codex_day.join("rollout-mf.jsonl.zst"), concat_zst).unwrap();

        let now = super::event_unix_ts(&json!({"timestamp": "2026-11-18T07:00:00Z"})).unwrap();
        let raw = super::usage_from_home(home.path(), 2.0, now, None, &[], true, false).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["token_summary"]["total_turns"].as_u64(), Some(2));
        assert_eq!(parsed["token_summary"]["input_tokens"].as_u64(), Some(90_000));
        // Across the 5h window reset (92% -> reset -> 15%), delta is +15.0%, not 0.0%!
        assert_eq!(parsed["codex_quota"]["primary_5h"]["window_delta_percent"].as_f64(), Some(15.0));
        // Self-hosting read containing literal `"CACHE_MISS root="` inside source code stayed a 100% warm hit!
        assert_eq!(parsed["token_summary"]["cctx_warm_cache_hit_percent"].as_f64(), Some(100.0));

        // 3. Round-2 hardening checks:
        //    - Multi-quoted `bash -c 'rg foo src' && rm -rf 'bar'` must NOT unwrap into a single quote!
        let split_quote = super::shell_lookup::classify_shell_command_str(
            "bash -c 'rg foo src' && rm -rf 'bar'",
            None,
        );
        assert!(!split_quote.is_source_lookup);
        //    - `rg -g '*.rs' -C 3 config.json` skips `-g`/`-C` values; single positional `config.json` is pattern, not path!
        let flag_val = super::shell_lookup::classify_shell_command_str(
            "rg -g '*.rs' -C 3 config.json",
            None,
        );
        assert!(flag_val.is_source_lookup && !flag_val.non_source_host_only);
        assert_eq!(flag_val.inferred_target, None);

        //    - Active in-progress session (<15m after compaction) must NOT trigger `premature_compaction`!
        let active_home = tempfile::tempdir().unwrap();
        let active_dir = active_home.path().join(".claude/projects/live");
        std::fs::create_dir_all(&active_dir).unwrap();
        let live_events = [
            json!({"type": "assistant", "sessionId": "live-1", "timestamp": "2026-11-18T06:50:00Z", "message": {"model": "claude-opus-4-7", "usage": {"input_tokens": 1000, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 150_000, "output_tokens": 400}}}),
            json!({"type": "assistant", "sessionId": "live-1", "timestamp": "2026-11-18T06:58:00Z", "message": {"model": "claude-opus-4-7", "usage": {"input_tokens": 1000, "cache_creation_input_tokens": 45_000, "cache_read_input_tokens": 0, "output_tokens": 400}}}),
        ];
        std::fs::write(
            active_dir.join("live-1.jsonl"),
            live_events.iter().map(serde_json::Value::to_string).collect::<Vec<_>>().join("\n"),
        )
        .unwrap();
        let raw_live = super::usage_from_home(active_home.path(), 1.0, now, None, &[], true, false).unwrap();
        let parsed_live: serde_json::Value = serde_json::from_str(&raw_live).unwrap();
        assert_eq!(parsed_live["cache_lifecycle"]["premature_compactions"].as_u64(), Some(0));
    }
}
