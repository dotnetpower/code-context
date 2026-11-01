use crate::daemon::{self, Request};
use crate::mcp::MAX_MESSAGE;
use crate::{Result, invalid};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Instant;

const CACHE_GUIDANCE: &str = "CCTX is an optional nonblocking root cache. CACHE_MISS returns immediately and queues only the requested root; continue that lookup with permitted host search/read. Do not poll/repeat, switch CCTX transports or run code_sync to wait for warmup. CACHE_LOAD_FAILED reports the load failure. Neither means zero matches or a missing disk file. Startup and legacy prewarm flags do not load/pin source. Inactive roots expire after15minutes and may be evicted earlier by LRU/memory limits. Do not enumerate sibling worktrees speculatively. Explicitly needed code_sync remains a waiting barrier.";

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
        tool["inputSchema"]["properties"]["root"] = json!({"type":"string","description":"Optional absolute registered project/worktree root. Pass explicitly when known; if omitted, resolves from the current workspace."});
        if tool["name"] != "code_read" && tool["name"] != "code_files" {
            tool["inputSchema"]["properties"]["paths"] = json!({"type":"array","maxItems":64,"items":{"type":"string"},"description":"Optional non-hidden relative path prefixes within root."});
            tool["inputSchema"]["properties"]["glob"] = json!({"type":"string","description":"Optional filename/path glob."});
            tool["inputSchema"]["properties"]["max_matches_per_file"] = json!({"type":"integer","minimum":1,"maximum":64,"description":"Optional per-file match cap; prioritizes symbol definitions."});
        }
        if tool["name"] == "code_read" {
            tool["inputSchema"]["properties"]["page"] = json!({"type":"boolean","default":false,
                "description":"Paginate large reads (<256KiB) returning snippets, complete, and next_ranges."});
            tool["inputSchema"]["properties"]["split_ranges"] = json!({
                "type":"boolean","default":true,
                "description":"Split broad ranges into <=500-line segments (max 64 segments, 256KiB)."
            });
        }
        let prefix = match tool["name"].as_str() {
            Some("code_files") => {
                "Use FIRST for filename/path glob discovery, NOT content."
            }
            Some("code_grep") => {
                "Use FIRST for saved-source search. Literal by default; Set regex=true for alternation (a|b)."
            }
            Some("code_context") => {
                "Use FIRST for a symbol plus nearby saved source in ONE call. Literal by default; Set regex=true for alternation (a|b)."
            }
            _ => {
                "Use FIRST for known saved-source ranges (keep <=150 lines per call to stay inline) with live SHA-256 verification."
            }
        };
        tool["description"] = json!(format!(
            "{prefix} CACHE_MISS returns immediately and schedules only this root; continue with permitted host search/read. These backend MCP tools avoid native extension-host dispatch in background sessions. Do not duplicate a pending native call or run code_sync to wait."
        ));
    }
    for (name, description) in [
        (
            "code_status",
            "Show index state (including unloaded), cache queue/misses/load errors, coverage and memory without loading, reconciling or hashing source. Ordinary cold lookups return CACHE_MISS immediately and schedule only that root; no startup prewarm.",
        ),
        (
            "code_sync",
            "Explicit full-source synchronization barrier: recheck ignore policy, hash permitted source, update changed files; use before exhaustive absence claims or after edits.",
        ),
    ] {
        items.push(json!({"name":name,"description":description,
            "inputSchema":{"type":"object","properties":{"root":{"type":"string","description":"Optional absolute registered root; omit for current workspace."}},"additionalProperties":false},
            "annotations":{"readOnlyHint":name=="code_status","destructiveHint":false,"openWorldHint":false}}));
    }
    items.push(json!({"name":"code_workspaces","description":"List explicitly registered source roots and permitted scopes/globs. Use the current workspace root for all searches.",
        "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}));
    tools
}

pub fn dispatch(request: Value) -> Option<Value> {
    let mut pending_traces = std::collections::VecDeque::new();
    dispatch_with_root(request, None, None, &mut pending_traces)
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

fn infer_root_from_arguments(arguments: &serde_json::Map<String, Value>) -> Option<PathBuf> {
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
    if raw_candidates.is_empty() {
        return None;
    }
    let specs = crate::catalog::load().ok()?;
    for raw in &raw_candidates {
        let cleaned = clean_target_candidate(raw);
        if cleaned.starts_with('/') {
            let target = std::path::Path::new(cleaned);
            if let Some(root) = specs
                .iter()
                .filter(|spec| target.starts_with(&spec.root))
                .max_by_key(|spec| spec.root.components().count())
                .map(|spec| spec.root.clone())
            {
                return Some(root);
            }
        }
    }
    if specs.len() > 1 {
        for raw in &raw_candidates {
            let cleaned = clean_target_candidate(raw).trim_start_matches("./");
            if cleaned.is_empty() || cleaned.starts_with('/') || cleaned.starts_with("..") {
                continue;
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
        let (first_seg, rest_dash) = suffix.split_once('-').unwrap_or((suffix, ""));
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

fn normalize_single_range_map(
    map: &mut serde_json::Map<String, Value>,
    default_span: u64,
) {
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
    .and_then(|v| v.as_u64());
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
        .and_then(|v| v.as_u64())
        .filter(|&n| n > 0);
    let mut start = explicit_start;
    if let Some(Value::Array(vr)) = map.remove("view_range")
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
            .and_then(Value::as_i64)
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
    let s = start
        .and_then(|v| v.as_i64())
        .filter(|&n| n > 0)
        .map(|n| n as u64)
        .or(extracted_start)
        .unwrap_or(1);
    let e = end
        .and_then(|v| v.as_i64())
        .filter(|&n| n > 0)
        .map(|n| n as u64)
        .or_else(|| count_span.map(|c| s + c - 1))
        .or(extracted_end)
        .unwrap_or(s + default_span)
        .max(s);
    map.insert("start".into(), json!(s));
    map.insert("end".into(), json!(e));
}

pub(crate) fn normalize_mcp_argument_aliases(
    name: &str,
    arguments: &mut serde_json::Map<String, Value>,
) {
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
        let regex_alias = take_first_alias(arguments, &["isRegexp", "is_regex", "useRegex"]);
        if !arguments.contains_key("regex")
            && let Some(val) = regex_alias
        {
            arguments.insert("regex".into(), val);
        }
        let ci_alias = take_first_alias(arguments, &["caseInsensitive", "case_insensitive"]);
        let cs_alias = arguments.remove("isCaseSensitive");
        if !arguments.contains_key("ignore_case") {
            if let Some(val) = ci_alias {
                arguments.insert("ignore_case".into(), val);
            } else if let Some(Value::Bool(cs)) = cs_alias {
                arguments.insert("ignore_case".into(), Value::Bool(!cs));
            }
        }
        let max_per_file = arguments.remove("maxMatchesPerFile");
        if !arguments.contains_key("max_matches_per_file")
            && let Some(val) = max_per_file
            && !val.is_null()
        {
            arguments.insert("max_matches_per_file".into(), val);
        }
    }
    if name == "code_context" {
        let radius_alias =
            take_first_alias(arguments, &["contextLines", "context_lines", "context"]);
        if !arguments.contains_key("radius")
            && let Some(val) = radius_alias
        {
            arguments.insert("radius".into(), val);
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
            ],
        );
        if !arguments.contains_key("glob")
            && let Some(val) = glob_alias
            && val.as_str().is_some_and(|s| !s.trim().is_empty())
        {
            arguments.insert("glob".into(), val);
        }
    }
    if name == "code_files"
        && !arguments.contains_key("glob")
        && let Some(pattern) = arguments.remove("pattern")
    {
        arguments.insert("glob".into(), pattern);
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
                } else {
                    cleaned_paths.push(Value::String(trimmed.to_owned()));
                }
            } else {
                cleaned_paths.push(item);
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
        }
    }
}

fn dispatch_with_root(
    request: Value,
    default_root: Option<&PathBuf>,
    client: Option<&mut daemon::Client>,
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
        "initialize" => Ok(
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},
            "serverInfo":{"name":"code-context","version":env!("CARGO_PKG_VERSION")},
            "instructions":format!("{CACHE_GUIDANCE} Use these tools before shell search/read for saved files in a registered workspace. In a foreground VS Code session where #cctxNativeContext/#cctxNativeRead/#cctxNativeGrep/#cctxNativeFiles are callable, prefer those Native tools first for lower token overhead. Use code_files for filename/path glob discovery, not a content pattern. Pass explicit absolute root when known (or omit for the current workspace); keep read batches <=150 lines (<8KiB) to stay inline. Prefer code_context for surrounding source, code_grep for content, code_read for known files. In a background session where native tools depend on unavailable foreground host dispatch, prefer these backend MCP tools. Do not duplicate pending lookups or run code_sync to repair host dispatch. If both client surfaces are unavailable but a backend terminal is usable, use cctx --backend shared with the same authorized root, except CACHE_MISS requires a permitted host read instead. Never switch transport to bypass content exclusions, trust decisions or access denials. Empty results do not cover files outside registered scopes/globs. Hidden/ignored/excluded targets require policy-aware host tools. No editor buffers or cross-root search.")}),
        ),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools()),
        "tools/call" => {
            let started = Instant::now();
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
                default_root,
                client,
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
                trace["serialize_ms"] = json!(formatting.elapsed().as_secs_f64() * 1000.0);
                trace["total_ms"] = json!(started.elapsed().as_secs_f64() * 1000.0);
                trace["completed_unix_ms"] = json!(daemon::trace_unix_ms());
                trace["success"] = json!(result["isError"] == false);
                let compact_footer = json!({
                    "trace_id": trace["trace_id"],
                    "operation": trace["operation"],
                    "success": trace["success"],
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
    client: Option<&mut daemon::Client>,
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
        .transpose()?;
    if root.as_ref().is_some_and(|path| !path.is_absolute()) {
        return Err(invalid("root must be absolute when provided").into());
    }
    if name == "code_workspaces" && (!arguments.is_empty() || root.is_some()) {
        return Err(invalid("code_workspaces takes empty arguments").into());
    }
    let root = if name == "code_workspaces" {
        None
    } else {
        Some(match root {
            Some(root) => root,
            None => infer_root_from_arguments(&arguments)
                .or_else(|| default_root.cloned())
                .ok_or_else(|| invalid("current directory is not inside a registered workspace"))?,
        })
    };
    if arguments.contains_key(daemon::TRACE_ARGUMENT)
        || arguments.contains_key(daemon::PREV_TRACE_ARGUMENT)
    {
        return Err(invalid("reserved internal trace argument").into());
    }
    if (name == "code_status" || pending_traces.len() >= 64) && !pending_traces.is_empty() {
        let items: Vec<Value> = pending_traces.drain(..).collect();
        arguments.insert(daemon::PREV_TRACE_ARGUMENT.into(), Value::Array(items));
    }
    if let Some(trace) = trace.as_mut() {
        trace["root"] = json!(root);
        arguments.insert(daemon::TRACE_ARGUMENT.into(), trace["trace_id"].clone());
        trace["prepare_ms"] = json!(prepare_started.elapsed().as_secs_f64() * 1000.0);
    }
    let request = Request {
        root,
        name: name.into(),
        arguments: Value::Object(arguments),
        compact: true,
    };
    let ipc_started = Instant::now();
    if let Some(client) = client {
        let raw_result = match client.call_raw(&request) {
            Ok(bytes) => Ok(bytes),
            Err(error) if daemon::is_application_error(error.as_ref()) => Err(error),
            Err(first) => {
                *client = daemon::Client::connect_traced().map_err(|reconnect| {
                    invalid(format!(
                        "daemon request failed ({first}); reconnect failed ({reconnect})"
                    ))
                })?;
                client.call_raw(&request)
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
        return Ok(text);
    }
    let mut value = daemon::request(&request)?;
    if name == "code_status" {
        let object = value
            .as_object_mut()
            .ok_or_else(|| invalid("daemon status is not an object"))?;
        object.insert("mcp_client".into(), crate::diagnostics::capabilities()?);
    }
    Ok(serde_json::to_string(&value)?)
}

pub fn serve(input: impl BufRead, mut output: impl Write) -> Result<()> {
    let mut input = input;
    let default_root = Some(crate::catalog::canonicalize(&std::env::current_dir()?)?);
    let mut client = daemon::Client::connect_traced()?;
    let mut pending_traces: std::collections::VecDeque<Value> =
        std::collections::VecDeque::with_capacity(64);
    let trace_stderr = std::env::var_os("CCTX_TRACE_STDERR").is_some();
    let mut bytes = Vec::with_capacity(512);
    let mut out_buf = Vec::with_capacity(4096);
    loop {
        bytes.clear();
        let count = std::io::Read::take(&mut input, MAX_MESSAGE as u64 + 1)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            flush_pending_traces(&mut client, &mut pending_traces);
            return Ok(());
        }
        if count > MAX_MESSAGE {
            flush_pending_traces(&mut client, &mut pending_traces);
            return Err(invalid("MCP input exceeds 1 MiB").into());
        }
        let started = Instant::now();
        let response = match serde_json::from_slice(&bytes) {
            Ok(value) => dispatch_with_root(
                value,
                default_root.as_ref(),
                Some(&mut client),
                &mut pending_traces,
            ),
            Err(error) => Some(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":error.to_string()}}),
            ),
        };
        if let Some(response) = response {
            let ser_started = Instant::now();
            out_buf.clear();
            serde_json::to_writer(&mut out_buf, &response)?;
            out_buf.push(b'\n');
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
        }
    }
}
