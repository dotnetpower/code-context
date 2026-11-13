//! Unstructured log, `/tmp` offloaded tool output, and multi-command batch
//! indexing & retrieval (`pctx log` / `pctx batch` / `cctx probe {log|batch}`).
//!
//! Executes or ingests large unstructured outputs outside the model's context
//! window, scrubs secrets and ANSI escapes, persists chunks in a bounded local
//! spool (`@s1`..`@s32`), and returns an anomaly digest + BM25/Trigram RRF hits
//! strictly inside `INLINE_BUDGET` (`<8 KiB`).

use super::text_index::{
    AnomalyDigest, DEFAULT_MAX_CHUNK_BYTES, RankedHit, TextChunk, TextIndex, chunk_structured_text,
    redact_unstructured_text, summarize_anomalies,
};
use super::{
    BatchProbeArgs, LogProbeArgs, clamp_json_budget, clamp_utf8_bytes, truncate_utf8_chars,
};
use crate::catalog::{private_dir, state_dir};
use crate::inline_budget::INLINE_BUDGET;
use crate::{Result, invalid};
use fs2::FileExt;
use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_LOG_READ_BYTES: usize = 32 * 1024 * 1024;
const MAX_CMD_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
const MAX_BATCH_COMMANDS: usize = 16;
const MAX_SPOOL_FILES: usize = 32;

const PRELOAD_ENV_VARS: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "NODE_OPTIONS",
    "PYTHONSTARTUP",
    "PERL5OPT",
    "RUBYOPT",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpoolRecord {
    pub spool_id: String,
    pub created_unix: u64,
    pub raw_bytes: usize,
    pub total_lines: usize,
    pub commands: Vec<CommandRunMeta>,
    pub anomalies: AnomalyDigest,
    pub chunks: Vec<TextChunk>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandRunMeta {
    pub index: usize,
    pub command: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub truncated_capture: bool,
    pub duration_ms: u64,
    pub raw_bytes: usize,
    pub line_count: usize,
}

#[derive(Serialize, Deserialize, Default)]
struct SpoolSequence {
    next_id: u64,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn spool_dir() -> Result<PathBuf> {
    let dir = state_dir()?.join("spool");
    private_dir(&dir)?;
    Ok(dir)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("spool file missing parent directory"))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".spool-")
        .tempfile_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub fn save_spool(mut record: SpoolRecord) -> Result<String> {
    let dir = spool_dir()?;
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join("spool.lock"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir.join("spool.lock"), std::fs::Permissions::from_mode(0o600));
    }
    lock_file.lock_exclusive()?;

    let seq_path = dir.join("sequence.json");
    let mut seq: SpoolSequence = std::fs::read(&seq_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(SpoolSequence { next_id: 1 });
    let assigned_num = seq.next_id.max(1);
    seq.next_id = assigned_num + 1;
    let spool_id = format!("s{assigned_num}");
    record.spool_id = spool_id.clone();

    let payload = serde_json::to_vec(&record)?;
    write_private_file(&dir.join(format!("{spool_id}.json")), &payload)?;
    write_private_file(&seq_path, &serde_json::to_vec(&seq)?)?;

    // Evict oldest spool files beyond MAX_SPOOL_FILES
    let mut entries: Vec<(u64, PathBuf)> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let num = name
                .strip_prefix('s')
                .and_then(|s| s.strip_suffix(".json"))
                .and_then(|n| n.parse::<u64>().ok())?;
            Some((num, e.path()))
        })
        .collect();
    entries.sort_by_key(|(num, _)| *num);
    if entries.len() > MAX_SPOOL_FILES {
        let remove_count = entries.len() - MAX_SPOOL_FILES;
        for (_, old_path) in entries.into_iter().take(remove_count) {
            let _ = std::fs::remove_file(old_path);
        }
    }
    let _ = lock_file.unlock();
    Ok(spool_id)
}

pub fn load_spool(reference: &str) -> Result<SpoolRecord> {
    let dir = spool_dir()?;
    let trimmed = reference.trim().trim_start_matches('@');
    let target_path = if trimmed.eq_ignore_ascii_case("latest") {
        let mut best: Option<(u64, PathBuf)> = None;
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(num) = name
                .strip_prefix('s')
                .and_then(|s| s.strip_suffix(".json"))
                .and_then(|n| n.parse::<u64>().ok())
                && best.as_ref().is_none_or(|(b, _)| num > *b)
            {
                best = Some((num, entry.path()));
            }
        }
        best.map(|(_, p)| p)
            .ok_or_else(|| invalid("no saved probe spool found (@latest)"))?
    } else {
        if !trimmed.starts_with('s')
            || trimmed.len() < 2
            || !trimmed[1..].bytes().all(|b| b.is_ascii_digit())
        {
            return Err(invalid(format!(
                "invalid spool handle `{reference}` (expected @s1, @s2, or @latest)"
            ))
            .into());
        }
        dir.join(format!("{trimmed}.json"))
    };
    let bytes = std::fs::read(&target_path).map_err(|_| {
        invalid(format!(
            "probe spool `@{trimmed}` not found or already evicted"
        ))
    })?;
    serde_json::from_slice(&bytes).map_err(Into::into)
}

fn read_bounded_source(source: &str) -> Result<(String, String)> {
    if source == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .lock()
            .take((MAX_LOG_READ_BYTES as u64) + 1)
            .read_to_end(&mut buf)?;
        if buf.len() > MAX_LOG_READ_BYTES {
            buf.truncate(MAX_LOG_READ_BYTES);
        }
        return Ok(("stdin".to_string(), String::from_utf8_lossy(&buf).into_owned()));
    }
    let path = Path::new(source);
    let file = File::open(path)
        .map_err(|e| invalid(format!("cannot open log source `{source}`: {e}")))?;
    let mut buf = Vec::new();
    file.take((MAX_LOG_READ_BYTES as u64) + 1)
        .read_to_end(&mut buf)?;
    if buf.len() > MAX_LOG_READ_BYTES {
        buf.truncate(MAX_LOG_READ_BYTES);
    }
    let label = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| source.to_string());
    Ok((label, String::from_utf8_lossy(&buf).into_owned()))
}

fn level_matches_line(line: &str, level: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    match level {
        "fatal" | "panic" => {
            lower.contains("fatal")
                || lower.contains("panic")
                || lower.contains("segfault")
                || lower.contains("oom_kill")
        }
        "error" | "err" => {
            lower.contains("error")
                || lower.contains("exception")
                || lower.contains("traceback")
                || lower.contains("fatal")
                || lower.contains("panic")
                || lower.contains("failed")
        }
        "warn" | "warning" => lower.contains("warn") || lower.contains("deprecated"),
        "info" => lower.contains("info"),
        _ => lower.contains(&level.to_ascii_lowercase()),
    }
}

fn filter_chunks_by_regex_and_level<'a>(
    chunks: &'a [TextChunk],
    regex_pat: Option<&str>,
    level: Option<&str>,
) -> Result<Vec<&'a TextChunk>> {
    let matcher = if let Some(pat) = regex_pat {
        Some(
            RegexMatcherBuilder::new()
                .case_smart(true)
                .multi_line(true)
                .build(pat)
                .map_err(|e| invalid(format!("invalid --regex `{pat}`: {e}")))?,
        )
    } else {
        None
    };
    let mut out = Vec::new();
    for ch in chunks {
        if let Some(m) = &matcher
            && !m.is_match(ch.body.as_bytes()).unwrap_or(false)
        {
            continue;
        }
        if let Some(lvl) = level
            && !ch.body.lines().any(|line| level_matches_line(line, lvl))
        {
            continue;
        }
        out.push(ch);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn format_spool_response(
    spool_handle: Option<&str>,
    source_desc: &str,
    raw_bytes: usize,
    total_lines: usize,
    commands: &[CommandRunMeta],
    anomalies: &AnomalyDigest,
    all_chunks: &[TextChunk],
    queries: &[String],
    regex_pat: Option<&str>,
    level: Option<&str>,
    chunk_ord: Option<usize>,
    limit: usize,
    as_json: bool,
) -> Result<String> {
    let limit = limit.clamp(1, 16);

    // Direct single-chunk expansion (`--chunk <ord>`)
    if let Some(ord) = chunk_ord {
        let ch = all_chunks.get(ord).ok_or_else(|| {
            invalid(format!(
                "chunk #{ord} out of bounds (available chunks: 0..{})",
                all_chunks.len().saturating_sub(1)
            ))
        })?;
        if as_json {
            return clamp_json_budget(json!({
                "source": source_desc,
                "spool": spool_handle.map(|s| format!("@{s}")),
                "chunk_count": all_chunks.len(),
                "chunk": ch,
            }));
        }
        let mut text = format!(
            "[pctx log] source={} chunk=#{}/{} L{}-L{} ({} B)",
            source_desc,
            ch.ord,
            all_chunks.len().saturating_sub(1),
            ch.start_line,
            ch.end_line,
            ch.bytes
        );
        if let Some(sp) = spool_handle {
            text.push_str(&format!(" spool=@{sp}"));
        }
        if let Some(t) = &ch.title {
            text.push_str(&format!(" section=\"{t}\""));
        }
        text.push('\n');
        text.push_str(&ch.body);
        return Ok(clamp_utf8_bytes(&text, INLINE_BUDGET));
    }

    let filtered_refs = filter_chunks_by_regex_and_level(all_chunks, regex_pat, level)?;
    let filtered_owned: Vec<TextChunk> = filtered_refs.into_iter().cloned().collect();

    let mut query_results: Vec<(String, Vec<RankedHit>)> = Vec::new();
    if !queries.is_empty() {
        let index = TextIndex::build(&filtered_owned);
        for q in queries {
            let hits = index.search(q, limit);
            query_results.push((q.clone(), hits));
        }
    } else if regex_pat.is_some() || level.is_some() {
        let label = match (regex_pat, level) {
            (Some(r), Some(l)) => format!("regex={r} level={l}"),
            (Some(r), None) => format!("regex={r}"),
            (None, Some(l)) => format!("level={l}"),
            (None, None) => String::new(),
        };
        let hits: Vec<RankedHit> = filtered_owned
            .iter()
            .take(limit)
            .map(|ch| RankedHit {
                ord: ch.ord,
                source_label: ch.source_label.clone(),
                title: ch.title.clone(),
                start_line: ch.start_line,
                end_line: ch.end_line,
                score: 1.0,
                matched_all_terms: true,
                typo_corrected: false,
                body: ch.body.clone(),
            })
            .collect();
        query_results.push((label, hits));
    }

    if as_json {
        let mut samples_or_hits: Vec<Value> = Vec::new();
        for (q, hits) in &query_results {
            samples_or_hits.push(json!({
                "query": q,
                "hit_count": hits.len(),
                "hits": hits,
            }));
        }
        let preview_tail: Vec<String> = if query_results.is_empty() {
            all_chunks
                .last()
                .map(|c| {
                    c.body
                        .lines()
                        .rev()
                        .take(5)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .map(|l| truncate_utf8_chars(l, 140))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let payload = json!({
            "source": source_desc,
            "spool": spool_handle.map(|s| format!("@{s}")),
            "raw_bytes": raw_bytes,
            "total_lines": total_lines,
            "chunk_count": all_chunks.len(),
            "commands": commands,
            "anomalies": anomalies,
            "results": samples_or_hits,
            "tail_preview": preview_tail,
        });
        let rendered = clamp_json_budget(payload)?;
        return Ok(rendered);
    }

    // Human / LLM BPE-compact text format
    let mut out = String::with_capacity(2048);
    out.push_str(&format!(
        "[pctx] source={source_desc} raw={raw_bytes}B lines={total_lines} chunks={}",
        all_chunks.len()
    ));
    if let Some(sp) = spool_handle {
        out.push_str(&format!(" spool=@{sp}"));
    }
    out.push_str(&format!(
        " anomalies(fatal={} error={} warn={})\n",
        anomalies.fatal_count, anomalies.error_count, anomalies.warn_count
    ));

    if !commands.is_empty() {
        for cmd in commands {
            let status_str = if cmd.timed_out {
                "TIMEOUT".to_string()
            } else {
                format!("exit={}", cmd.exit_code.unwrap_or(-1))
            };
            out.push_str(&format!(
                "  cmd[{}] {} ({}ms, {}B, {}L): {}\n",
                cmd.index,
                status_str,
                cmd.duration_ms,
                cmd.raw_bytes,
                cmd.line_count,
                truncate_utf8_chars(&cmd.command, 100)
            ));
        }
    }

    if !anomalies.highlights.is_empty() {
        out.push_str("  anomalies:\n");
        for hl in anomalies.highlights.iter().take(6) {
            out.push_str(&format!("    - {hl}\n"));
        }
    }

    if !query_results.is_empty() {
        // Reserve inline budget equitably across queries
        let remaining_budget = INLINE_BUDGET.saturating_sub(out.len() + 220);
        let total_hits: usize = query_results.iter().map(|(_, h)| h.len()).sum();
        let per_hit_cap = remaining_budget
            .checked_div(total_hits)
            .map_or(800, |n| n.clamp(280, 1_600));
        for (q, hits) in &query_results {
            if hits.is_empty() {
                out.push_str(&format!("## query \"{q}\": 0 hits\n"));
                continue;
            }
            out.push_str(&format!("## query \"{q}\" ({} hit(s)):\n", hits.len()));
            for hit in hits {
                let mut header = format!(
                    "--- chunk #{} [{}] L{}-L{} score={:.4}",
                    hit.ord, hit.source_label, hit.start_line, hit.end_line, hit.score
                );
                if hit.typo_corrected {
                    header.push_str(" (typo-corrected)");
                }
                if let Some(title) = &hit.title {
                    header.push_str(&format!(" :: {title}"));
                }
                header.push('\n');
                if out.len() + header.len() + 80 > INLINE_BUDGET.saturating_sub(140) {
                    out.push_str("... [remaining hits omitted to stay <8KiB; query specific --chunk <ord>]\n");
                    break;
                }
                out.push_str(&header);
                let snippet = if hit.body.len() > per_hit_cap {
                    let mut end = per_hit_cap.min(hit.body.len());
                    while end > 0 && !hit.body.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!(
                        "{}\n  … [+{}B in chunk #{}; expand via --chunk {}]",
                        &hit.body[..end],
                        hit.body.len() - end,
                        hit.ord,
                        hit.ord
                    )
                } else {
                    hit.body.clone()
                };
                out.push_str(&snippet);
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    } else if !all_chunks.is_empty() {
        // Digest mode (no query supplied): show brief head + tail preview lines
        if let Some(first) = all_chunks.first() {
            out.push_str("  head:\n");
            for (idx, line) in first.body.lines().take(4).enumerate() {
                out.push_str(&format!(
                    "    L{}: {}\n",
                    first.start_line + idx,
                    truncate_utf8_chars(line, 120)
                ));
            }
        }
        if all_chunks.len() > 1
            && let Some(last) = all_chunks.last()
        {
            let tail_lines: Vec<&str> = last.body.lines().collect();
            let take_n = tail_lines.len().min(4);
            let skip_n = tail_lines.len().saturating_sub(take_n);
            out.push_str("  tail:\n");
            for (offset, line) in tail_lines[skip_n..].iter().enumerate() {
                out.push_str(&format!(
                    "    L{}: {}\n",
                    last.start_line + skip_n + offset,
                    truncate_utf8_chars(line, 120)
                ));
            }
        }
    }

    if let Some(sp) = spool_handle {
        let returned_bytes = out.len() + 90;
        if raw_bytes > returned_bytes {
            let saved_pct = (1.0 - (returned_bytes as f64 / raw_bytes.max(1) as f64)) * 100.0;
            out.push_str(&format!(
                "more: pctx log @{sp} -q \"<terms>\" | pctx log @{sp} --chunk <0..{}> (~{saved_pct:.1}% kept out of context)",
                all_chunks.len().saturating_sub(1)
            ));
        } else {
            out.push_str(&format!(
                "more: pctx log @{sp} -q \"<terms>\" | pctx log @{sp} --chunk <0..{}>",
                all_chunks.len().saturating_sub(1)
            ));
        }
    }
    Ok(clamp_utf8_bytes(&out, INLINE_BUDGET))
}

pub fn run_log(args: &LogProbeArgs) -> Result<String> {
    let trimmed_src = args.source.trim();
    if trimmed_src.starts_with('@') {
        let spool = load_spool(trimmed_src)?;
        let spool_id = spool.spool_id.clone();
        return format_spool_response(
            Some(&spool_id),
            &format!("@{spool_id}"),
            spool.raw_bytes,
            spool.total_lines,
            &spool.commands,
            &spool.anomalies,
            &spool.chunks,
            &args.queries,
            args.regex.as_deref(),
            args.level.as_deref(),
            args.chunk,
            args.limit,
            args.json,
        );
    }

    let (label, raw_text) = read_bounded_source(trimmed_src)?;
    let raw_bytes = raw_text.len();
    let scrubbed = redact_unstructured_text(&raw_text, &[]);
    let total_lines = if scrubbed.is_empty() {
        0
    } else {
        scrubbed.lines().count()
    };
    let anomalies = summarize_anomalies(&scrubbed, 6);
    let chunks = chunk_structured_text(&label, &scrubbed, DEFAULT_MAX_CHUNK_BYTES);

    let spool_id = if args.save {
        Some(save_spool(SpoolRecord {
            spool_id: String::new(),
            created_unix: now_unix(),
            raw_bytes,
            total_lines,
            commands: Vec::new(),
            anomalies: anomalies.clone(),
            chunks: chunks.clone(),
        })?)
    } else {
        None
    };

    format_spool_response(
        spool_id.as_deref(),
        &label,
        raw_bytes,
        total_lines,
        &[],
        &anomalies,
        &chunks,
        &args.queries,
        args.regex.as_deref(),
        args.level.as_deref(),
        args.chunk,
        args.limit,
        args.json,
    )
}

struct CapturedShellOutput {
    combined_text: String,
    exit_code: Option<i32>,
    timed_out: bool,
    truncated_capture: bool,
    duration_ms: u64,
}

fn execute_bounded_shell(cmd_str: &str, cwd: Option<&Path>, timeout: Duration) -> Result<CapturedShellOutput> {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd.exe");
        c.args(["/d", "/s", "/c", cmd_str]);
        c
    } else {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", cmd_str]);
        c
    };
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for var in PRELOAD_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd.env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("NO_COLOR", "1")
        .env("CI", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| invalid(format!("failed to spawn shell command `{cmd_str}`: {e}")))?;
    let child_pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| invalid("missing child stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| invalid("missing child stderr"))?;

    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::with_capacity(8192);
        let _ = stdout
            .take((MAX_CMD_CAPTURE_BYTES as u64) + 1)
            .read_to_end(&mut buf);
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::with_capacity(4096);
        let _ = stderr
            .take(((MAX_CMD_CAPTURE_BYTES / 2) as u64) + 1)
            .read_to_end(&mut buf);
        buf
    });

    let mut sleep_ms = 2u64;
    let (status, timed_out) = loop {
        if let Some(st) = child.try_wait()? {
            break (Some(st), false);
        }
        if started.elapsed() >= timeout {
            #[cfg(unix)]
            unsafe {
                let _ = libc::kill(-(child_pid as i32), libc::SIGKILL);
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                let _ = Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &child_pid.to_string()])
                    .creation_flags(CREATE_NO_WINDOW)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = child.kill();
            let st = child.wait().ok();
            break (st, true);
        }
        std::thread::sleep(Duration::from_millis(sleep_ms));
        sleep_ms = (sleep_ms * 2).min(25);
    };

    let mut out_bytes = out_handle.join().unwrap_or_default();
    let mut err_bytes = err_handle.join().unwrap_or_default();
    let mut truncated_capture = false;
    if out_bytes.len() > MAX_CMD_CAPTURE_BYTES {
        out_bytes.truncate(MAX_CMD_CAPTURE_BYTES);
        truncated_capture = true;
    }
    if err_bytes.len() > MAX_CMD_CAPTURE_BYTES / 2 {
        err_bytes.truncate(MAX_CMD_CAPTURE_BYTES / 2);
        truncated_capture = true;
    }

    let mut combined = String::from_utf8_lossy(&out_bytes).into_owned();
    if !err_bytes.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&err_bytes));
    }
    Ok(CapturedShellOutput {
        combined_text: combined,
        exit_code: status.and_then(|s| s.code()),
        timed_out,
        truncated_capture,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

pub fn run_batch(args: &BatchProbeArgs) -> Result<String> {
    if args.commands.is_empty() {
        return Err(invalid("at least one --cmd is required for `pctx batch`").into());
    }
    if args.commands.len() > MAX_BATCH_COMMANDS {
        return Err(invalid(format!(
            "`pctx batch` accepts at most {MAX_BATCH_COMMANDS} commands per call"
        ))
        .into());
    }
    let timeout_secs = args.timeout.clamp(1, 300);
    let timeout = Duration::from_secs(timeout_secs);
    let cwd = args.cwd.as_deref();

    let mut all_chunks: Vec<TextChunk> = Vec::new();
    let mut cmd_metas: Vec<CommandRunMeta> = Vec::new();
    let mut combined_scrubbed = String::new();
    let mut total_raw_bytes = 0usize;
    let mut total_lines = 0usize;

    for (idx, raw_cmd) in args.commands.iter().enumerate() {
        let captured = execute_bounded_shell(raw_cmd, cwd, timeout)?;
        let raw_len = captured.combined_text.len();
        total_raw_bytes = total_raw_bytes.saturating_add(raw_len);
        let scrubbed_cmd_label = redact_unstructured_text(raw_cmd, &[]);
        let scrubbed_out = redact_unstructured_text(&captured.combined_text, &[]);
        let line_count = if scrubbed_out.is_empty() {
            0
        } else {
            scrubbed_out.lines().count()
        };
        total_lines = total_lines.saturating_add(line_count);

        let short_label = format!("cmd#{}", idx + 1);
        let mut cmd_chunks = chunk_structured_text(&short_label, &scrubbed_out, DEFAULT_MAX_CHUNK_BYTES);
        for ch in &mut cmd_chunks {
            ch.ord = all_chunks.len();
            if ch.title.is_none() {
                ch.title = Some(truncate_utf8_chars(&scrubbed_cmd_label, 80));
            }
            all_chunks.push(ch.clone());
        }
        if !scrubbed_out.is_empty() {
            if !combined_scrubbed.is_empty() {
                combined_scrubbed.push('\n');
            }
            combined_scrubbed.push_str(&scrubbed_out);
        }
        cmd_metas.push(CommandRunMeta {
            index: idx + 1,
            command: scrubbed_cmd_label,
            exit_code: captured.exit_code,
            timed_out: captured.timed_out,
            truncated_capture: captured.truncated_capture,
            duration_ms: captured.duration_ms,
            raw_bytes: raw_len,
            line_count,
        });
    }

    let anomalies = summarize_anomalies(&combined_scrubbed, 6);
    let spool_id = save_spool(SpoolRecord {
        spool_id: String::new(),
        created_unix: now_unix(),
        raw_bytes: total_raw_bytes,
        total_lines,
        commands: cmd_metas.clone(),
        anomalies: anomalies.clone(),
        chunks: all_chunks.clone(),
    })?;

    format_spool_response(
        Some(&spool_id),
        &format!("batch({} cmd)", cmd_metas.len()),
        total_raw_bytes,
        total_lines,
        &cmd_metas,
        &anomalies,
        &all_chunks,
        &args.queries,
        args.regex.as_deref(),
        args.level.as_deref(),
        None,
        args.limit,
        args.json,
    )
}
