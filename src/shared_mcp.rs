use crate::daemon::{self, Request};
use crate::mcp::MAX_MESSAGE;
use crate::{Result, invalid};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Instant;

pub fn tools() -> Value {
    static TOOLS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    TOOLS.get_or_init(build_tools).clone()
}

fn build_tools() -> Value {
    let mut tools = crate::mcp::tools();
    let items = tools["tools"]
        .as_array_mut()
        .expect("static tool schema is an array");
    for tool in items.iter_mut() {
        tool["inputSchema"]["properties"]["root"] =
            json!({"type":"string","description":"Explicit absolute root."});
        if tool["name"] != "code_read" && tool["name"] != "code_files" {
            tool["inputSchema"]["properties"]["paths"] = json!({"type":"array","maxItems":64,"items":{"type":"string"},"description":"Relative path prefixes."});
            tool["inputSchema"]["properties"]["glob"] = json!({"type":"string"});
            tool["inputSchema"]["properties"]["max_matches_per_file"] = json!({"type":"integer","minimum":1,"maximum":64,"description":"Per-file cap."});
        }
        if tool["name"] == "code_read" {
            tool["inputSchema"]["properties"]["page"] = json!({"type":"boolean","default":false,
                "description":"Paginate >150L via next_ranges."});
            tool["inputSchema"]["properties"]["split_ranges"] =
                json!({"type":"boolean","default":true});
        }
        let prefix = match tool["name"].as_str() {
            Some("code_files") => {
                "Find workspace files by filename, path, or glob pattern (!content). Primary in AgentHost / Copilot SDK / background / subagent / CLI sessions (and fallback when cctxNativeFiles is unavailable)."
            }
            Some("code_grep") => {
                "Search workspace code lines for exact text or regex matches. Set regex=true for a|b. Primary in AgentHost / Copilot SDK / background / subagent / CLI sessions (and fallback when cctxNativeGrep is unavailable)."
            }
            Some("code_context") => {
                "Find symbol definition, references, callers, and surrounding function body in 1 call (!1..150 slicing). Set regex=true for a|b. Primary in AgentHost / Copilot SDK / background / subagent / CLI sessions (and fallback when cctxNativeContext is unavailable)."
            }
            _ => "Read file lines, batched 1-based line ranges, or markdown heading sections (<=150L, <8KiB). Primary in AgentHost / Copilot SDK / background / subagent / CLI sessions (and fallback when cctxNativeRead is unavailable).",
        };
        tool["description"] = json!(format!(
            "{prefix} Prefer this FIRST in AgentHost / Copilot SDK / background sessions so Reload Window or multi-window never stalls ('Running on another client... Skip?') or fails client dispatch; in foreground Editor Chat without backend MCP, use cctxNative* first. CACHE_MISS -> 1 host read; backend MCP tools avoid native extension-host dispatch when no UI client is connected. Do not duplicate a pending native call."
        ));
    }
    for (name, description) in [
        (
            "code_status",
            "Show index state, cache misses & memory without loading source.",
        ),
        (
            "code_sync",
            "Explicit full-source sync barrier for exhaustive absence claims only.",
        ),
    ] {
        items.push(json!({"name":name,"description":description,
            "inputSchema":{"type":"object","properties":{"root":{"type":"string","description":"Explicit absolute root."}},"additionalProperties":false},
            "annotations":{"readOnlyHint":name=="code_status","destructiveHint":false,"openWorldHint":false}}));
    }
    items.push(json!({"name":"code_workspaces","description":"List registered roots, scopes & globs.",
        "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}));
    tools
}

pub fn dispatch(request: Value) -> Option<Value> {
    let mut default_root = None;
    let mut client = None;
    let mut pending_traces = std::collections::VecDeque::new();
    dispatch_with_root(
        request,
        &mut default_root,
        &mut client,
        false,
        &mut pending_traces,
    )
}

fn compact_mcp_snippet_payload(_name: &str, text: String) -> String {
    if !text.contains("\"sha256\":") && !text.contains("\"notice\":") {
        return text;
    }
    let Ok(mut parsed) = serde_json::from_str::<Value>(&text) else {
        return text;
    };
    let mut modified = false;
    let snippets_opt = if parsed.is_array() {
        parsed.as_array_mut()
    } else {
        parsed.get_mut("snippets").and_then(Value::as_array_mut)
    };
    if let Some(snippets) = snippets_opt {
        for item in snippets.iter_mut() {
            let Some(obj) = item.as_object_mut() else {
                continue;
            };
            let full_sha = obj
                .get("sha256")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let short_sha = if full_sha.len() == 64 {
                let s = full_sha[..12].to_string();
                obj.insert("sha256".into(), json!(s));
                modified = true;
                s
            } else {
                full_sha
            };
            let Some(notice) = obj.get("notice").and_then(Value::as_str).map(str::to_owned) else {
                continue;
            };
            let start = obj.get("start").and_then(Value::as_u64).unwrap_or(1);
            let end = obj.get("end").and_then(Value::as_u64).unwrap_or(start);
            let exact_overlap_tag = format!("OVERLAP_READ: lines {start}..{end} already read");
            if let Some(lines) = obj.get_mut("lines").and_then(Value::as_array_mut) {
                if notice.contains(&exact_overlap_tag) && lines.len() >= 30 {
                    let omitted = lines.len() - 8;
                    let tail: Vec<Value> = lines.split_off(lines.len() - 4);
                    lines.truncate(4);
                    lines.push(json!(format!(
                        "... [{omitted} unchanged lines ({}..{}) omitted on exact duplicate read; sha256={short_sha} unchanged — reuse prior context] ...",
                        start + 4,
                        end - 4
                    )));
                    lines.extend(tail);
                    modified = true;
                }
                if notice.contains("SEQUENTIAL_SLICE")
                    && let Some(first_str) = lines.first().and_then(Value::as_str).map(str::to_owned)
                {
                    lines[0] = json!(format!("{first_str}  /* [CCTX_OUTLINE: {notice}] */"));
                    modified = true;
                }
            }
        }
    }
    if modified {
        serde_json::to_string(&parsed).unwrap_or(text)
    } else {
        text
    }
}

fn mcp_caller_id() -> u64 {
    static CALLER_ID: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CALLER_ID.get_or_init(|| {
        if let Some(inherited) = std::env::var("CCTX_MCP_CALLER_ID")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&id| id != 0)
        {
            return inherited;
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1_u64, |d| d.as_nanos() as u64);
        let pid = u64::from(std::process::id());
        let mixed = nanos.rotate_left(17) ^ (pid.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        (mixed & ((1_u64 << 53) - 1)).max(1)
    })
}

fn flush_pending_traces(
    client: &mut daemon::Client,
    pending_traces: &mut std::collections::VecDeque<Value>,
) {
    while let Some(trace) = pending_traces.pop_front() {
        if let Err(error) = client.call(&Request {
            root: None,
            name: "code_trace".into(),
            arguments: trace,
            compact: true,
        }) {
            eprintln!("CCTX_TRACE_REPORT_FAILED: {error}");
        }
    }
}

fn flush_pending_traces_opt(
    client: Option<&mut daemon::Client>,
    pending_traces: &mut std::collections::VecDeque<Value>,
) {
    match client {
        Some(client) => flush_pending_traces(client, pending_traces),
        None => pending_traces.clear(),
    }
}

fn clean_target_candidate(raw: &str) -> &str {
    let mut s = raw.trim();
    if s.len() >= 2
        && ((s.starts_with('`') && s.ends_with('`'))
            || (s.starts_with('"') && s.ends_with('"'))
            || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s = s[1..s.len() - 1].trim();
    }
    if s.starts_with('[')
        && let Some((_, rest)) = s.split_once("](")
        && let Some(target) = rest.strip_suffix(')')
    {
        s = target.trim();
        if s.starts_with('<') && s.ends_with('>') && s.len() >= 2 {
            s = s[1..s.len() - 1].trim();
        }
    }
    if let Some(rest) = s.strip_prefix("file://") {
        s = rest;
    } else if let Some(scheme_end) = s.find("://")
        && matches!(
            &s[..scheme_end],
            "vscode-remote"
                | "vscode-file"
                | "vscode-userdata"
                | "vscode-insiders"
                | "cursor"
                | "windsurf"
                | "vscodium"
                | "git"
        )
        && let Some(slash) = s[scheme_end + 3..].find('/')
    {
        s = &s[scheme_end + 3 + slash..];
    }
    if let Some((base, _)) = s.rsplit_once('#') {
        s = base;
    } else if let Some((base, suffix)) = s.split_once(':')
        && !suffix.is_empty()
        && suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'-' || b == b':')
    {
        s = base;
    }
    s
}

fn percent_decode_utf8(raw: &str) -> String {
    if !raw.contains('%') {
        return raw.to_owned();
    }
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            )
        {
            out.push(((hi << 4) | lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn decode_mcp_root_candidate(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut path_part = if let Some(rest) = trimmed.strip_prefix("file://localhost") {
        percent_decode_utf8(rest)
    } else if let Some(rest) = trimmed.strip_prefix("file:///") {
        format!("/{}", percent_decode_utf8(rest))
    } else if let Some(_rest) = trimmed.strip_prefix("file://") {
        #[cfg(windows)]
        {
            format!("\\\\{}", percent_decode_utf8(_rest).replace('/', "\\"))
        }
        #[cfg(not(windows))]
        {
            return None;
        }
    } else if let Some(scheme_end) = trimmed.find("://") {
        let scheme = &trimmed[..scheme_end];
        if matches!(
            scheme,
            "vscode-remote"
                | "vscode-agent-host"
                | "vscode-file"
                | "vscode-insiders"
                | "cursor"
                | "windsurf"
                | "vscodium"
        ) {
            let after_scheme = &trimmed[scheme_end + 3..];
            let slash = after_scheme.find('/')?;
            let with_query = &after_scheme[slash..];
            let without_query = with_query.split(['?', '#']).next().unwrap_or(with_query);
            percent_decode_utf8(without_query)
        } else {
            return None;
        }
    } else {
        trimmed.to_owned()
    };
    #[cfg(unix)]
    {
        let bytes = path_part.as_bytes();
        if bytes.len() >= 4
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b':'
            && (bytes[3] == b'/' || bytes[3] == b'\\')
        {
            let drive = (bytes[1] as char).to_ascii_lowercase();
            let mnt_drive = format!("/mnt/{drive}");
            if std::path::Path::new(&mnt_drive).is_dir() {
                let tail = path_part[3..].replace('\\', "/");
                path_part = format!("{mnt_drive}{tail}");
            }
        } else if bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'/' || bytes[2] == b'\\')
        {
            let drive = (bytes[0] as char).to_ascii_lowercase();
            let mnt_drive = format!("/mnt/{drive}");
            if std::path::Path::new(&mnt_drive).is_dir() {
                let tail = path_part[2..].replace('\\', "/");
                path_part = format!("{mnt_drive}{tail}");
            }
        }
    }
    #[cfg(windows)]
    {
        let bytes = path_part.as_bytes();
        if bytes.len() >= 4
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b':'
            && (bytes[3] == b'/' || bytes[3] == b'\\')
        {
            path_part.remove(0);
        }
    }
    let candidate = PathBuf::from(&path_part);
    if !candidate.is_absolute() {
        return None;
    }
    Some(
        crate::catalog::canonicalize(&candidate)
            .unwrap_or_else(|_| clean_lexical_absolute_path(&candidate)),
    )
}

fn find_git_root_upwards(path: &std::path::Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut cur = Some(path);
    for _ in 0..8 {
        let Some(dir) = cur else {
            break;
        };
        if dir.parent().is_none() || home.as_deref() == Some(dir) {
            break;
        }
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

fn normalize_workspace_root_candidate(
    path: &std::path::Path,
    specs: &[crate::catalog::Spec],
) -> Option<PathBuf> {
    let matched_spec = specs
        .iter()
        .filter(|spec| path.starts_with(&spec.root))
        .max_by_key(|spec| spec.root.components().count());
    let git_root = find_git_root_upwards(path);
    if let Some(spec) = matched_spec {
        if let Some(ref git) = git_root
            && git.starts_with(&spec.root)
            && git != &spec.root
            && spec.allow_worktrees
        {
            return Some(git.clone());
        }
        return Some(spec.root.clone());
    }
    git_root
}

fn infer_root_from_arguments(
    arguments: &serde_json::Map<String, Value>,
    default_root: Option<&PathBuf>,
) -> Option<PathBuf> {
    let mut raw_candidates: Vec<&str> = Vec::new();
    if let Some(paths) = arguments.get("paths").and_then(Value::as_array) {
        raw_candidates.extend(paths.iter().filter_map(Value::as_str));
    }
    if let Some(ranges) = arguments.get("ranges").and_then(Value::as_array) {
        raw_candidates.extend(
            ranges
                .iter()
                .filter_map(|r| r.get("path").and_then(Value::as_str)),
        );
    }
    if let Some(glob_str) = arguments.get("glob").and_then(Value::as_str) {
        let trimmed = glob_str.trim();
        if !trimmed.is_empty() {
            if let Some(wild_pos) = trimmed.find(['*', '?', '[', '{']) {
                if let Some(slash) = trimmed[..wild_pos].rfind('/')
                    && slash > 0
                {
                    let prefix = &trimmed[..slash];
                    if prefix != "." {
                        raw_candidates.push(prefix);
                    }
                }
            } else {
                raw_candidates.push(trimmed);
            }
        }
    }
    if raw_candidates.is_empty() {
        return None;
    }
    let specs = crate::catalog::load().ok()?;
    for raw in &raw_candidates {
        let cleaned = clean_target_candidate(raw);
        if cleaned.starts_with('/') {
            let target = std::path::Path::new(cleaned);
            let matched_spec_root = specs
                .iter()
                .filter(|spec| target.starts_with(&spec.root))
                .max_by_key(|spec| spec.root.components().count())
                .map(|spec| spec.root.clone());
            if let Some(def) = default_root
                && let Some(norm_def) = normalize_workspace_root_candidate(def, &specs)
                && target.starts_with(&norm_def)
                && norm_def.join(".git").exists()
                && matched_spec_root
                    .as_ref()
                    .is_none_or(|sr| norm_def.components().count() > sr.components().count())
            {
                return Some(norm_def);
            }
            if let Some(root) = matched_spec_root {
                return Some(root);
            }
        }
    }
    if let Some(def) = default_root
        && let Some(norm_def) = normalize_workspace_root_candidate(def, &specs)
    {
        return Some(norm_def);
    }
    if !specs.is_empty() {
        for raw in &raw_candidates {
            let mut cleaned = clean_target_candidate(raw).trim_start_matches("./");
            if let Some(rest) = cleaned
                .strip_prefix("a/")
                .or_else(|| cleaned.strip_prefix("b/"))
                && !rest.is_empty()
            {
                cleaned = rest;
            }
            if cleaned.is_empty() || cleaned.starts_with('/') || cleaned.starts_with("..") {
                continue;
            }
            if let Some((first_seg, rest_rel)) = cleaned.split_once('/')
                && !first_seg.is_empty()
                && !rest_rel.is_empty()
            {
                let by_basename: Vec<_> = specs
                    .iter()
                    .filter(|spec| {
                        spec.root.file_name().and_then(|n| n.to_str()) == Some(first_seg)
                            && spec.root.join(rest_rel).exists()
                    })
                    .collect();
                if by_basename.len() == 1 {
                    return Some(by_basename[0].root.clone());
                }
            }
            let matching: Vec<_> = specs
                .iter()
                .filter(|spec| spec.root.join(cleaned).exists())
                .collect();
            if matching.len() == 1 {
                return Some(matching[0].root.clone());
            }
        }
    }
    None
}

fn take_first_alias(
    arguments: &mut serde_json::Map<String, Value>,
    aliases: &[&str],
) -> Option<Value> {
    let mut chosen = None;
    for &alias in aliases {
        if let Some(val) = arguments.remove(alias)
            && chosen.is_none()
            && !val.is_null()
        {
            chosen = Some(val);
        }
    }
    chosen
}

fn parse_diagnostic_or_inline_range(raw: &str) -> (String, Option<u64>, Option<u64>) {
    let mut trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("-->") {
        trimmed = rest.trim();
    }
    if (trimmed.starts_with("https://github.com/") || trimmed.starts_with("https://gitlab.com/"))
        && let Some((_, after_blob)) = trimmed.split_once("/blob/")
        && let Some((_, repo_rel)) = after_blob.split_once('/')
        && !repo_rel.is_empty()
    {
        trimmed = repo_rel;
    }
    if let Some(rest) = trimmed.strip_prefix("File \"")
        && let Some((file_part, after_quote)) = rest.split_once('"')
    {
        let mut parsed_line = None;
        if let Some((_, line_part)) = after_quote.split_once("line ") {
            let digits: String = line_part
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = digits.parse::<u64>()
                && n > 0
            {
                parsed_line = Some(n);
            }
        }
        return (
            file_part.to_owned(),
            parsed_line,
            parsed_line.map(|n| n + 40),
        );
    }
    if let Some((base, frag)) = trimmed.rsplit_once('#')
        && !base.is_empty()
        && let Some(line_spec) = frag.strip_prefix('L').or_else(|| {
            frag.as_bytes()
                .first()
                .is_some_and(u8::is_ascii_digit)
                .then_some(frag)
        })
    {
        let (start_part, end_part) = line_spec.split_once('-').unwrap_or((line_spec, ""));
        let start_digits: String = start_part
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(s) = start_digits.parse::<u64>()
            && s > 0
        {
            let end_clean = end_part.strip_prefix('L').unwrap_or(end_part);
            let end_digits: String = end_clean
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            let e = end_digits.parse::<u64>().ok().filter(|&n| n >= s).unwrap_or(s + 40);
            return (base.to_owned(), Some(s), Some(e));
        }
    }
    if let Some((base, suffix)) = trimmed.split_once(':')
        && base.len() > 1
        && !base.contains("://")
        && suffix.as_bytes().first().is_some_and(u8::is_ascii_digit)
    {
        let numeric_prefix = suffix.split_whitespace().next().unwrap_or(suffix);
        let (first_seg, rest_dash) = numeric_prefix.split_once('-').unwrap_or((numeric_prefix, ""));
        let start_str = first_seg.split(':').next().unwrap_or("");
        if let Ok(s) = start_str.parse::<u64>()
            && s > 0
        {
            let end_str = rest_dash.split(':').next().unwrap_or("");
            let e = end_str.parse::<u64>().ok().filter(|&n| n >= s).unwrap_or(s + 40);
            return (base.to_owned(), Some(s), Some(e));
        }
    }
    (trimmed.to_owned(), None, None)
}

fn value_as_i64_coerced(val: &Value) -> Option<i64> {
    val.as_i64()
        .or_else(|| val.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
}

fn value_as_u64_coerced(val: &Value) -> Option<u64> {
    val.as_u64()
        .or_else(|| val.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
}

fn value_as_bool_coerced(val: &Value) -> Option<bool> {
    val.as_bool().or_else(|| match val.as_str()?.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Some(true),
        "false" | "0" | "no" => Some(false),
        _ => None,
    })
}

fn clean_lexical_absolute_path(path: &std::path::Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn sanitize_object_keys(map: &mut serde_json::Map<String, Value>) {
    let dirty_keys: Vec<String> = map
        .keys()
        .filter(|k| {
            let trimmed = k.trim_matches(|c: char| c.is_whitespace() || c == '"' || c == '\'');
            !trimmed.is_empty() && trimmed != k.as_str()
        })
        .cloned()
        .collect();
    for dirty in dirty_keys {
        if let Some(val) = map.remove(&dirty) {
            let clean = dirty
                .trim_matches(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                .to_owned();
            if !map.contains_key(&clean) {
                map.insert(clean, val);
            }
        }
    }
}

fn normalize_single_range_map(
    map: &mut serde_json::Map<String, Value>,
    default_span: u64,
) {
    sanitize_object_keys(map);
    if !map.contains_key("path")
        && let Some(path) = take_first_alias(
            map,
            &[
                "filePath",
                "file_path",
                "filepath",
                "relative_path",
                "relativePath",
                "target_file",
                "targetFile",
                "uri",
                "file",
                "target",
            ],
        )
    {
        map.insert("path".into(), path);
    }
    let mut extracted_start = None;
    let mut extracted_end = None;
    if let Some(raw_path) = map.get("path").and_then(Value::as_str) {
        let (clean_path, es, ee) = parse_diagnostic_or_inline_range(raw_path);
        extracted_start = es;
        extracted_end = ee;
        if es.is_some() && clean_path != raw_path {
            map.insert("path".into(), Value::String(clean_path));
        }
    }
    let explicit_start = map.remove("start").or_else(|| {
        take_first_alias(
            map,
            &[
                "start_line",
                "startLine",
                "start_line_number",
                "startLineNumber",
                "from_line",
                "fromLine",
                "offset",
            ],
        )
    });
    let line_alias = take_first_alias(map, &["line", "line_number", "lineNumber"]);
    let center_alias = take_first_alias(
        map,
        &["around_line", "aroundLine", "center_line", "centerLine"],
    );
    let radius_alias = take_first_alias(
        map,
        &["radius", "context_lines", "contextLines", "context"],
    )
    .and_then(|v| value_as_u64_coerced(&v));
    let mut end = map.remove("end").or_else(|| {
        take_first_alias(
            map,
            &[
                "end_line",
                "endLine",
                "end_line_number",
                "endLineNumber",
                "to_line",
                "toLine",
            ],
        )
    });
    let count_span = take_first_alias(map, &["numLines", "lineCount", "limit"])
        .and_then(|v| value_as_u64_coerced(&v))
        .filter(|&n| n > 0);
    let mut start = explicit_start;
    if let Some(Value::Array(vr)) = take_first_alias(map, &["view_range", "line_range", "lineRange", "lines"])
        && vr.len() == 2
    {
        start = start.or_else(|| vr.first().cloned());
        end = end.or_else(|| vr.get(1).cloned());
    }
    if start.is_none()
        && end.is_none()
        && let Some(center) = center_alias
            .as_ref()
            .or(line_alias.as_ref())
            .and_then(value_as_i64_coerced)
            .filter(|&n| n > 0)
            .map(|n| n as u64)
        && (center_alias.is_some() || radius_alias.is_some())
    {
        let r = radius_alias.unwrap_or(20);
        let s = center.saturating_sub(r).max(1);
        let e = center.saturating_add(r);
        map.insert("start".into(), json!(s));
        map.insert("end".into(), json!(e));
        return;
    }
    start = start.or(line_alias).or(center_alias);
    let raw_s = start.as_ref().and_then(value_as_i64_coerced);
    let raw_e = end.as_ref().and_then(value_as_i64_coerced);
    let mut s = match raw_s {
        Some(n) if n > 0 => n as u64,
        Some(0) => 1,
        _ => extracted_start.unwrap_or(1),
    };
    let mut e = match raw_e {
        Some(-1) => s + 499,
        Some(n) if n > 0 => n as u64,
        _ => count_span
            .map(|c| s + c - 1)
            .or(extracted_end)
            .unwrap_or(s + default_span),
    };
    if s == e + 1 && e >= 20 {
        e = s + default_span;
    } else if s > e && e >= 1 {
        std::mem::swap(&mut s, &mut e);
    }
    map.insert("start".into(), json!(s));
    map.insert("end".into(), json!(e));
}

pub(crate) fn normalize_mcp_argument_aliases(
    name: &str,
    arguments: &mut serde_json::Map<String, Value>,
) {
    sanitize_object_keys(arguments);
    // Strip null or empty-string optional parameters commonly emitted by LLMs
    for optional_key in [
        "root",
        "glob",
        "paths",
        "max_matches_per_file",
        "regex",
        "ignore_case",
        "limit",
        "radius",
        "page",
        "split_ranges",
    ] {
        if let Some(val) = arguments.get(optional_key)
            && (val.is_null()
                || (matches!(optional_key, "root" | "glob")
                    && val.as_str().is_some_and(|s| s.trim().is_empty())))
        {
            arguments.remove(optional_key);
        }
    }
    if !arguments.contains_key("root")
        && let Some(root_alias) = take_first_alias(
            arguments,
            &[
                "workspaceRoot",
                "workspace_root",
                "cwd",
                "projectRoot",
                "project_root",
                "repoRoot",
                "repo_root",
            ],
        )
        && root_alias.as_str().is_some_and(|s| !s.trim().is_empty())
    {
        arguments.insert("root".into(), root_alias);
    } else {
        for k in [
            "workspaceRoot",
            "workspace_root",
            "cwd",
            "projectRoot",
            "project_root",
            "repoRoot",
            "repo_root",
        ] {
            arguments.remove(k);
        }
    }
    if matches!(name, "code_grep" | "code_context") {
        let alias_val = take_first_alias(
            arguments,
            &[
                "query",
                "symbol",
                "symbol_name",
                "symbolName",
                "search",
                "search_query",
                "searchQuery",
                "regex_pattern",
                "regexPattern",
                "text",
                "needle",
            ],
        );
        if !arguments.contains_key("pattern")
            && let Some(val) = alias_val
        {
            arguments.insert("pattern".into(), val);
        }
        let regex_alias = take_first_alias(arguments, &["isRegexp", "is_regex", "useRegex", "regexp"]);
        let fixed_alias = take_first_alias(arguments, &["fixed_strings", "fixedStrings", "literal"]);
        if !arguments.contains_key("regex") {
            if let Some(val) = regex_alias {
                arguments.insert("regex".into(), val);
            } else if let Some(b) = fixed_alias.as_ref().and_then(value_as_bool_coerced) {
                arguments.insert("regex".into(), Value::Bool(!b));
            }
        }
        if let Some(b) = arguments.get("regex").and_then(value_as_bool_coerced) {
            arguments.insert("regex".into(), Value::Bool(b));
        }
        let ci_alias = take_first_alias(arguments, &["caseInsensitive", "case_insensitive", "-i"]);
        let cs_alias = arguments.remove("isCaseSensitive");
        if !arguments.contains_key("ignore_case") {
            if let Some(val) = ci_alias {
                arguments.insert("ignore_case".into(), val);
            } else if let Some(cs) = cs_alias.as_ref().and_then(value_as_bool_coerced) {
                arguments.insert("ignore_case".into(), Value::Bool(!cs));
            }
        }
        if let Some(b) = arguments.get("ignore_case").and_then(value_as_bool_coerced) {
            arguments.insert("ignore_case".into(), Value::Bool(b));
        }
        let max_per_file = arguments.remove("maxMatchesPerFile");
        if !arguments.contains_key("max_matches_per_file")
            && let Some(val) = max_per_file
            && !val.is_null()
        {
            arguments.insert("max_matches_per_file".into(), val);
        }
        if let Some(n) = arguments.get("max_matches_per_file").and_then(value_as_i64_coerced) {
            arguments.insert("max_matches_per_file".into(), json!(n.clamp(1, 64)));
        }
    }
    if name == "code_context" {
        let radius_alias = take_first_alias(
            arguments,
            &[
                "contextLines",
                "context_lines",
                "context",
                "before_context",
                "after_context",
                "-C",
                "-A",
                "-B",
            ],
        );
        if !arguments.contains_key("radius")
            && let Some(val) = radius_alias
        {
            arguments.insert("radius".into(), val);
        }
        if let Some(r) = arguments.get("radius").and_then(value_as_i64_coerced) {
            arguments.insert("radius".into(), json!(r.clamp(0, 100)));
        }
    }
    if matches!(name, "code_files" | "code_grep" | "code_context") {
        let limit_alias =
            take_first_alias(arguments, &["maxResults", "max_results", "head_limit"]);
        if !arguments.contains_key("limit")
            && let Some(val) = limit_alias
        {
            arguments.insert("limit".into(), val);
        }
        if let Some(lim) = arguments.get("limit").and_then(value_as_i64_coerced)
            && lim > 0
        {
            let max_lim = if name == "code_context" { 64 } else { 1000 };
            arguments.insert("limit".into(), json!(lim.clamp(1, max_lim)));
        }
        let glob_alias = take_first_alias(
            arguments,
            &[
                "includePattern",
                "include_pattern",
                "filePattern",
                "file_pattern",
                "glob_pattern",
                "globPattern",
                "file_glob",
                "fileGlob",
                "include_glob",
                "includeGlob",
                "include",
                "filter",
                "globs",
                "patterns",
            ],
        );
        if !arguments.contains_key("glob")
            && let Some(val) = glob_alias
            && (val.as_str().is_some_and(|s| !s.trim().is_empty()) || val.is_array())
        {
            arguments.insert("glob".into(), val);
        }
        if let Some(Value::Array(glob_arr)) = arguments.get("glob").cloned() {
            let parts: Vec<String> = glob_arr
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            if parts.len() == 1 {
                arguments.insert("glob".into(), Value::String(parts[0].clone()));
            } else if parts.len() > 1 {
                arguments.insert("glob".into(), Value::String(format!("{{{}}}", parts.join(","))));
            } else {
                arguments.remove("glob");
            }
        }
    }
    if name == "code_files"
        && !arguments.contains_key("glob")
        && let Some(pattern) = take_first_alias(arguments, &["pattern", "query", "search"])
    {
        arguments.insert("glob".into(), pattern);
    }
    if matches!(name, "code_files" | "code_grep" | "code_context")
        && let Some(raw_glob) = arguments.get("glob").and_then(Value::as_str).map(str::to_owned)
    {
        let mut cleaned_glob = raw_glob.trim().to_owned();
        while cleaned_glob.starts_with("./") && cleaned_glob.len() > 2 {
            cleaned_glob = cleaned_glob[2..].to_owned();
        }
        if !cleaned_glob.contains('{') && cleaned_glob.contains(',') {
            let parts: Vec<&str> = cleaned_glob
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if parts.len() >= 2
                && parts
                    .iter()
                    .all(|p| p.starts_with("*.") || p.starts_with("**/*."))
            {
                let exts: Vec<&str> = parts
                    .iter()
                    .map(|p| p.trim_start_matches("**/").trim_start_matches("*."))
                    .collect();
                cleaned_glob = format!("*.{{{}}}", exts.join(","));
            }
        }
        if cleaned_glob.starts_with('.')
            && cleaned_glob.len() > 1
            && cleaned_glob[1..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            cleaned_glob = format!("*{cleaned_glob}");
        }
        if cleaned_glob != raw_glob {
            arguments.insert("glob".into(), Value::String(cleaned_glob));
        }
    }
    if matches!(name, "code_files" | "code_grep" | "code_context") {
        let single_path = take_first_alias(
            arguments,
            &[
                "path",
                "file_path",
                "filePath",
                "filepath",
                "relative_path",
                "relativePath",
                "target_file",
                "targetFile",
                "dir",
                "directory",
                "folder",
            ],
        );
        if !arguments.contains_key("paths")
            && let Some(single_path) = single_path
        {
            if single_path.is_string() {
                arguments.insert("paths".into(), Value::Array(vec![single_path]));
            } else if single_path.is_array() {
                arguments.insert("paths".into(), single_path);
            }
        }
    }
    if matches!(name, "code_files" | "code_grep" | "code_context")
        && let Some(Value::Array(paths_arr)) = arguments.get_mut("paths")
    {
        let mut cleaned_paths = Vec::with_capacity(paths_arr.len());
        let mut extracted_glob: Option<String> = None;
        for item in paths_arr.drain(..) {
            if let Some(raw_str) = item.as_str() {
                let trimmed = raw_str.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed.contains(['*', '?', '[', '{']) {
                    if let Some(wild_pos) = trimmed.find(['*', '?', '[', '{'])
                        && let Some(slash) = trimmed[..wild_pos].rfind('/')
                        && slash > 0
                    {
                        let prefix = &trimmed[..slash];
                        if prefix != "." {
                            cleaned_paths.push(Value::String(prefix.to_owned()));
                        }
                    }
                    if extracted_glob.is_none() {
                        extracted_glob = Some(trimmed.to_owned());
                    }
                } else if trimmed.contains(',') && !trimmed.contains('{') {
                    for part in trimmed.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                        cleaned_paths.push(Value::String(part.to_owned()));
                    }
                } else {
                    cleaned_paths.push(Value::String(trimmed.to_owned()));
                }
            } else {
                cleaned_paths.push(item);
            }
        }
        if cleaned_paths.len() > 1 {
            let non_hidden: Vec<Value> = cleaned_paths
                .iter()
                .filter(|v| {
                    v.as_str().is_none_or(|p| {
                        !std::path::Path::new(p)
                            .components()
                            .any(|c| c.as_os_str().to_str().is_some_and(|s| s.starts_with('.') && s != "."))
                    })
                })
                .cloned()
                .collect();
            if !non_hidden.is_empty() && non_hidden.len() < cleaned_paths.len() {
                cleaned_paths = non_hidden;
            }
        }
        *paths_arr = cleaned_paths;
        if !arguments.contains_key("glob")
            && let Some(g) = extracted_glob
        {
            arguments.insert("glob".into(), Value::String(g));
        }
    }
    if name == "code_read" {
        if let Some(split_alias) = arguments.remove("splitRanges")
            && !arguments.contains_key("split_ranges")
            && !split_alias.is_null()
        {
            arguments.insert("split_ranges".into(), split_alias);
        }
        if let Some(b) = arguments.get("split_ranges").and_then(value_as_bool_coerced) {
            arguments.insert("split_ranges".into(), Value::Bool(b));
        }
        if let Some(b) = arguments.get("page").and_then(value_as_bool_coerced) {
            arguments.insert("page".into(), Value::Bool(b));
        }
        if !arguments.contains_key("paths")
            && let Some(paths_alias) =
                take_first_alias(arguments, &["files", "targets", "file_paths", "filePaths"])
        {
            arguments.insert("paths".into(), paths_alias);
        }
        if let Some(Value::Array(ranges)) = arguments.get_mut("ranges") {
            let default_span = if ranges.len() > 1 { 79 } else { 119 };
            for range in ranges.iter_mut() {
                if let Some(map) = range.as_object_mut() {
                    normalize_single_range_map(map, default_span);
                }
            }
        } else if let Some(Value::Array(paths)) = arguments.remove("paths") {
            let default_span = if paths.len() > 1 { 79 } else { 119 };
            let mut template = serde_json::Map::new();
            for key in [
                "start",
                "end",
                "start_line",
                "end_line",
                "startLine",
                "endLine",
                "start_line_number",
                "end_line_number",
                "startLineNumber",
                "endLineNumber",
                "from_line",
                "to_line",
                "fromLine",
                "toLine",
                "line",
                "line_number",
                "lineNumber",
                "around_line",
                "aroundLine",
                "center_line",
                "centerLine",
                "radius",
                "context_lines",
                "contextLines",
                "context",
                "offset",
                "numLines",
                "lineCount",
                "limit",
                "view_range",
            ] {
                if let Some(v) = arguments.remove(key) {
                    template.insert(key.into(), v);
                }
            }
            let built: Vec<Value> = paths
                .into_iter()
                .filter_map(|p| p.as_str().map(str::to_owned))
                .filter(|p| !p.trim().is_empty())
                .map(|path| {
                    let mut m = template.clone();
                    m.insert("path".into(), Value::String(path));
                    normalize_single_range_map(&mut m, default_span);
                    Value::Object(m)
                })
                .collect();
            arguments.insert("ranges".into(), Value::Array(built));
        } else if let Some(path) = take_first_alias(
            arguments,
            &[
                "path",
                "filePath",
                "file_path",
                "filepath",
                "relative_path",
                "relativePath",
                "target_file",
                "targetFile",
                "uri",
                "file",
                "target",
            ],
        ) && path.is_string()
        {
            let mut single = serde_json::Map::new();
            single.insert("path".into(), path);
            for key in [
                "start",
                "end",
                "start_line",
                "end_line",
                "startLine",
                "endLine",
                "start_line_number",
                "end_line_number",
                "startLineNumber",
                "endLineNumber",
                "from_line",
                "to_line",
                "fromLine",
                "toLine",
                "line",
                "line_number",
                "lineNumber",
                "around_line",
                "aroundLine",
                "center_line",
                "centerLine",
                "radius",
                "context_lines",
                "contextLines",
                "context",
                "offset",
                "numLines",
                "lineCount",
                "limit",
                "view_range",
            ] {
                if let Some(v) = arguments.remove(key) {
                    single.insert(key.into(), v);
                }
            }
            normalize_single_range_map(&mut single, 119);
            arguments.insert("ranges".into(), Value::Array(vec![Value::Object(single)]));
        } else if let Some(raw_root) = arguments.get("root").and_then(Value::as_str)
            && let cleaned_root = clean_target_candidate(raw_root)
            && cleaned_root.starts_with('/')
            && let Ok(specs) = crate::catalog::load()
        {
            let target = std::path::Path::new(cleaned_root);
            if let Some(spec) = specs
                .iter()
                .filter(|spec| target.starts_with(&spec.root) && target != spec.root)
                .max_by_key(|spec| spec.root.components().count())
                && let Ok(rel) = target.strip_prefix(&spec.root)
                && let Some(rel_str) = rel.to_str()
                && !rel_str.is_empty()
                && (target.is_file()
                    || raw_root.contains('#')
                    || raw_root.contains(':')
                    || arguments.keys().any(|k| {
                        matches!(
                            k.as_str(),
                            "start"
                                | "end"
                                | "start_line"
                                | "end_line"
                                | "startLine"
                                | "endLine"
                                | "line"
                                | "around_line"
                                | "view_range"
                        )
                    }))
            {
                let mut single = serde_json::Map::new();
                let rebuilt_path = if let Some((_, frag)) = raw_root.rsplit_once('#') {
                    format!("{rel_str}#{frag}")
                } else if let Some((_, suffix)) = raw_root.split_once(':')
                    && suffix.as_bytes().first().is_some_and(u8::is_ascii_digit)
                {
                    format!("{rel_str}:{suffix}")
                } else {
                    rel_str.to_owned()
                };
                single.insert("path".into(), Value::String(rebuilt_path));
                for key in [
                    "start",
                    "end",
                    "start_line",
                    "end_line",
                    "startLine",
                    "endLine",
                    "start_line_number",
                    "end_line_number",
                    "startLineNumber",
                    "endLineNumber",
                    "from_line",
                    "to_line",
                    "fromLine",
                    "toLine",
                    "line",
                    "line_number",
                    "lineNumber",
                    "around_line",
                    "aroundLine",
                    "center_line",
                    "centerLine",
                    "radius",
                    "context_lines",
                    "contextLines",
                    "context",
                    "offset",
                    "numLines",
                    "lineCount",
                    "limit",
                    "view_range",
                ] {
                    if let Some(v) = arguments.remove(key) {
                        single.insert(key.into(), v);
                    }
                }
                normalize_single_range_map(&mut single, 119);
                arguments.insert(
                    "root".into(),
                    Value::String(spec.root.to_string_lossy().into_owned()),
                );
                arguments.insert("ranges".into(), Value::Array(vec![Value::Object(single)]));
            }
        }
        if let Some(Value::Array(ranges_arr)) = arguments.get_mut("ranges")
            && ranges_arr.len() > 1
        {
            let mut deduped: Vec<Value> = Vec::with_capacity(ranges_arr.len());
            for item in ranges_arr.drain(..) {
                let is_dup = deduped.iter().any(|existing| {
                    existing.get("path") == item.get("path")
                        && existing.get("start").and_then(Value::as_u64).is_some_and(|es| {
                            item.get("start").and_then(Value::as_u64).is_some_and(|s| es <= s)
                        })
                        && existing.get("end").and_then(Value::as_u64).is_some_and(|ee| {
                            item.get("end").and_then(Value::as_u64).is_some_and(|e| ee >= e)
                        })
                });
                if !is_dup {
                    deduped.push(item);
                }
            }
            deduped.truncate(64);
            *ranges_arr = deduped;
        }
    }
}

fn dispatch_with_root(
    request: Value,
    default_root: &mut Option<PathBuf>,
    client: &mut Option<daemon::Client>,
    allow_connect: bool,
    pending_traces: &mut std::collections::VecDeque<Value>,
) -> Option<Value> {
    if !request.is_object() || request["jsonrpc"] != "2.0" || !request["method"].is_string() {
        return Some(
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"invalid JSON-RPC request"}}),
        );
    }
    let id = request.get("id")?.clone();
    if !(id.is_null() || id.is_string() || id.is_i64() || id.is_u64()) {
        return Some(
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"request ID must be a string or integer"}}),
        );
    }
    let method = request["method"].as_str()?;
    let result = match method {
        "initialize" => {
            let params = &request["params"];
            let requested_version = params["protocolVersion"].as_str().unwrap_or("");
            let supported_versions = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
            let protocol_version = if supported_versions.contains(&requested_version) {
                requested_version
            } else {
                "2025-06-18"
            };
            let specs = crate::catalog::load().unwrap_or_default();
            let mut init_roots: Vec<PathBuf> = Vec::with_capacity(4);
            for key in ["workspaceFolders", "roots"] {
                if let Some(items) = params[key].as_array() {
                    for item in items {
                        if let Some(raw) = item["uri"]
                            .as_str()
                            .or_else(|| item["path"].as_str())
                            .or_else(|| item.as_str())
                            && let Some(decoded) = decode_mcp_root_candidate(raw)
                            && !init_roots.contains(&decoded)
                        {
                            init_roots.push(decoded);
                        }
                    }
                }
            }
            for key in ["rootUri", "rootPath"] {
                if let Some(raw) = params[key].as_str()
                    && let Some(decoded) = decode_mcp_root_candidate(raw)
                    && !init_roots.contains(&decoded)
                {
                    init_roots.push(decoded);
                }
            }
            if let Some(explicit_root) = init_roots
                .iter()
                .find_map(|cand| normalize_workspace_root_candidate(cand, &specs))
            {
                *default_root = Some(explicit_root);
            }
            let mut warm_candidates: Vec<PathBuf> = Vec::with_capacity(2);
            for cand in &init_roots {
                if let Some(norm) = normalize_workspace_root_candidate(cand, &specs)
                    && !warm_candidates.contains(&norm)
                {
                    warm_candidates.push(norm);
                    if warm_candidates.len() >= 2 {
                        break;
                    }
                }
            }
            if warm_candidates.len() < 2
                && let Some(def) = default_root.as_ref()
                && let Some(norm_def) = normalize_workspace_root_candidate(def, &specs)
                && !warm_candidates.contains(&norm_def)
            {
                warm_candidates.push(norm_def);
            }
            if !warm_candidates.is_empty() && allow_connect {
                let _ = std::thread::Builder::new()
                    .name("cctx-mcp-warm".into())
                    .stack_size(128 * 1024)
                    .spawn(move || {
                        for candidate in warm_candidates {
                            let _ = daemon::send_warm_hint_nonblocking(candidate);
                        }
                    });
            }
            let instruction_root = default_root
                .as_deref()
                .and_then(|def| normalize_workspace_root_candidate(def, &specs));
            Ok(
                json!({"protocolVersion":protocol_version,"capabilities":{"tools":{"listChanged":false}},
                "serverInfo":{"name":"code-context","version":env!("CARGO_PKG_VERSION")},
                "instructions": crate::prompt_formula::render(
                    crate::prompt_formula::PromptStyle::from_env().unwrap_or_default(),
                    crate::prompt_formula::PromptTarget::Mcp,
                    instruction_root.as_deref(),
                )}),
            )
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools()),
        "tools/call" => {
            let started = Instant::now();
            if client.is_none() && allow_connect {
                *client = daemon::Client::connect_or_start_traced().ok();
            }
            let mut trace = client.as_ref().filter(|c| c.supports_tracing()).map(|_| json!({
                "trace_id":daemon::new_trace_id(),"origin":"mcp",
                "started_unix_ms":daemon::trace_unix_ms(),"completed_unix_ms":null,
                "operation":request["params"]["name"],"root":request["params"]["arguments"]["root"],
                "queue_ms":0.0,"prepare_ms":0.0,"ipc_ms":0.0,"available":true,
                "serialize_ms":0.0,"write_ms":0.0,"total_ms":0.0,"success":false,
                "daemon":null,
            }));
            let outcome = tool_call(
                &request["params"],
                default_root.as_ref(),
                client,
                allow_connect,
                &mut trace,
                pending_traces,
            );
            if let Some(trace) = trace.as_mut() {
                trace["prepare_ms"] = json!(
                    (started.elapsed().as_secs_f64() * 1000.0
                        - trace["ipc_ms"].as_f64().expect("trace IPC time"))
                    .max(0.0)
                );
            }
            let formatting = Instant::now();
            let mut result = match outcome {
                Ok(text) => {
                    json!({"content":[{"type":"text","text":text}],"isError":false})
                }
                Err(error) => {
                    json!({"content":[{"type":"text","text":error.to_string()}],"isError":true})
                }
            };
            if let Some(mut trace) = trace {
                if let Some(first_text) = result["content"]
                    .get_mut(0)
                    .and_then(|item| item.get_mut("text"))
                {
                    let needs_nl = first_text
                        .as_str()
                        .is_some_and(|s| !s.is_empty() && !s.ends_with('\n'));
                    if needs_nl {
                        let mut owned = first_text.as_str().unwrap_or_default().to_owned();
                        owned.push('\n');
                        *first_text = Value::String(owned);
                    }
                }
                trace["serialize_ms"] = json!(formatting.elapsed().as_secs_f64() * 1000.0);
                trace["total_ms"] = json!(started.elapsed().as_secs_f64() * 1000.0);
                trace["completed_unix_ms"] = json!(daemon::trace_unix_ms());
                trace["success"] = json!(result["isError"] == false);
                let compact_footer = json!({
                    "trace_id": trace["trace_id"],
                    "operation": trace["operation"],
                    "success": trace["success"],
                    "total_ms": (trace["total_ms"].as_f64().unwrap_or(0.0) * 100.0).round() / 100.0,
                });
                result["content"]
                    .as_array_mut()
                    .expect("tool content")
                    .push(json!({
                        "type":"text","text":format!("CCTX_TRACE {compact_footer}")
                    }));
                result["_meta"] = json!({"cctx_trace":trace});
            }
            Ok(result)
        }
        _ => Err((-32601, "unknown method")),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err((code, message)) => {
            json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
        }
    })
}

fn tool_call(
    params: &Value,
    default_root: Option<&PathBuf>,
    client: &mut Option<daemon::Client>,
    allow_connect: bool,
    trace: &mut Option<Value>,
    pending_traces: &mut std::collections::VecDeque<Value>,
) -> Result<String> {
    let prepare_started = Instant::now();
    let name = params["name"]
        .as_str()
        .ok_or_else(|| invalid("tool name required"))?;
    if ![
        "code_files",
        "code_grep",
        "code_read",
        "code_context",
        "code_status",
        "code_sync",
        "code_workspaces",
    ]
    .contains(&name)
    {
        return Err(invalid("unknown tool").into());
    }
    let mut arguments = params["arguments"]
        .as_object()
        .ok_or_else(|| invalid("arguments must be an object"))?
        .clone();
    normalize_mcp_argument_aliases(name, &mut arguments);
    let root = arguments
        .remove("root")
        .map(serde_json::from_value::<PathBuf>)
        .transpose()?
        .map(|p| {
            if p.is_absolute() {
                clean_lexical_absolute_path(&p)
            } else {
                p
            }
        });
    if root.as_ref().is_some_and(|path| !path.is_absolute()) {
        return Err(invalid("root must be absolute when provided").into());
    }
    if name == "code_workspaces" && (!arguments.is_empty() || root.is_some()) {
        return Err(invalid("code_workspaces takes empty arguments").into());
    }
    let root = if name == "code_workspaces" {
        None
    } else {
        let mut resolved = match root {
            Some(explicit_root) => {
                if !explicit_root.exists()
                    && let Some(inferred) = infer_root_from_arguments(&arguments, None)
                {
                    inferred
                } else {
                    explicit_root
                }
            }
            None => infer_root_from_arguments(&arguments, default_root)
                .or_else(|| default_root.cloned())
                .ok_or_else(|| invalid("current directory is not inside a registered workspace"))?,
        };
        if std::fs::metadata(&resolved).is_ok_and(|m| m.is_file()) {
            let canonical_file =
                crate::catalog::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
            let matched_spec = crate::catalog::load()
                .ok()
                .and_then(|specs| {
                    specs
                        .into_iter()
                        .filter(|s| canonical_file.starts_with(&s.root))
                        .max_by_key(|s| s.root.components().count())
                });
            if let Some(spec) = matched_spec
                && let Ok(rel) = canonical_file.strip_prefix(&spec.root)
            {
                let rel_str = rel.to_string_lossy().into_owned();
                if name == "code_read" && !arguments.contains_key("ranges") {
                    arguments.insert(
                        "ranges".into(),
                        json!([{"path": rel_str, "start": 1, "end": 120}]),
                    );
                } else if matches!(name, "code_grep" | "code_context")
                    && !arguments.contains_key("paths")
                {
                    arguments.insert("paths".into(), json!([rel_str]));
                }
                resolved = spec.root;
            } else {
                return Err(invalid(format!(
                    "UNREGISTERED_FILE_ROOT root={}: root must be a registered workspace directory, not a standalone file outside registered workspaces. Use a permitted host read/view tool directly without retrying CCTX.",
                    resolved.display()
                ))
                .into());
            }
        } else {
            let lossy = resolved.to_string_lossy();
            let is_host_scratch = [
                "/.copilot/",
                "/.claude/",
                "/.vscode/",
                "/.vscode-server/",
            ]
            .iter()
            .any(|seg| lossy.contains(seg) || lossy.ends_with(seg.trim_end_matches('/')));
            if is_host_scratch {
                let is_registered = crate::catalog::load()
                    .ok()
                    .is_some_and(|specs| specs.iter().any(|s| resolved.starts_with(&s.root)));
                if !is_registered {
                    return Err(invalid(format!(
                        "UNREGISTERED_SCRATCH_ROOT root={}: agent session-state/instructions/config directories are outside registered source workspaces. Use permitted host view/grep/glob tools directly; do not retry with CCTX.",
                        resolved.display()
                    ))
                    .into());
                }
            }
        }
        Some(resolved)
    };
    if arguments.contains_key(daemon::TRACE_ARGUMENT)
        || arguments.contains_key(daemon::PREV_TRACE_ARGUMENT)
        || arguments.contains_key(daemon::CALLER_ARGUMENT)
    {
        return Err(invalid("reserved internal trace argument").into());
    }
    if !pending_traces.is_empty() {
        let items: Vec<Value> = pending_traces.drain(..).collect();
        arguments.insert(daemon::PREV_TRACE_ARGUMENT.into(), Value::Array(items));
    }
    if client.as_ref().is_some_and(|c| c.supports_caller_session())
        && matches!(name, "code_files" | "code_grep" | "code_read" | "code_context")
    {
        arguments.insert(daemon::CALLER_ARGUMENT.into(), json!(mcp_caller_id()));
    }
    if let Some(trace) = trace.as_mut() {
        trace["root"] = json!(root);
        arguments.insert(daemon::TRACE_ARGUMENT.into(), trace["trace_id"].clone());
        trace["prepare_ms"] = json!(prepare_started.elapsed().as_secs_f64() * 1000.0);
    }
    let root_text = root.as_ref().map(|path| path.to_string_lossy().into_owned());
    let request = Request {
        root,
        name: name.into(),
        arguments: Value::Object(arguments),
        compact: true,
    };
    let ipc_started = Instant::now();
    if client.is_none() && allow_connect {
        *client = Some(daemon::Client::connect_or_start_traced()?);
    }
    if let Some(active_client) = client.as_mut() {
        let raw_result = match active_client.call_raw(&request) {
            Ok(bytes) => Ok(bytes),
            Err(error) if daemon::is_application_error(error.as_ref()) => Err(error),
            Err(first) => {
                *active_client = daemon::Client::connect_or_start_traced().map_err(|reconnect| {
                    invalid(format!(
                        "daemon request failed ({first}); reconnect failed ({reconnect})"
                    ))
                })?;
                active_client.call_raw(&request)
            }
        };
        if let Some(trace) = trace.as_mut() {
            trace["ipc_ms"] = json!(ipc_started.elapsed().as_secs_f64() * 1000.0);
        }
        let raw_bytes = raw_result?;
        let text = if let Some(trace) = trace.as_mut() {
            let expected_id = trace["trace_id"].as_str().expect("generated trace ID");
            let (raw_value, daemon_trace) = daemon::unwrap_trace_raw(&raw_bytes, expected_id)?;
            trace["daemon"] = daemon_trace;
            raw_value.get().to_owned()
        } else {
            String::from_utf8(raw_bytes)?
        };
        if name == "code_status" {
            let mut value: Value = serde_json::from_str(&text)?;
            let object = value
                .as_object_mut()
                .ok_or_else(|| invalid("daemon status is not an object"))?;
            object.insert("mcp_client".into(), crate::diagnostics::capabilities()?);
            return Ok(serde_json::to_string(&value)?);
        }
        let text = if matches!(name, "code_read" | "code_context") {
            compact_mcp_snippet_payload(name, text)
        } else {
            text
        };
        return Ok(crate::inline_budget::fit_inline(name, root_text.as_deref(), text));
    }
    let mut value = daemon::request(&request)?;
    if name == "code_status" {
        let object = value
            .as_object_mut()
            .ok_or_else(|| invalid("daemon status is not an object"))?;
        object.insert("mcp_client".into(), crate::diagnostics::capabilities()?);
    }
    Ok(crate::inline_budget::fit_inline(name, root_text.as_deref(), serde_json::to_string(&value)?))
}

pub fn serve_stdio() -> Result<()> {
    #[cfg(unix)]
    {
        struct RawStdin;
        impl std::io::Read for RawStdin {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                loop {
                    let rc = unsafe {
                        libc::read(
                            libc::STDIN_FILENO,
                            buf.as_mut_ptr().cast::<libc::c_void>(),
                            buf.len(),
                        )
                    };
                    if rc >= 0 {
                        return Ok(rc as usize);
                    }
                    let err = std::io::Error::last_os_error();
                    if err.kind() != std::io::ErrorKind::Interrupted {
                        return Err(err);
                    }
                }
            }
        }
        let reader = std::io::BufReader::new(RawStdin);
        serve_inner(reader, std::io::stdout().lock(), Some(|r| r.buffer().is_empty()))
    }
    #[cfg(not(unix))]
    {
        serve(std::io::stdin().lock(), std::io::stdout().lock())
    }
}

pub fn serve<R: BufRead>(input: R, output: impl Write) -> Result<()> {
    serve_inner::<R>(input, output, None)
}

#[cfg(unix)]
struct SelfExecWatch {
    disk_exe: PathBuf,
    self_dev: u64,
    self_ino: u64,
    last_check: Instant,
    disabled: bool,
}

#[cfg(unix)]
impl SelfExecWatch {
    fn capture() -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        if std::env::var_os("CCTX_DISABLE_MCP_REEXEC").is_some() {
            return None;
        }
        let link = std::fs::read_link("/proc/self/exe").ok()?;
        let cleaned = PathBuf::from(
            link.to_string_lossy()
                .trim_end_matches(" (deleted)")
                .to_owned(),
        );
        if !cleaned.is_absolute() {
            return None;
        }
        let self_meta = std::fs::metadata("/proc/self/exe").ok()?;
        Some(Self {
            disk_exe: cleaned,
            self_dev: self_meta.dev(),
            self_ino: self_meta.ino(),
            last_check: Instant::now(),
            disabled: false,
        })
    }

    fn maybe_reexec(
        &mut self,
        default_root: Option<&PathBuf>,
        client: Option<&mut daemon::Client>,
        pending_traces: &mut std::collections::VecDeque<Value>,
        output: &mut impl Write,
    ) {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::os::unix::process::CommandExt;
        if self.disabled || self.last_check.elapsed() < std::time::Duration::from_secs(2) {
            return;
        }
        self.last_check = Instant::now();
        let Ok(disk_meta) = std::fs::metadata(&self.disk_exe) else {
            return;
        };
        if !disk_meta.is_file()
            || disk_meta.permissions().mode() & 0o111 == 0
            || (disk_meta.dev() == self.self_dev && disk_meta.ino() == self.self_ino)
        {
            return;
        }
        flush_pending_traces_opt(client, pending_traces);
        let _ = output.flush();
        let mut cmd = std::process::Command::new(&self.disk_exe);
        cmd.arg("shared-mcp")
            .env("CCTX_MCP_CALLER_ID", mcp_caller_id().to_string());
        if let Some(root) = default_root {
            cmd.env("CCTX_MCP_DEFAULT_ROOT", root);
        }
        let _ = cmd.exec();
        self.disabled = true;
    }
}

#[cfg(unix)]
fn stdin_has_pending_bytes() -> bool {
    let mut avail: libc::c_int = 0;
    let rc = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::FIONREAD, &mut avail) };
    rc == 0 && avail > 0
}

fn serve_inner<R: BufRead>(
    input: R,
    mut output: impl Write,
    _is_buffer_empty: Option<fn(&R) -> bool>,
) -> Result<()> {
    let mut input = input;
    let mut default_root = match std::env::var_os("CCTX_MCP_DEFAULT_ROOT") {
        Some(inherited) => {
            let path = PathBuf::from(inherited);
            Some(crate::catalog::canonicalize(&path).unwrap_or(path))
        }
        None => std::env::current_dir()
            .ok()
            .map(|cwd| crate::catalog::canonicalize(&cwd).unwrap_or(cwd)),
    };
    let mut client: Option<daemon::Client> = None;
    let mut pending_traces: std::collections::VecDeque<Value> =
        std::collections::VecDeque::with_capacity(64);
    let trace_stderr = std::env::var_os("CCTX_TRACE_STDERR").is_some();
    #[cfg(unix)]
    let mut reexec_watch = if _is_buffer_empty.is_some() {
        SelfExecWatch::capture()
    } else {
        None
    };
    let mut bytes = Vec::with_capacity(512);
    let mut out_buf = Vec::with_capacity(4096);
    loop {
        #[cfg(unix)]
        if let (Some(watch), Some(is_empty)) = (reexec_watch.as_mut(), _is_buffer_empty)
            && is_empty(&input)
        {
            let mut ready_input = false;
            loop {
                let mut pfd = libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let rc = unsafe { libc::poll(&mut pfd, 1, -1) };
                if rc >= 0 {
                    ready_input = (pfd.revents & libc::POLLIN) != 0
                        && (pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)) == 0
                        && stdin_has_pending_bytes();
                    break;
                }
                if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    break;
                }
            }
            if ready_input {
                watch.maybe_reexec(
                    default_root.as_ref(),
                    client.as_mut(),
                    &mut pending_traces,
                    &mut output,
                );
            }
        }
        let framing = match crate::mcp::read_mcp_frame(
            &mut input,
            &mut bytes,
            MAX_MESSAGE,
            "MCP input exceeds 1 MiB",
        ) {
            Ok(Some(framing)) => framing,
            Ok(None) => {
                flush_pending_traces_opt(client.as_mut(), &mut pending_traces);
                return Ok(());
            }
            Err(error) => {
                flush_pending_traces_opt(client.as_mut(), &mut pending_traces);
                return Err(error);
            }
        };
        let started = Instant::now();
        let response = match serde_json::from_slice(&bytes) {
            Ok(value) => dispatch_with_root(
                value,
                &mut default_root,
                &mut client,
                true,
                &mut pending_traces,
            ),
            Err(error) => Some(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":error.to_string()}}),
            ),
        };
        if let Some(response) = response {
            let ser_started = Instant::now();
            crate::mcp::encode_mcp_frame(framing, &response, &mut out_buf)?;
            let serialize_ms = ser_started.elapsed().as_secs_f64() * 1000.0;
            let write_started = Instant::now();
            output.write_all(&out_buf)?;
            output.flush()?;
            let write_ms = write_started.elapsed().as_secs_f64() * 1000.0;
            if let Some(mut trace) = response["result"]["_meta"].get("cctx_trace").cloned() {
                trace["serialize_ms"] = json!(
                    trace["serialize_ms"]
                        .as_f64()
                        .expect("trace serialization time")
                        + serialize_ms
                );
                trace["write_ms"] = json!(write_ms);
                trace["total_ms"] = json!(started.elapsed().as_secs_f64() * 1000.0);
                trace["completed_unix_ms"] = json!(daemon::trace_unix_ms());
                if trace_stderr {
                    eprintln!("CCTX_CLIENT_TRACE {trace}");
                }
                if pending_traces.len() >= 64 {
                    pending_traces.pop_front();
                }
                pending_traces.push_back(trace);
            }
            #[cfg(unix)]
            if let (Some(watch), Some(is_empty)) = (reexec_watch.as_mut(), _is_buffer_empty)
                && is_empty(&input)
                && stdin_has_pending_bytes()
            {
                watch.maybe_reexec(
                    default_root.as_ref(),
                    client.as_mut(),
                    &mut pending_traces,
                    &mut output,
                );
            }
        }
    }
}
