//! Per-workspace Compaction Continuity Journal (`cctx journal` / `pctx journal`).
//!
//! Preserves high-signal session decisions, blockers, failure root causes, and
//! edited file paths in a private, secret-scrubbed local store under `CCTX_HOME`,
//! restoring a deterministic `<= 2,048 B` (`DEFAULT_STATE_BUDGET`) state block
//! plus Okapi BM25 + Trigram RRF prompt recall across context-window compactions
//! without any `PreToolUse` denials or standing MCP schema bloat.

use crate::catalog::{canonicalize, private_dir, state_dir};
use crate::inline_budget::INLINE_BUDGET;
use crate::probe::text_index::{TextChunk, TextIndex, redact_unstructured_text};
use crate::probe::{clamp_json_budget, clamp_utf8_bytes, truncate_utf8_chars};
use crate::{Result, invalid};
use clap::{Args, Subcommand, ValueEnum};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default byte cap for compaction continuity blocks (`2 KiB`).
pub const DEFAULT_STATE_BUDGET: usize = 2_048;

/// Maximum journal entries retained per workspace root before older unpinned entries compact.
pub const MAX_JOURNAL_ENTRIES: usize = 500;

/// Maximum scrubbed detail bytes stored on a single journal entry.
pub const MAX_ENTRY_DETAIL_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum JournalKind {
    #[default]
    Decision,
    Blocker,
    Failure,
    Edit,
    Finding,
    Command,
}

impl JournalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Blocker => "blocker",
            Self::Failure => "failure",
            Self::Edit => "edit",
            Self::Finding => "finding",
            Self::Command => "command",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum HookAgent {
    #[default]
    Auto,
    Vscode,
    CopilotCli,
    ClaudeCode,
    Gemini,
}

#[derive(Clone, Debug, Subcommand)]
pub enum JournalCommand {
    /// Record a decision, blocker, failure root cause, edited file, or finding
    #[command(visible_alias = "add")]
    Record(RecordArgs),
    /// Render the bounded `<= 2,048 B` compaction state block (and optional BM25 prompt recall)
    Resume(ResumeArgs),
    /// Show the full scrubbed record of one journal entry by sequence number
    Show(ShowArgs),
    /// Search workspace journal entries with Okapi BM25 + Trigram RRF
    Search(SearchArgs),
    /// Show journal entry counts, disk usage, and compaction state
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Purge journal entries by session, retention count, resolved blockers, or all
    Purge(PurgeArgs),
    /// Fail-open, zero-deny hook handler (`PreCompact`, `PostToolUse`, `UserPromptSubmit`, `SessionStart`)
    Hook {
        #[arg(long, value_enum, default_value_t = HookAgent::Auto)]
        agent: HookAgent,
    },
    /// Emit non-intrusive continuity hook JSON (no `PreToolUse` hooks; zero tool-call latency)
    HookConfig {
        #[arg(long, value_enum, default_value_t = HookAgent::Vscode)]
        agent: HookAgent,
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Clone, Debug, Args)]
pub struct RecordArgs {
    /// Entry category
    #[arg(long, short = 'k', value_enum, default_value_t = JournalKind::Decision)]
    pub kind: JournalKind,
    /// Concise 1-line summary
    #[arg(long, short = 's')]
    pub summary: String,
    /// Optional extended rationale, error trace, or verification output (`<= 4 KiB`)
    #[arg(long, short = 'd')]
    pub detail: Option<String>,
    /// Associated workspace-relative file path or `path:line` (repeatable)
    #[arg(long = "file", short = 'f')]
    pub files: Vec<String>,
    /// Optional searchable tag (repeatable)
    #[arg(long = "tag", short = 't')]
    pub tags: Vec<String>,
    /// Optional session identifier (defaults to `CCTX_SESSION_ID` or `"local"`)
    #[arg(long)]
    pub session: Option<String>,
    /// Pin entry so ring eviction preserves it across long project lifetimes
    #[arg(long)]
    pub pin: bool,
    /// Emit structured JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct ResumeArgs {
    /// Maximum UTF-8 bytes of the returned compaction state block (`256..=7400`)
    #[arg(long, default_value_t = DEFAULT_STATE_BUDGET)]
    pub budget: usize,
    /// Optional current user prompt / query to attach BM25-matched journal recall
    #[arg(long, short = 'q')]
    pub query: Option<String>,
    /// Optional sequence number to expand one full entry directly
    #[arg(long)]
    pub seq: Option<u64>,
    /// Optional session filter
    #[arg(long)]
    pub session: Option<String>,
    /// Emit structured JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct ShowArgs {
    /// 1-based journal entry sequence ID
    pub seq: u64,
    /// Emit structured JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct SearchArgs {
    /// One or more search queries ranked with Okapi BM25 + Trigram RRF
    #[arg(required = true)]
    pub queries: Vec<String>,
    /// Optional category filter
    #[arg(long, short = 'k', value_enum)]
    pub kind: Option<JournalKind>,
    /// Optional session filter
    #[arg(long)]
    pub session: Option<String>,
    /// Maximum hits per query (`1..=20`)
    #[arg(long, default_value_t = 5)]
    pub limit: usize,
    /// Emit structured JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct PurgeArgs {
    /// Delete all entries for this workspace root
    #[arg(long)]
    pub all: bool,
    /// Delete only entries recorded under this session ID
    #[arg(long)]
    pub session: Option<String>,
    /// Keep only the newest `N` entries (pinned entries are preserved unless `--all`)
    #[arg(long)]
    pub keep_last: Option<usize>,
    /// Remove all `blocker` entries once resolved
    #[arg(long)]
    pub resolve_blockers: bool,
    /// Emit structured JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournalEntry {
    pub seq: u64,
    pub ts_unix: u64,
    pub session_id: String,
    pub kind: JournalKind,
    pub pinned: bool,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingCompact {
    pub triggered_unix: u64,
    pub session_id: String,
    pub agent: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct JournalStore {
    root: String,
    next_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_compact: Option<PendingCompact>,
    #[serde(default)]
    entries: Vec<JournalEntry>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn normalize_root_key(root: &Path) -> PathBuf {
    if let Ok(canon) = canonicalize(root) {
        return canon;
    }
    if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(root))
            .unwrap_or_else(|_| root.to_path_buf())
    }
}

fn journals_dir() -> Result<PathBuf> {
    let dir = state_dir()?.join("journals");
    private_dir(&dir)?;
    Ok(dir)
}

fn root_journal_paths(root: &Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let norm = normalize_root_key(root);
    let mut hasher = Sha256::new();
    hasher.update(norm.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let mut hex_id = String::with_capacity(32);
    for b in &digest[..16] {
        hex_id.push_str(&format!("{b:02x}"));
    }
    let dir = journals_dir()?;
    let data_path = dir.join(format!("{hex_id}.json"));
    let lock_path = dir.join(format!("{hex_id}.lock"));
    Ok((norm, data_path, lock_path))
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("journal path missing parent"))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".journal-")
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

fn with_locked_store<T, F>(root: &Path, mutate: bool, f: F) -> Result<T>
where
    F: FnOnce(&Path, &mut JournalStore, &Path) -> Result<T>,
{
    let (norm_root, data_path, lock_path) = root_journal_paths(root)?;
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600));
    }
    if mutate {
        lock_file.lock_exclusive()?;
    } else {
        lock_file.lock_shared()?;
    }

    let mut store: JournalStore = if data_path.exists() {
        let bytes = std::fs::read(&data_path)?;
        serde_json::from_slice(&bytes).unwrap_or_default()
    } else {
        JournalStore::default()
    };
    if store.root.is_empty() {
        store.root = norm_root.display().to_string();
    }
    if store.next_seq == 0 {
        store.next_seq = store.entries.last().map_or(1, |e| e.seq + 1);
    }

    let result = f(&norm_root, &mut store, &data_path);
    if mutate && result.is_ok() {
        enforce_store_cap(&mut store);
        let serialized = serde_json::to_vec(&store)?;
        write_private_atomic(&data_path, &serialized)?;
    }
    let _ = lock_file.unlock();
    result
}

fn enforce_store_cap(store: &mut JournalStore) {
    if store.entries.len() <= MAX_JOURNAL_ENTRIES {
        return;
    }
    let excess = store.entries.len() - MAX_JOURNAL_ENTRIES;
    let mut dropped = 0usize;
    store.entries.retain(|entry| {
        if dropped < excess && !entry.pinned {
            dropped += 1;
            false
        } else {
            true
        }
    });
    if store.entries.len() > MAX_JOURNAL_ENTRIES {
        let drain_n = store.entries.len() - MAX_JOURNAL_ENTRIES;
        store.entries.drain(0..drain_n);
    }
}

fn clean_single_line(raw: &str, max_chars: usize) -> String {
    let scrubbed = redact_unstructured_text(raw, &[]);
    let collapsed = scrubbed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    truncate_utf8_chars(&collapsed, max_chars)
}

fn clean_detail(raw: Option<&str>) -> Option<String> {
    let text = raw?.trim();
    if text.is_empty() {
        return None;
    }
    let scrubbed = redact_unstructured_text(text, &[]);
    if scrubbed.len() <= MAX_ENTRY_DETAIL_BYTES {
        Some(scrubbed)
    } else {
        let mut end = MAX_ENTRY_DETAIL_BYTES;
        while end > 0 && !scrubbed.is_char_boundary(end) {
            end -= 1;
        }
        Some(format!("{}…[truncated]", &scrubbed[..end]))
    }
}

fn resolve_session_id(explicit: Option<&str>) -> String {
    if let Some(s) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return clean_single_line(s, 64);
    }
    if let Ok(env_id) = std::env::var("CCTX_SESSION_ID")
        && !env_id.trim().is_empty()
    {
        return clean_single_line(&env_id, 64);
    }
    "local".to_string()
}

struct EntryDraft<'a> {
    kind: JournalKind,
    summary: &'a str,
    detail: Option<&'a str>,
    files: &'a [String],
    tags: &'a [String],
    session_id: &'a str,
    pinned: bool,
}

fn append_entry_to_store(store: &mut JournalStore, draft: EntryDraft<'_>) -> Result<JournalEntry> {
    let EntryDraft {
        kind,
        summary,
        detail,
        files,
        tags,
        session_id,
        pinned,
    } = draft;
    let clean_sum = clean_single_line(summary, 200);
    if clean_sum.is_empty() {
        return Err(invalid("journal summary cannot be empty").into());
    }
    let clean_files: Vec<String> = files
        .iter()
        .map(|f| clean_single_line(f, 120))
        .filter(|f| !f.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let clean_tags: Vec<String> = tags
        .iter()
        .map(|t| clean_single_line(t, 48))
        .filter(|t| !t.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    // Coalesce consecutive unpinned Edit entries touching the exact same file set in the same session
    if kind == JournalKind::Edit
        && !pinned
        && detail.is_none()
        && !clean_files.is_empty()
        && let Some(last) = store.entries.last_mut()
        && last.kind == JournalKind::Edit
        && !last.pinned
        && last.session_id == session_id
        && last.files == clean_files
    {
        last.ts_unix = now_unix();
        last.summary = clean_sum;
        return Ok(last.clone());
    }

    let seq = store.next_seq.max(1);
    store.next_seq = seq + 1;
    let entry = JournalEntry {
        seq,
        ts_unix: now_unix(),
        session_id: session_id.to_string(),
        kind,
        pinned,
        summary: clean_sum,
        files: clean_files,
        tags: clean_tags,
        detail: clean_detail(detail),
    };
    store.entries.push(entry.clone());
    Ok(entry)
}

fn entry_to_chunk(entry: &JournalEntry) -> TextChunk {
    let mut body = entry.summary.clone();
    if !entry.files.is_empty() {
        body.push_str("\nfiles: ");
        body.push_str(&entry.files.join(", "));
    }
    if !entry.tags.is_empty() {
        body.push_str("\ntags: ");
        body.push_str(&entry.tags.join(", "));
    }
    if let Some(d) = &entry.detail {
        body.push('\n');
        body.push_str(d);
    }
    TextChunk {
        ord: entry.seq as usize,
        source_label: format!("#{} {}", entry.seq, entry.kind.as_str()),
        title: Some(format!("[{}] {}", entry.kind.as_str(), entry.summary)),
        start_line: 1,
        end_line: body.lines().count().max(1),
        bytes: body.len(),
        body,
    }
}

fn format_compact_entry_line(e: &JournalEntry) -> String {
    let pin_mark = if e.pinned { "*" } else { "" };
    let files_part = if e.files.is_empty() {
        String::new()
    } else {
        let preview: Vec<&str> = e.files.iter().take(3).map(String::as_str).collect();
        let suffix = if e.files.len() > 3 {
            format!("+{}", e.files.len() - 3)
        } else {
            String::new()
        };
        format!(" ({}{suffix})", preview.join(","))
    };
    let ptr_part = if e.detail.is_some() {
        format!(" [detail: pctx journal show {}]", e.seq)
    } else {
        String::new()
    };
    format!(
        "- #{}{} [{}]{}: {}{}",
        e.seq,
        pin_mark,
        e.kind.as_str(),
        files_part,
        truncate_utf8_chars(&e.summary, 130),
        ptr_part
    )
}

/// Renders the deterministic `<= budget` compaction continuity block (`<cctx_journal_state>`).
pub fn render_state_block(
    root_display: &str,
    entries: &[JournalEntry],
    query: Option<&str>,
    budget: usize,
) -> String {
    let budget = budget.clamp(256, INLINE_BUDGET);
    let upto = entries.last().map_or(0, |e| e.seq);
    let open_tag = format!(
        "<cctx_journal_state root=\"{root_display}\" upto=\"{upto}\" entries=\"{}\">\n",
        entries.len()
    );
    let footer = format!(
        "more: cctx --root \"{root_display}\" journal search \"<terms>\" | show <seq>\n</cctx_journal_state>"
    );

    if entries.is_empty() {
        let empty = format!("{open_tag}empty: true\n</cctx_journal_state>");
        return clamp_state_strictly(&empty, budget);
    }

    let recall_reserve = if query.is_some_and(|q| !q.trim().is_empty()) {
        (budget / 3).clamp(180, 680)
    } else {
        0
    };
    let main_limit = budget.saturating_sub(open_tag.len() + footer.len() + recall_reserve + 8);

    let mut body = String::new();
    let mut included_seqs: BTreeSet<u64> = BTreeSet::new();

    // 1. Collect deduplicated edited files summary line first (ultra-high density)
    let mut edited_files: Vec<String> = Vec::new();
    let mut seen_files: BTreeSet<String> = BTreeSet::new();
    for e in entries.iter().rev() {
        if e.kind == JournalKind::Edit {
            for f in &e.files {
                if seen_files.insert(f.clone()) {
                    edited_files.push(f.clone());
                }
            }
        }
    }
    if !edited_files.is_empty() {
        let preview: Vec<&str> = edited_files.iter().take(10).map(String::as_str).collect();
        let extra = if edited_files.len() > 10 {
            format!(" (+{} more)", edited_files.len() - 10)
        } else {
            String::new()
        };
        let files_line = format!("edited_files: {}{extra}\n", preview.join(", "));
        if body.len() + files_line.len() <= main_limit {
            body.push_str(&files_line);
        }
    }

    // 2. Priority-ordered entries: Blockers -> Pinned Decisions -> Recent Decisions -> Failures -> Findings/Commands -> Recent Edits
    let mut ordered: Vec<&JournalEntry> = Vec::new();
    ordered.extend(entries.iter().rev().filter(|e| e.kind == JournalKind::Blocker));
    ordered.extend(
        entries
            .iter()
            .rev()
            .filter(|e| e.kind == JournalKind::Decision && e.pinned),
    );
    ordered.extend(
        entries
            .iter()
            .rev()
            .filter(|e| e.kind == JournalKind::Decision && !e.pinned)
            .take(8),
    );
    ordered.extend(
        entries
            .iter()
            .rev()
            .filter(|e| e.kind == JournalKind::Failure)
            .take(5),
    );
    ordered.extend(
        entries
            .iter()
            .rev()
            .filter(|e| matches!(e.kind, JournalKind::Finding | JournalKind::Command))
            .take(5),
    );
    ordered.extend(
        entries
            .iter()
            .rev()
            .filter(|e| e.kind == JournalKind::Edit)
            .take(4),
    );

    for e in ordered {
        if included_seqs.contains(&e.seq) {
            continue;
        }
        let line = format!("{}\n", format_compact_entry_line(e));
        if body.len() + line.len() > main_limit {
            continue;
        }
        included_seqs.insert(e.seq);
        body.push_str(&line);
    }

    // 3. Optional prompt-matched BM25 + Trigram RRF recall over remaining entries
    if let Some(q) = query.map(str::trim).filter(|q| !q.is_empty()) {
        let remaining_entries: Vec<&JournalEntry> = entries
            .iter()
            .filter(|e| !included_seqs.contains(&e.seq))
            .collect();
        if !remaining_entries.is_empty() {
            let chunks: Vec<TextChunk> = remaining_entries.iter().map(|e| entry_to_chunk(e)).collect();
            let index = TextIndex::build(&chunks);
            let hits = index.search(q, 4);
            if !hits.is_empty() {
                let allowed_total = budget.saturating_sub(open_tag.len() + footer.len() + 4);
                let recall_hdr = format!("recall(query=\"{}\"):\n", truncate_utf8_chars(q, 48));
                if body.len() + recall_hdr.len() < allowed_total {
                    body.push_str(&recall_hdr);
                    for hit in hits {
                        if let Some(orig) = remaining_entries.iter().find(|e| e.seq as usize == hit.ord) {
                            let line = format!("{}\n", format_compact_entry_line(orig));
                            if body.len() + line.len() <= allowed_total {
                                body.push_str(&line);
                            }
                        }
                    }
                }
            }
        }
    }

    let full = format!("{open_tag}{body}{footer}");
    clamp_state_strictly(&full, budget)
}

fn clamp_state_strictly(rendered: &str, budget: usize) -> String {
    if rendered.len() <= budget {
        return rendered.to_string();
    }
    let close = "\n</cctx_journal_state>";
    let keep = budget.saturating_sub(close.len());
    let mut end = keep.min(rendered.len());
    while end > 0 && !rendered.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(nl) = rendered[..end].rfind('\n')
        && nl > 0
    {
        end = nl;
    }
    format!("{}{close}", &rendered[..end])
}

pub fn run(root: &Path, cmd: &JournalCommand) -> Result<String> {
    match cmd {
        JournalCommand::Record(args) => run_record(root, args),
        JournalCommand::Resume(args) => run_resume(root, args),
        JournalCommand::Show(args) => run_show(root, args),
        JournalCommand::Search(args) => run_search(root, args),
        JournalCommand::Status { json: as_json } => run_status(root, *as_json),
        JournalCommand::Purge(args) => run_purge(root, args),
        JournalCommand::Hook { agent } => Ok(run_hook_fail_open(root, *agent)),
        JournalCommand::HookConfig { agent, output } => run_hook_config(root, *agent, output.as_deref()),
    }
}

fn run_record(root: &Path, args: &RecordArgs) -> Result<String> {
    let session_id = resolve_session_id(args.session.as_deref());
    let entry = with_locked_store(root, true, |_, store, _| {
        append_entry_to_store(
            store,
            EntryDraft {
                kind: args.kind,
                summary: &args.summary,
                detail: args.detail.as_deref(),
                files: &args.files,
                tags: &args.tags,
                session_id: &session_id,
                pinned: args.pin,
            },
        )
    })?;
    if args.json {
        return clamp_json_budget(json!({
            "recorded": true,
            "entry": entry,
        }));
    }
    Ok(format!(
        "[cctx journal] recorded #{} [{}] (session={}): {}",
        entry.seq,
        entry.kind.as_str(),
        entry.session_id,
        entry.summary
    ))
}

fn format_single_entry(root_display: &str, entry: &JournalEntry, as_json: bool) -> Result<String> {
    if as_json {
        return clamp_json_budget(json!({
            "root": root_display,
            "entry": entry,
        }));
    }
    let mut out = format!(
        "[cctx journal #{}] kind={} session={} ts={}{}\nsummary: {}\n",
        entry.seq,
        entry.kind.as_str(),
        entry.session_id,
        entry.ts_unix,
        if entry.pinned { " pinned=true" } else { "" },
        entry.summary
    );
    if !entry.files.is_empty() {
        out.push_str(&format!("files: {}\n", entry.files.join(", ")));
    }
    if !entry.tags.is_empty() {
        out.push_str(&format!("tags: {}\n", entry.tags.join(", ")));
    }
    if let Some(detail) = &entry.detail {
        out.push_str("--- detail ---\n");
        out.push_str(detail);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(clamp_utf8_bytes(out.trim_end(), INLINE_BUDGET))
}

fn run_show(root: &Path, args: &ShowArgs) -> Result<String> {
    with_locked_store(root, false, |norm_root, store, _| {
        let entry = store
            .entries
            .iter()
            .find(|e| e.seq == args.seq)
            .ok_or_else(|| invalid(format!("journal entry #{} not found", args.seq)))?;
        format_single_entry(&norm_root.display().to_string(), entry, args.json)
    })
}

fn run_resume(root: &Path, args: &ResumeArgs) -> Result<String> {
    if let Some(seq) = args.seq {
        return run_show(
            root,
            &ShowArgs {
                seq,
                json: args.json,
            },
        );
    }
    with_locked_store(root, false, |norm_root, store, _| {
        let filtered: Vec<JournalEntry> = match &args.session {
            Some(sess) => store
                .entries
                .iter()
                .filter(|e| e.session_id == *sess || e.pinned)
                .cloned()
                .collect(),
            None => store.entries.clone(),
        };
        let root_str = norm_root.display().to_string();
        let block = render_state_block(&root_str, &filtered, args.query.as_deref(), args.budget);
        if args.json {
            return clamp_json_budget(json!({
                "root": root_str,
                "upto_seq": filtered.last().map_or(0, |e| e.seq),
                "entry_count": filtered.len(),
                "bytes": block.len(),
                "budget": args.budget.clamp(256, INLINE_BUDGET),
                "state_block": block,
            }));
        }
        Ok(block)
    })
}

fn run_search(root: &Path, args: &SearchArgs) -> Result<String> {
    let limit = args.limit.clamp(1, 20);
    with_locked_store(root, false, |norm_root, store, _| {
        let candidates: Vec<&JournalEntry> = store
            .entries
            .iter()
            .filter(|e| args.kind.is_none_or(|k| e.kind == k))
            .filter(|e| args.session.as_ref().is_none_or(|s| &e.session_id == s))
            .collect();
        let chunks: Vec<TextChunk> = candidates.iter().map(|e| entry_to_chunk(e)).collect();
        let index = TextIndex::build(&chunks);

        let mut results_json = Vec::new();
        let mut text_out = format!(
            "[cctx journal search] root={} indexed_entries={}\n",
            norm_root.display(),
            candidates.len()
        );

        for q in &args.queries {
            let hits = index.search(q, limit);
            if args.json {
                results_json.push(json!({
                    "query": q,
                    "hit_count": hits.len(),
                    "hits": hits,
                }));
            } else if hits.is_empty() {
                text_out.push_str(&format!("## query \"{q}\": 0 hits\n"));
            } else {
                text_out.push_str(&format!("## query \"{q}\" ({} hit(s)):\n", hits.len()));
                for h in hits {
                    if let Some(entry) = candidates.iter().find(|e| e.seq as usize == h.ord) {
                        text_out.push_str(&format!(
                            "  {} (score={:.4}{})\n",
                            format_compact_entry_line(entry),
                            h.score,
                            if h.typo_corrected { ", typo-corrected" } else { "" }
                        ));
                        if let Some(detail) = &entry.detail {
                            text_out.push_str(&format!(
                                "    detail: {}\n",
                                truncate_utf8_chars(&detail.replace('\n', " ⏎ "), 200)
                            ));
                        }
                    }
                }
            }
        }
        if args.json {
            return clamp_json_budget(json!({
                "root": norm_root.display().to_string(),
                "indexed_entries": candidates.len(),
                "results": results_json,
            }));
        }
        Ok(clamp_utf8_bytes(text_out.trim_end(), INLINE_BUDGET))
    })
}

fn run_status(root: &Path, as_json: bool) -> Result<String> {
    with_locked_store(root, false, |norm_root, store, data_path| {
        let disk_bytes = std::fs::metadata(data_path).map_or(0, |m| m.len());
        let mut by_kind: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut pinned_count = 0usize;
        for e in &store.entries {
            *by_kind.entry(e.kind.as_str()).or_insert(0) += 1;
            if e.pinned {
                pinned_count += 1;
            }
        }
        let upto = store.entries.last().map_or(0, |e| e.seq);
        if as_json {
            return clamp_json_budget(json!({
                "root": norm_root.display().to_string(),
                "entries": store.entries.len(),
                "pinned": pinned_count,
                "upto_seq": upto,
                "disk_bytes": disk_bytes,
                "by_kind": by_kind,
                "pending_compact": store.pending_compact,
            }));
        }
        Ok(format!(
            "[cctx journal status] root={} entries={} pinned={} upto=#{} disk={}B pending_compact={} kinds={:?}",
            norm_root.display(),
            store.entries.len(),
            pinned_count,
            upto,
            disk_bytes,
            store.pending_compact.is_some(),
            by_kind
        ))
    })
}

fn run_purge(root: &Path, args: &PurgeArgs) -> Result<String> {
    if !args.all && args.session.is_none() && args.keep_last.is_none() && !args.resolve_blockers {
        return Err(invalid(
            "specify at least one purge selector: --all, --session <ID>, --keep-last <N>, or --resolve-blockers",
        )
        .into());
    }
    let (deleted, remaining) = with_locked_store(root, true, |_, store, _| {
        let before = store.entries.len();
        if args.all {
            store.entries.clear();
            store.pending_compact = None;
            return Ok((before, 0usize));
        }
        if args.resolve_blockers {
            store.entries.retain(|e| e.kind != JournalKind::Blocker);
        }
        if let Some(sess) = &args.session {
            store.entries.retain(|e| &e.session_id != sess);
        }
        if let Some(keep_n) = args.keep_last
            && store.entries.len() > keep_n
        {
            let mut to_remove = store.entries.len() - keep_n;
            store.entries.retain(|e| {
                if to_remove > 0 && !e.pinned {
                    to_remove -= 1;
                    false
                } else {
                    true
                }
            });
        }
        let after = store.entries.len();
        Ok((before.saturating_sub(after), after))
    })?;

    if args.json {
        return clamp_json_budget(json!({
            "deleted": deleted,
            "remaining": remaining,
        }));
    }
    Ok(format!(
        "[cctx journal purge] deleted={deleted} remaining={remaining}"
    ))
}

// ---------------- Multi-Agent Fail-Open Hook Handler ----------------

#[derive(Debug)]
struct DecodedHookEvent {
    canonical_event: &'static str,
    raw_event_name: String,
    camel_style: bool,
    session_id: String,
    cwd: Option<PathBuf>,
    source: Option<String>,
    prompt: Option<String>,
    tool_name: Option<String>,
    tool_input: Value,
    tool_is_error: bool,
    tool_result_text: String,
}

fn decode_hook_stdin(raw: &Value) -> Option<DecodedHookEvent> {
    let obj = raw.as_object()?;
    let (raw_event, camel_style) = if let Some(s) = obj.get("hook_event_name").and_then(Value::as_str) {
        (s.to_string(), false)
    } else if let Some(s) = obj.get("hookEventName").and_then(Value::as_str) {
        (s.to_string(), true)
    } else if obj.contains_key("toolResult") {
        ("postToolUse".to_string(), true)
    } else if obj.contains_key("prompt") {
        ("userPromptSubmitted".to_string(), true)
    } else if obj.contains_key("trigger") {
        ("preCompact".to_string(), true)
    } else if obj.contains_key("source") {
        ("sessionStart".to_string(), true)
    } else {
        return None;
    };

    let canonical_event = match raw_event.as_str() {
        "PreCompact" | "preCompact" | "PreCompress" | "compaction" => "PreCompact",
        "PostToolUse" | "postToolUse" | "AfterTool" | "tool_result" | "execute.after" => "PostToolUse",
        "UserPromptSubmit" | "userPromptSubmitted" | "BeforeAgent" | "prompt" => "UserPromptSubmit",
        "SessionStart" | "sessionStart" | "session_start" | "context" => "SessionStart",
        _ => "Other",
    };

    let session_id = obj
        .get("session_id")
        .or_else(|| obj.get("sessionId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("hook")
        .to_string();
    let cwd = obj
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("CLAUDE_PROJECT_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        });
    let source = obj.get("source").and_then(Value::as_str).map(String::from);
    let prompt = obj.get("prompt").and_then(Value::as_str).map(String::from);
    let tool_name = obj
        .get("tool_name")
        .or_else(|| obj.get("toolName"))
        .and_then(Value::as_str)
        .map(String::from);

    let tool_input = if let Some(inp) = obj.get("tool_input") {
        inp.clone()
    } else if let Some(args) = obj.get("toolArgs") {
        match args {
            Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
            other => other.clone(),
        }
    } else {
        Value::Null
    };

    let (tool_is_error, tool_result_text) = if let Some(tr) = obj.get("toolResult").and_then(Value::as_object) {
        let is_err = tr
            .get("resultType")
            .and_then(Value::as_str)
            .is_some_and(|rt| rt != "success");
        let text = tr
            .get("textResultForLlm")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        (is_err, text)
    } else if let Some(resp) = obj.get("tool_response") {
        match resp {
            Value::String(s) => (false, s.clone()),
            Value::Object(map) => {
                let is_err = map.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                    || map
                        .get("exitCode")
                        .or_else(|| map.get("exit_code"))
                        .and_then(Value::as_i64)
                        .is_some_and(|code| code != 0);
                let text = map
                    .get("stdout")
                    .or_else(|| map.get("output"))
                    .or_else(|| map.get("content"))
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| serde_json::to_string(resp).unwrap_or_default());
                (is_err, text)
            }
            _ => (false, String::new()),
        }
    } else {
        (false, String::new())
    };

    Some(DecodedHookEvent {
        canonical_event,
        raw_event_name: raw_event,
        camel_style,
        session_id,
        cwd,
        source,
        prompt,
        tool_name,
        tool_input,
        tool_is_error,
        tool_result_text,
    })
}

fn relativize_path(root: &Path, raw_path: &str) -> String {
    let p = Path::new(raw_path);
    if let Ok(rel) = p.strip_prefix(root) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    raw_path.replace('\\', "/")
}

fn extract_edited_files(root: &Path, tool_name: &str, input: &Value) -> Vec<String> {
    let lower = tool_name.to_ascii_lowercase();
    let is_edit = matches!(
        lower.as_str(),
        "edit" | "create" | "write" | "multiedit" | "notebookedit" | "apply_patch" | "replace_string_in_file"
    );
    if !is_edit {
        return Vec::new();
    }
    let mut files = Vec::new();
    if let Some(obj) = input.as_object() {
        for key in ["path", "file_path", "filePath", "notebook_path"] {
            if let Some(p) = obj.get(key).and_then(Value::as_str)
                && !p.is_empty()
            {
                files.push(relativize_path(root, p));
            }
        }
        if let Some(arr) = obj.get("filePaths").and_then(Value::as_array) {
            for item in arr.iter().filter_map(Value::as_str) {
                files.push(relativize_path(root, item));
            }
        }
    }
    files
}

fn encode_additional_context(ev: &DecodedHookEvent, agent: HookAgent, context: &str) -> String {
    let use_camel_top = match agent {
        HookAgent::CopilotCli => true,
        HookAgent::ClaudeCode | HookAgent::Vscode | HookAgent::Gemini => false,
        HookAgent::Auto => ev.camel_style && ev.raw_event_name.chars().next().is_some_and(char::is_lowercase),
    };
    if use_camel_top {
        serde_json::to_string(&json!({
            "additionalContext": context,
        }))
        .unwrap_or_else(|_| "{}".to_string())
    } else {
        serde_json::to_string(&json!({
            "hookSpecificOutput": {
                "hookEventName": ev.raw_event_name,
                "additionalContext": context,
            }
        }))
        .unwrap_or_else(|_| "{}".to_string())
    }
}

pub fn handle_hook_payload(explicit_root: &Path, agent: HookAgent, raw_stdin: &[u8]) -> Result<String> {
    let trimmed = std::str::from_utf8(raw_stdin)
        .map_err(|_| invalid("non-utf8 hook payload"))?
        .trim();
    if trimmed.is_empty() {
        return Ok("{}".to_string());
    }
    let parsed: Value = serde_json::from_str(trimmed)?;
    let Some(ev) = decode_hook_stdin(&parsed) else {
        return Ok("{}".to_string());
    };
    let effective_root = if explicit_root == Path::new(".") {
        ev.cwd.as_deref().unwrap_or(explicit_root)
    } else {
        explicit_root
    };

    match ev.canonical_event {
        "PreCompact" => {
            with_locked_store(effective_root, true, |_, store, _| {
                store.pending_compact = Some(PendingCompact {
                    triggered_unix: now_unix(),
                    session_id: clean_single_line(&ev.session_id, 64),
                    agent: format!("{agent:?}").to_ascii_lowercase(),
                });
                Ok(())
            })?;
            Ok("{}".to_string())
        }
        "PostToolUse" => {
            if let Some(tool_name) = ev.tool_name.as_deref() {
                let norm_root = normalize_root_key(effective_root);
                let edited = extract_edited_files(&norm_root, tool_name, &ev.tool_input);
                if !edited.is_empty() {
                    let summary = format!("Edited {}", edited.join(", "));
                    let clean_sess = clean_single_line(&ev.session_id, 64);
                    let _ = with_locked_store(&norm_root, true, |_, store, _| {
                        let _ = append_entry_to_store(
                            store,
                            EntryDraft {
                                kind: JournalKind::Edit,
                                summary: &summary,
                                detail: None,
                                files: &edited,
                                tags: &[],
                                session_id: &clean_sess,
                                pinned: false,
                            },
                        )?;
                        Ok(())
                    });
                } else if ev.tool_is_error && !ev.tool_result_text.trim().is_empty() {
                    let first_err_line = ev
                        .tool_result_text
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("tool error");
                    let summary = format!("{tool_name} failed: {}", truncate_utf8_chars(first_err_line.trim(), 120));
                    let detail_snip = truncate_utf8_chars(&ev.tool_result_text, 600);
                    let clean_sess = clean_single_line(&ev.session_id, 64);
                    let _ = with_locked_store(&norm_root, true, |_, store, _| {
                        let _ = append_entry_to_store(
                            store,
                            EntryDraft {
                                kind: JournalKind::Failure,
                                summary: &summary,
                                detail: Some(&detail_snip),
                                files: &[],
                                tags: &[],
                                session_id: &clean_sess,
                                pinned: false,
                            },
                        )?;
                        Ok(())
                    });
                }
            }
            Ok("{}".to_string())
        }
        "SessionStart" | "UserPromptSubmit" => {
            let is_compact_source = ev
                .source
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case("compact") || s.eq_ignore_ascii_case("resume"));
            let injected = with_locked_store(effective_root, true, |norm_root, store, _| {
                let should_inject = store.pending_compact.is_some()
                    || (ev.canonical_event == "SessionStart" && is_compact_source);
                if !should_inject || store.entries.is_empty() {
                    if should_inject {
                        store.pending_compact = None;
                    }
                    return Ok(None);
                }
                store.pending_compact = None;
                let block = render_state_block(
                    &norm_root.display().to_string(),
                    &store.entries,
                    ev.prompt.as_deref(),
                    DEFAULT_STATE_BUDGET,
                );
                Ok(Some(block))
            })?;
            if let Some(block) = injected {
                return Ok(encode_additional_context(&ev, agent, &block));
            }
            Ok("{}".to_string())
        }
        _ => Ok("{}".to_string()),
    }
}

/// Fail-open wrapper: always returns valid JSON (never panics or causes non-zero hook exit).
pub fn run_hook_fail_open(explicit_root: &Path, agent: HookAgent) -> String {
    let mut stdin_buf = Vec::with_capacity(4096);
    if std::io::stdin()
        .lock()
        .take(2 * 1024 * 1024)
        .read_to_end(&mut stdin_buf)
        .is_err()
    {
        return "{}".to_string();
    }
    handle_hook_payload(explicit_root, agent, &stdin_buf).unwrap_or_else(|_| "{}".to_string())
}

fn run_hook_config(root: &Path, agent: HookAgent, output: Option<&Path>) -> Result<String> {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "cctx".to_string());
    let root_arg = if root == Path::new(".") {
        String::new()
    } else {
        format!(" --root \"{}\"", normalize_root_key(root).display())
    };
    let agent_flag = match agent {
        HookAgent::CopilotCli => "copilot-cli",
        HookAgent::ClaudeCode => "claude-code",
        HookAgent::Gemini => "gemini",
        HookAgent::Vscode | HookAgent::Auto => "vscode",
    };
    let cmd = format!("\"{exe}\"{root_arg} journal hook --agent {agent_flag}");

    let config = match agent {
        HookAgent::CopilotCli => json!({
            "version": 1,
            "hooks": {
                "sessionStart": [{"type": "command", "bash": cmd, "timeoutSec": 5}],
                "userPromptSubmitted": [{"type": "command", "bash": cmd, "timeoutSec": 5}],
                "postToolUse": [{"type": "command", "bash": cmd, "timeoutSec": 5}],
                "preCompact": [{"type": "command", "bash": cmd, "timeoutSec": 5}]
            }
        }),
        _ => json!({
            "hooks": {
                "SessionStart": [{"hooks": [{"type": "command", "command": cmd, "timeout": 5}]}],
                "UserPromptSubmit": [{"hooks": [{"type": "command", "command": cmd, "timeout": 5}]}],
                "PostToolUse": [{"hooks": [{"type": "command", "command": cmd, "timeout": 5}]}],
                "PreCompact": [{"hooks": [{"type": "command", "command": cmd, "timeout": 5}]}]
            }
        }),
    };

    let serialized = serde_json::to_string_pretty(&config)?;
    if let Some(out_path) = output {
        if let Some(parent) = out_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(out_path, format!("{serialized}\n"))?;
    }
    Ok(serialized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_block_respects_strict_2048b_budget_and_includes_recall() {
        let mut entries = Vec::new();
        for i in 1..=40 {
            entries.push(JournalEntry {
                seq: i,
                ts_unix: 1_700_000_000 + i,
                session_id: "s1".to_string(),
                kind: if i == 3 {
                    JournalKind::Blocker
                } else if i % 5 == 0 {
                    JournalKind::Decision
                } else {
                    JournalKind::Finding
                },
                pinned: i == 5,
                summary: format!("Entry summary number {i} with detailed context padding for budget verification"),
                files: vec![format!("src/mod_{i}.rs")],
                tags: vec!["perf".to_string()],
                detail: Some("extended rationale".to_string()),
            });
        }
        entries.push(JournalEntry {
            seq: 41,
            ts_unix: 1_700_000_100,
            session_id: "s1".to_string(),
            kind: JournalKind::Finding,
            pinned: false,
            summary: "SQLite WAL checkpoint timeout fixed via busy_timeout=5000ms".to_string(),
            files: vec!["src/db.rs".to_string()],
            tags: vec!["wal".to_string()],
            detail: None,
        });

        let block = render_state_block(
            "/path/to/project",
            &entries,
            Some("WAL checkpoint busy_timeout"),
            DEFAULT_STATE_BUDGET,
        );
        assert!(
            block.len() <= DEFAULT_STATE_BUDGET,
            "exceeded budget: {} > {}",
            block.len(),
            DEFAULT_STATE_BUDGET
        );
        assert!(block.starts_with("<cctx_journal_state"));
        assert!(block.ends_with("</cctx_journal_state>"));
        assert!(block.contains("#3 [blocker]"));
        assert!(block.contains("#5* [decision]"));
        assert!(block.contains("busy_timeout=5000ms"));
    }
}
