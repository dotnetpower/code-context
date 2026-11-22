use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use super::rates::{TokenRates, rates_for_openai_or_generic};

const MAX_ZSTD_DECOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ROLLOUT_WALK_DEPTH: usize = 6;
const MAX_ROLLOUT_FILES_SCANNED: usize = 2_000;
pub(crate) const MAX_JSONL_LINE_BYTES: usize = 4 * 1024 * 1024;

/// Reads one newline-terminated JSONL line up to `max_bytes`. If a line exceeds `max_bytes`,
/// drains up to the next `\n` and returns `Ok(Some(false))` so callers skip the oversized record
/// without unbounded heap allocation. Returns `Ok(None)` on EOF.
pub(crate) fn read_bounded_jsonl_line(
    reader: &mut dyn BufRead,
    out: &mut String,
    max_bytes: usize,
) -> std::io::Result<Option<bool>> {
    out.clear();
    let mut raw = Vec::new();
    let mut overflowed = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if raw.is_empty() && !overflowed {
                return Ok(None);
            }
            break;
        }
        if let Some(nl_pos) = memchr::memchr(b'\n', available) {
            let take_len = nl_pos + 1;
            if !overflowed {
                if raw.len() + take_len <= max_bytes {
                    raw.extend_from_slice(&available[..take_len]);
                } else {
                    overflowed = true;
                    raw.clear();
                }
            }
            reader.consume(take_len);
            break;
        } else {
            let chunk_len = available.len();
            if !overflowed {
                if raw.len() + chunk_len <= max_bytes {
                    raw.extend_from_slice(available);
                } else {
                    overflowed = true;
                    raw.clear();
                }
            }
            reader.consume(chunk_len);
        }
    }
    if overflowed {
        return Ok(Some(false));
    }
    match String::from_utf8(raw) {
        Ok(s) => {
            *out = s;
            Ok(Some(true))
        }
        Err(err) => {
            *out = String::from_utf8_lossy(&err.into_bytes()).into_owned();
            Ok(Some(true))
        }
    }
}

/// Decodes single-frame AND concatenated multi-frame `.zst` streams up to `MAX_ZSTD_DECOMPRESSED_BYTES`.
struct MultiFrameZstdReader<R: BufRead> {
    inner: Option<R>,
    decoder: Option<ruzstd::decoding::StreamingDecoder<R, ruzstd::decoding::FrameDecoder>>,
    emitted: u64,
}

impl<R: BufRead> MultiFrameZstdReader<R> {
    fn new(mut inner: R) -> std::io::Result<Self> {
        if inner.fill_buf()?.is_empty() {
            return Ok(Self {
                inner: Some(inner),
                decoder: None,
                emitted: 0,
            });
        }
        let decoder = ruzstd::decoding::StreamingDecoder::new(inner)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(Self {
            inner: None,
            decoder: Some(decoder),
            emitted: 0,
        })
    }
}

impl<R: BufRead> Read for MultiFrameZstdReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() || self.emitted >= MAX_ZSTD_DECOMPRESSED_BYTES {
            return Ok(0);
        }
        let max_req = (MAX_ZSTD_DECOMPRESSED_BYTES - self.emitted).min(buf.len() as u64) as usize;
        let mut empty_transitions = 0usize;
        loop {
            let Some(mut dec) = self.decoder.take() else {
                return Ok(0);
            };
            let n = match dec.read(&mut buf[..max_req]) {
                Ok(n) => n,
                Err(_) if self.emitted > 0 => {
                    // Truncated trailing frame on an active rollout: preserve already-decoded lines.
                    return Ok(0);
                }
                Err(err) => return Err(err),
            };
            if n > 0 {
                self.emitted += n as u64;
                self.decoder = Some(dec);
                return Ok(n);
            }
            // Frame finished; check if another concatenated Zstandard frame follows.
            empty_transitions += 1;
            if empty_transitions > 64 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "excessive consecutive empty zstd frames",
                ));
            }
            let mut inner = dec.into_inner();
            if inner.fill_buf()?.is_empty() {
                self.inner = Some(inner);
                return Ok(0);
            }
            match ruzstd::decoding::StreamingDecoder::new(inner) {
                Ok(next_dec) => self.decoder = Some(next_dec),
                Err(_) if self.emitted > 0 => return Ok(0),
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        e.to_string(),
                    ));
                }
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CodexTurnParsed {
    pub ts: f64,
    pub session: String,
    pub agent_id: String,
    pub model: String,
    pub effort: String,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub rates: TokenRates,
    pub est_nano_aiu: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct CodexToolCallParsed {
    pub session: String,
    pub agent_id: String,
    pub call_id: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub tool: String,
    pub args: Value,
    pub started_at: f64,
    pub completed_at: Option<f64>,
    pub success: Option<bool>,
    pub output_text: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CodexQuotaSummary {
    pub sessions_with_limits: usize,
    pub latest_ts: f64,
    pub plan_type: Option<String>,
    pub primary_5h_used_pct: Option<f64>,
    pub primary_5h_resets_at: Option<i64>,
    pub primary_5h_delta_pct: f64,
    pub secondary_7d_used_pct: Option<f64>,
    pub secondary_7d_resets_at: Option<i64>,
    pub secondary_7d_delta_pct: f64,
    pub credits_balance: Option<f64>,
}

impl CodexQuotaSummary {
    pub(crate) fn to_json(&self) -> Option<Value> {
        if self.sessions_with_limits == 0
            && self.primary_5h_used_pct.is_none()
            && self.secondary_7d_used_pct.is_none()
        {
            return None;
        }
        Some(json!({
            "sessions_with_limits": self.sessions_with_limits,
            "plan_type": self.plan_type,
            "primary_5h": {
                "window_minutes": 300,
                "used_percent": self.primary_5h_used_pct,
                "window_delta_percent": (self.primary_5h_delta_pct * 100.0).round() / 100.0,
                "resets_at_unix": self.primary_5h_resets_at,
            },
            "secondary_7d": {
                "window_minutes": 10080,
                "used_percent": self.secondary_7d_used_pct,
                "window_delta_percent": (self.secondary_7d_delta_pct * 100.0).round() / 100.0,
                "resets_at_unix": self.secondary_7d_resets_at,
            },
            "credits_balance": self.credits_balance,
        }))
    }
}

#[derive(Default)]
pub(crate) struct CodexIngestResult {
    pub turns: Vec<CodexTurnParsed>,
    pub calls: Vec<CodexToolCallParsed>,
    pub session_cwds: Vec<(String, String)>,
    pub active_sessions: Vec<String>,
    pub explicit_compactions: Vec<(String, String, f64)>,
    pub quota: CodexQuotaSummary,
}

#[derive(Clone, Copy, Default)]
struct RawUsageSnap {
    input: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
    reasoning: u64,
    total: u64,
}

pub(crate) fn ingest_codex_home(
    state_home: &Path,
    cutoff: f64,
    now: f64,
    parse_ts: fn(&Value) -> Option<f64>,
) -> CodexIngestResult {
    let mut out = CodexIngestResult::default();
    let codex_dir = state_home.join(".codex");
    if !codex_dir.is_dir() {
        return out;
    }

    let mut candidate_files: Vec<(PathBuf, bool, f64)> = Vec::new();
    for subdir in ["sessions", "archived_sessions"] {
        let root = codex_dir.join(subdir);
        if root.is_dir() {
            collect_rollout_files(&root, cutoff, 0, &mut candidate_files);
        }
    }
    if candidate_files.is_empty() {
        return out;
    }
    // Keep at most MAX_ROLLOUT_FILES_SCANNED newest files, then process in deterministic path order.
    if candidate_files.len() > MAX_ROLLOUT_FILES_SCANNED {
        candidate_files.sort_by(|a, b| b.2.total_cmp(&a.2));
        candidate_files.truncate(MAX_ROLLOUT_FILES_SCANNED);
    }
    candidate_files.sort_by(|a, b| a.0.cmp(&b.0));

    // Deduplicate stem across `sessions/` and `archived_sessions/`, preferring plain `.jsonl` over `.jsonl.zst`.
    let plain_stems: std::collections::BTreeSet<String> = candidate_files
        .iter()
        .filter(|(_, is_zst, _)| !*is_zst)
        .filter_map(|(p, _, _)| rollout_stem_key(p))
        .collect();
    let mut seen_stems: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for (path, is_zst, _) in candidate_files {
        if let Some(stem) = rollout_stem_key(&path) {
            if is_zst && plain_stems.contains(&stem) {
                continue;
            }
            if !seen_stems.insert(stem) {
                continue;
            }
        }
        let Ok(mut reader) = open_rollout_reader(&path, is_zst) else {
            continue;
        };
        parse_single_rollout(&path, &mut *reader, cutoff, now, parse_ts, &mut out);
    }

    out
}

fn rollout_stem_key(path: &Path) -> Option<String> {
    let raw = path.file_name()?.to_string_lossy();
    Some(
        raw.trim_end_matches(".zst")
            .trim_end_matches(".jsonl")
            .to_owned(),
    )
}

fn collect_rollout_files(
    dir: &Path,
    cutoff: f64,
    depth: usize,
    out: &mut Vec<(PathBuf, bool, f64)>,
) {
    if depth > MAX_ROLLOUT_WALK_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            collect_rollout_files(&path, cutoff, depth + 1, out);
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let is_zst = name_str.ends_with(".jsonl.zst");
        let is_jsonl = name_str.ends_with(".jsonl");
        if !is_zst && !is_jsonl {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0.0, |d| d.as_secs_f64());
            if mtime >= cutoff {
                out.push((path, is_zst, mtime));
            }
        }
    }
}

fn open_rollout_reader(path: &Path, is_zst: bool) -> std::io::Result<Box<dyn BufRead>> {
    let file = std::fs::File::open(path)?;
    if is_zst {
        let multi = MultiFrameZstdReader::new(BufReader::with_capacity(64 * 1024, file))?;
        Ok(Box::new(BufReader::with_capacity(64 * 1024, multi)))
    } else {
        Ok(Box::new(BufReader::with_capacity(64 * 1024, file)))
    }
}

fn parse_single_rollout(
    path: &Path,
    reader: &mut dyn BufRead,
    cutoff: f64,
    now: f64,
    parse_ts: fn(&Value) -> Option<f64>,
    out: &mut CodexIngestResult,
) {
    let stem = path
        .file_name()
        .map(|s| {
            let raw = s.to_string_lossy();
            raw.trim_end_matches(".zst")
                .trim_end_matches(".jsonl")
                .to_owned()
        })
        .unwrap_or_else(|| "codex".into());

    let mut session_id = stem;
    let mut agent_id = String::from("main");
    let mut cwd: Option<String> = None;
    let mut current_model = String::from("gpt-5.3-codex");
    let mut current_effort = String::from("-");
    let mut prev_cumulative: Option<RawUsageSnap> = None;
    let mut last_seen_step: Option<RawUsageSnap> = None;
    let mut pending_calls: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut sess_has_limits = false;
    let mut sess_had_in_window_work = false;
    let mut prev_5h: Option<(f64, Option<i64>)> = None;
    let mut sess_5h_delta = 0.0_f64;
    let mut prev_7d: Option<(f64, Option<i64>)> = None;
    let mut sess_7d_delta = 0.0_f64;

    let mut line = String::with_capacity(4096);
    while let Ok(Some(valid_line)) = read_bounded_jsonl_line(reader, &mut line, MAX_JSONL_LINE_BYTES) {
        if !valid_line || line.trim().is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let top_type = ev.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = ev.get("payload").unwrap_or(&ev);

        if top_type == "session_meta" {
            if let Some(id) = payload.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                session_id = id.to_owned();
            }
            if let Some(c) = payload.get("cwd").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                cwd = Some(c.to_owned());
                out.session_cwds.push((session_id.clone(), c.to_owned()));
            }
            if let Some(m) = payload
                .get("model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                current_model = m.to_owned();
            }
            if let Some(src) = payload.get("source")
                && (src.as_str().is_some_and(|s| s.contains("subagent"))
                    || src.get("subagent").is_some()
                    || src.get("sub_agent").is_some())
            {
                agent_id = "subagent".into();
            }
            continue;
        }

        if top_type == "turn_context" {
            if let Some(c) = payload.get("cwd").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                cwd = Some(c.to_owned());
                out.session_cwds.push((session_id.clone(), c.to_owned()));
            }
            let settings = payload
                .get("collaboration_mode")
                .and_then(|cm| cm.get("settings").or(Some(cm)));
            if let Some(m) = payload
                .get("model")
                .or_else(|| settings.and_then(|s| s.get("model")))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                current_model = m.to_owned();
            }
            if let Some(ef) = payload
                .get("effort")
                .or_else(|| payload.get("reasoning_effort"))
                .or_else(|| settings.and_then(|s| s.get("reasoning_effort")))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                current_effort = ef.to_owned();
            }
            continue;
        }

        // Advance cumulative baseline even before `cutoff` so post-cutoff deltas are exact!
        let ts_opt = parse_ts(&ev).or_else(|| parse_ts(payload));
        let in_window = ts_opt.is_some_and(|ts| ts >= cutoff && ts <= now);

        if top_type == "compacted"
            || (top_type == "event_msg"
                && payload.get("type").and_then(Value::as_str) == Some("context_compacted"))
        {
            if let Some(ts) = ts_opt.filter(|_| in_window) {
                out.explicit_compactions
                    .push((session_id.clone(), agent_id.clone(), ts));
            }
            continue;
        }

        if top_type == "event_msg"
            && payload.get("type").and_then(Value::as_str) == Some("token_count")
        {
            if in_window
                && let Some(rl) = payload.get("rate_limits").and_then(Value::as_object)
            {
                sess_has_limits = true;
                let ts = ts_opt.unwrap_or(0.0);
                if let Some(prim) = rl.get("primary").and_then(Value::as_object)
                    && let Some(pct) = prim.get("used_percent").and_then(Value::as_f64)
                {
                    let reset_at = prim
                        .get("resets_at")
                        .or_else(|| prim.get("reset_at"))
                        .and_then(Value::as_i64);
                    if let Some((prev_pct, prev_rst)) = prev_5h {
                        let window_reset = (reset_at.is_some() && prev_rst.is_some() && reset_at != prev_rst)
                            || pct + 5.0 < prev_pct;
                        if window_reset {
                            sess_5h_delta += pct.max(0.0);
                        } else if pct > prev_pct {
                            sess_5h_delta += pct - prev_pct;
                        }
                    }
                    prev_5h = Some((pct, reset_at));
                    if ts >= out.quota.latest_ts {
                        out.quota.primary_5h_used_pct = Some(pct);
                        out.quota.primary_5h_resets_at = reset_at;
                    }
                }
                if let Some(sec) = rl.get("secondary").and_then(Value::as_object)
                    && let Some(pct) = sec.get("used_percent").and_then(Value::as_f64)
                {
                    let reset_at = sec
                        .get("resets_at")
                        .or_else(|| sec.get("reset_at"))
                        .and_then(Value::as_i64);
                    if let Some((prev_pct, prev_rst)) = prev_7d {
                        let window_reset = (reset_at.is_some() && prev_rst.is_some() && reset_at != prev_rst)
                            || pct + 5.0 < prev_pct;
                        if window_reset {
                            sess_7d_delta += pct.max(0.0);
                        } else if pct > prev_pct {
                            sess_7d_delta += pct - prev_pct;
                        }
                    }
                    prev_7d = Some((pct, reset_at));
                    if ts >= out.quota.latest_ts {
                        out.quota.secondary_7d_used_pct = Some(pct);
                        out.quota.secondary_7d_resets_at = reset_at;
                    }
                }
                if ts >= out.quota.latest_ts {
                    out.quota.latest_ts = ts;
                    if let Some(pt) = rl.get("plan_type").and_then(Value::as_str) {
                        out.quota.plan_type = Some(pt.to_owned());
                    }
                    if let Some(cred) = rl
                        .get("credits")
                        .and_then(|c| c.get("balance"))
                        .and_then(|b| {
                            b.as_f64()
                                .or_else(|| b.as_str().and_then(|s| s.parse::<f64>().ok()))
                        })
                    {
                        out.quota.credits_balance = Some(cred);
                    }
                }
            }

            if let Some(info) = payload.get("info").and_then(Value::as_object) {
                let total_u = info.get("total_token_usage").and_then(parse_usage_snap);
                let last_u = info.get("last_token_usage").and_then(parse_usage_snap);

                let delta = if let Some(tot) = total_u {
                    let d = match prev_cumulative {
                        None => last_u.unwrap_or(tot),
                        Some(prev) => {
                            if tot.total == prev.total && tot.input == prev.input && tot.output == prev.output {
                                // Duplicate status/rate-limit heartbeat with unchanged token totals.
                                continue;
                            } else if tot.input >= prev.input && tot.output >= prev.output {
                                RawUsageSnap {
                                    input: tot.input - prev.input,
                                    cached: tot.cached.saturating_sub(prev.cached),
                                    cache_write: tot.cache_write.saturating_sub(prev.cache_write),
                                    output: tot.output - prev.output,
                                    reasoning: tot.reasoning.saturating_sub(prev.reasoning),
                                    total: tot.total.saturating_sub(prev.total),
                                }
                            } else {
                                // Counter reset across resumed fork; use `last_token_usage`.
                                last_u.unwrap_or(tot)
                            }
                        }
                    };
                    prev_cumulative = Some(tot);
                    d
                } else if let Some(step) = last_u {
                    if last_seen_step.is_some_and(|p| {
                        p.input == step.input && p.output == step.output && p.total == step.total
                    }) {
                        continue;
                    }
                    last_seen_step = Some(step);
                    step
                } else {
                    continue;
                };

                if in_window && (delta.input > 0 || delta.output > 0) {
                    let ts = ts_opt.unwrap_or(cutoff);
                    // OpenAI `input_tokens` already includes `cached_input_tokens`.
                    let total_in = delta.input.max(delta.cached.saturating_add(delta.cache_write));
                    let uncached = total_in
                        .saturating_sub(delta.cached)
                        .saturating_sub(delta.cache_write);
                    let rates = rates_for_openai_or_generic(&current_model, delta.cache_write, 0);
                    let est_aic = (uncached as f64) * rates.input
                        + (delta.cached as f64) * rates.cache_read
                        + (delta.cache_write as f64) * rates.cache_write
                        + (delta.output as f64) * rates.output;
                    sess_had_in_window_work = true;
                    out.active_sessions.push(session_id.clone());
                    out.turns.push(CodexTurnParsed {
                        ts,
                        session: session_id.clone(),
                        agent_id: agent_id.clone(),
                        model: current_model.clone(),
                        effort: current_effort.clone(),
                        input_tokens: total_in,
                        cache_read_tokens: delta.cached,
                        cache_write_tokens: delta.cache_write,
                        output_tokens: delta.output,
                        reasoning_tokens: delta.reasoning,
                        rates,
                        est_nano_aiu: (est_aic * 1_000_000_000.0).round() as u64,
                    });
                }
            }
            continue;
        }

        if top_type == "response_item" && in_window {
            let item_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
            let ts = ts_opt.unwrap_or(cutoff);
            match item_type {
                "function_call" | "custom_tool_call" | "local_shell_call" => {
                    let call_id = payload
                        .get("call_id")
                        .or_else(|| payload.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    if call_id.is_empty() {
                        continue;
                    }
                    let tool = if item_type == "local_shell_call" {
                        "local_shell".to_owned()
                    } else {
                        payload
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_owned()
                    };
                    let args = if item_type == "local_shell_call" {
                        payload.get("action").cloned().unwrap_or_else(|| json!({}))
                    } else if let Some(raw_args) = payload.get("arguments") {
                        match raw_args {
                            Value::String(s) => {
                                serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({"raw": s}))
                            }
                            other => other.clone(),
                        }
                    } else if let Some(inp) = payload.get("input") {
                        match inp {
                            Value::String(s) => json!({ "patch": s }),
                            other => other.clone(),
                        }
                    } else {
                        json!({})
                    };
                    sess_had_in_window_work = true;
                    out.active_sessions.push(session_id.clone());
                    let idx = out.calls.len();
                    out.calls.push(CodexToolCallParsed {
                        session: session_id.clone(),
                        agent_id: agent_id.clone(),
                        call_id: call_id.clone(),
                        cwd: cwd.clone(),
                        model: Some(current_model.clone()),
                        tool,
                        args,
                        started_at: ts,
                        completed_at: Some(ts),
                        success: Some(true),
                        output_text: None,
                    });
                    pending_calls.insert(call_id, idx);
                }
                "function_call_output" | "custom_tool_call_output" => {
                    let Some(call_id) = payload.get("call_id").and_then(Value::as_str) else {
                        continue;
                    };
                    if let Some(&idx) = pending_calls.get(call_id) {
                        let rec = &mut out.calls[idx];
                        rec.completed_at = Some(ts);
                        let raw_out = payload
                            .get("output")
                            .map(|v| match v {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            })
                            .unwrap_or_default();
                        let is_err = is_codex_tool_output_error(&raw_out);
                        rec.success = Some(!is_err);
                        rec.output_text = Some(raw_out);
                    }
                }
                _ => {}
            }
        }
    }

    if sess_has_limits && (sess_had_in_window_work || sess_5h_delta > 0.0 || sess_7d_delta > 0.0) {
        out.quota.sessions_with_limits += 1;
        out.quota.primary_5h_delta_pct += sess_5h_delta;
        out.quota.secondary_7d_delta_pct += sess_7d_delta;
    }
}

fn is_codex_tool_output_error(raw_out: &str) -> bool {
    let trimmed = raw_out.trim_start();
    trimmed.starts_with("CACHE_MISS root=")
        || trimmed.starts_with("CACHE_LOAD_FAILED root=")
        || trimmed.starts_with("READ_POLICY_DENIED")
        || trimmed.starts_with("MCP server 'code-context': CACHE_MISS root=")
        || trimmed.starts_with("MCP server 'code-context': CACHE_LOAD_FAILED root=")
        || trimmed.starts_with("MCP server 'code-context': READ_POLICY_DENIED")
        || trimmed.starts_with("Process exited with code ")
        || raw_out.contains("\"isError\":true")
        || raw_out.contains("\"is_error\":true")
        || raw_out.contains("\"exit_code\":1")
}

fn parse_usage_snap(val: &Value) -> Option<RawUsageSnap> {
    let obj = val.as_object()?;
    let input = obj
        .get("input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as u64;
    let cached = obj
        .get("cached_input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as u64;
    let cache_write = obj
        .get("cache_write_input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as u64;
    let output = obj
        .get("output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as u64;
    let reasoning = obj
        .get("reasoning_output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as u64;
    let total = obj
        .get("total_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as u64;
    Some(RawUsageSnap {
        input,
        cached,
        cache_write,
        output,
        reasoning,
        total,
    })
}
