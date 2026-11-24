//! Session-driven adaptive BPE & formula profile builder for Copilot (WSL + Windows).
//!
//! Scans local Copilot session logs (`events.jsonl` `user.message` records) across Linux/WSL
//! and Windows user profiles, learns a corpus-specific Byte-Pair Encoding (BPE) merge table
//! plus safe, meaning-preserving formula compression rules, and persists them atomically to
//! `state_dir()/bpe-profile.json`.

use crate::catalog::{private_dir, state_dir};
use crate::diagnostics::codex::{MAX_JSONL_LINE_BYTES, read_bounded_jsonl_line};
use crate::prompt_formula::{
    TokenizerProfile, contains_hangul, estimate_bpe_tokens_for, normalize_bpe_operators,
};
use crate::{Result, invalid};
use clap::ValueEnum;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const PROFILE_SCHEMA_VERSION: u32 = 1;
pub const MAX_PROFILE_BYTES: usize = 512 * 1024;
const MAX_AUTO_STATE_BYTES: usize = 64 * 1024;
const MAX_SESSION_FILES: usize = 512;
const MAX_MESSAGES_SCANNED: usize = 4_000;
const MAX_MESSAGE_CHARS: usize = 2_048;
const MAX_XML_SCAN_BYTES: usize = 16 * 1024;
const MAX_BPE_VOCAB_WORDS: usize = 4_096;
const MAX_TOKENS_PER_MSG: usize = 160;
const MAX_NGRAM_ENTRIES: usize = 65_536;
const MAX_MERGES_LIMIT: usize = 512;
const MAX_LEARNED_FILLERS: usize = 32;
const MAX_LEARNED_REPLACEMENTS: usize = 64;
const MAX_DIR_ENTRIES: usize = 2_048;
const MAX_CONCURRENT_WAITERS: usize = 4;
const ERROR_BACKOFF_SECS: u64 = 3_600;
#[cfg(not(windows))]
const MAX_WINDOWS_HOME_ENTRIES: usize = 12;

static BUILD_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static ACTIVE_WAITERS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum BpeSource {
    /// Scan GitHub Copilot session logs (`~/.copilot/session-state/*/events.jsonl`).
    #[default]
    Copilot,
    /// Scan Copilot plus Claude Code (`~/.claude/projects/*/*.jsonl`) user prompts.
    All,
}

impl BpeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Copilot => "copilot",
            Self::All => "all",
        }
    }

    pub fn parse_str(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "copilot" => Some(Self::Copilot),
            "all" | "multi" => Some(Self::All),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BpeMergeRule {
    pub left: String,
    pub right: String,
    pub merged: String,
    pub frequency: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LearnedReplacement {
    pub from: String,
    pub to: String,
    pub frequency: usize,
    pub session_count: usize,
    pub tokens_saved: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionWatermark {
    pub session_count: usize,
    pub message_count: usize,
    pub latest_event_mtime_secs: u64,
    pub corpus_fingerprint: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CorpusStats {
    pub sessions_scanned: usize,
    pub messages_scanned: usize,
    pub messages_kept: usize,
    pub hangul_ratio: f64,
    pub avg_prompt_chars: f64,
    pub avg_raw_bpe_tokens: f64,
    pub avg_compressed_bpe_tokens: f64,
    pub estimated_token_savings_pct: f64,
    pub bpe_avg_tokens_per_prompt: f64,
    pub bpe_chars_per_token: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TokenizerCalibration {
    pub samples: usize,
    pub model_counts: BTreeMap<String, usize>,
    pub recommended_tokenizer: String,
    pub hangul_message_share: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AutoBuildPolicy {
    pub enabled: bool,
    pub min_interval_hours: f64,
    pub min_new_sessions: usize,
    pub min_new_messages: usize,
    pub drift_threshold: f64,
}

impl Default for AutoBuildPolicy {
    fn default() -> Self {
        Self {
            enabled: env_bool("CCTX_BPE_AUTO_BUILD").unwrap_or(true),
            min_interval_hours: env_f64("CCTX_BPE_AUTO_MIN_HOURS")
                .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 720.0)
                .unwrap_or(24.0),
            min_new_sessions: env_usize("CCTX_BPE_AUTO_MIN_NEW_SESSIONS")
                .filter(|v| *v >= 1 && *v <= 500)
                .unwrap_or(3),
            min_new_messages: env_usize("CCTX_BPE_AUTO_MIN_NEW_MESSAGES")
                .filter(|v| *v >= 1 && *v <= 5_000)
                .unwrap_or(25),
            drift_threshold: env_f64("CCTX_BPE_AUTO_DRIFT_THRESHOLD")
                .filter(|v| v.is_finite() && *v > 0.0 && *v < 1.0)
                .unwrap_or(0.18),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BpeProfile {
    pub schema_version: u32,
    pub built_at_unix: u64,
    pub platform: String,
    pub source: BpeSource,
    pub window_hours: f64,
    pub tokenizer: String,
    pub watermark: SessionWatermark,
    pub stats: CorpusStats,
    pub calibration: TokenizerCalibration,
    pub auto_policy: AutoBuildPolicy,
    #[serde(default)]
    pub merges: Vec<BpeMergeRule>,
    #[serde(default)]
    pub learned_fillers: Vec<String>,
    #[serde(default)]
    pub learned_replacements: Vec<LearnedReplacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_digest: Option<String>,
}

impl BpeProfile {
    pub fn tokenizer_profile(&self) -> TokenizerProfile {
        TokenizerProfile::for_model(&self.tokenizer)
    }

    pub fn compute_digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.schema_version.to_le_bytes());
        hasher.update(self.source.as_str().as_bytes());
        hasher.update(b"\0");
        hasher.update(self.tokenizer.as_bytes());
        hasher.update(b"\0");
        for m in &self.merges {
            hasher.update(m.left.as_bytes());
            hasher.update(b"\x1f");
            hasher.update(m.right.as_bytes());
            hasher.update(b"\x1e");
        }
        hasher.update(b"\0");
        for f in &self.learned_fillers {
            hasher.update(f.as_bytes());
            hasher.update(b"\x1e");
        }
        hasher.update(b"\0");
        for r in &self.learned_replacements {
            hasher.update(r.from.as_bytes());
            hasher.update(b"\x1f");
            hasher.update(r.to.as_bytes());
            hasher.update(b"\x1e");
        }
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(64);
        for b in digest {
            hex.push_str(&format!("{b:02x}"));
        }
        hex
    }
}

impl BpeBuildConfig {
    pub fn validate(&self) -> Result<()> {
        if !self.window_hours.is_finite() || self.window_hours < 1.0 || self.window_hours > 8760.0 {
            return Err(invalid("window_hours must be finite and between 1 and 8760").into());
        }
        if !(2..=100).contains(&self.min_freq) {
            return Err(invalid("min_freq must be between 2 and 100").into());
        }
        if !(1..=50).contains(&self.min_sessions) {
            return Err(invalid("min_sessions must be between 1 and 50").into());
        }
        if !(1..=20).contains(&self.min_savings_tokens) {
            return Err(invalid("min_savings_tokens must be between 1 and 20").into());
        }
        if self.max_merges > MAX_MERGES_LIMIT {
            return Err(invalid(format!("max_merges must be between 0 and {MAX_MERGES_LIMIT}")).into());
        }
        for dir in &self.session_dirs {
            let raw = dir.to_string_lossy();
            if raw.contains('\0') || raw.chars().any(|c| c.is_control()) {
                return Err(invalid("session_dir contains control characters").into());
            }
            if dir.components().any(|c| matches!(c, Component::ParentDir)) {
                return Err(invalid("session_dir must not contain '..' traversal segments").into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditSeverity {
    Low,
    Medium,
    High,
    Critical,
}

impl AuditSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AuditFinding {
    pub code: String,
    pub severity: AuditSeverity,
    pub category: String,
    pub message: String,
    pub remediation: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AuditCounts {
    pub critical: usize,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BpeAuditReport {
    pub passed: bool,
    pub highest_severity: String,
    pub counts: AuditCounts,
    pub exists: bool,
    pub digest_valid: Option<bool>,
    pub profile_path: String,
    pub platform: String,
    pub findings: Vec<AuditFinding>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct AutoBuildState {
    last_checked_unix: u64,
    last_error_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct BpeBuildConfig {
    pub window_hours: f64,
    pub source: BpeSource,
    pub min_freq: usize,
    pub min_sessions: usize,
    pub min_savings_tokens: usize,
    pub max_merges: usize,
    pub include_windows_homes: bool,
    pub session_dirs: Vec<PathBuf>,
    pub dry_run: bool,
    pub if_needed: bool,
    pub tokenizer: Option<TokenizerProfile>,
    pub now_unix: Option<u64>,
    pub state_dir_override: Option<PathBuf>,
}

impl Default for BpeBuildConfig {
    fn default() -> Self {
        Self {
            window_hours: 168.0, // 7 days
            source: BpeSource::Copilot,
            min_freq: 3,
            min_sessions: 2,
            min_savings_tokens: 1,
            max_merges: 256,
            include_windows_homes: default_include_windows_homes(),
            session_dirs: Vec::new(),
            dry_run: false,
            if_needed: false,
            tokenizer: None,
            now_unix: None,
            state_dir_override: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RebuildCheck {
    pub needed: bool,
    pub reason: String,
    pub changed_sessions: usize,
    pub total_sessions_in_window: usize,
    pub latest_event_mtime_secs: u64,
    pub hours_since_last_build: Option<f64>,
    pub drift_score: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BpeBuildReport {
    pub action: String,
    pub applied: bool,
    pub dry_run: bool,
    pub reason: String,
    pub profile_path: String,
    pub platform: String,
    pub scanned_homes: Vec<String>,
    pub rebuild_check: RebuildCheck,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<BpeProfile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BpeStatusReport {
    pub exists: bool,
    pub active: bool,
    pub auto_build_enabled: bool,
    pub profile_path: String,
    pub has_backup: bool,
    pub platform: String,
    pub scanned_homes: Vec<String>,
    pub rebuild_check: RebuildCheck,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<BpeProfileSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BpeProfileSummary {
    pub schema_version: u32,
    pub built_at_unix: u64,
    pub source: BpeSource,
    pub window_hours: f64,
    pub tokenizer: String,
    pub sessions_scanned: usize,
    pub messages_kept: usize,
    pub merge_count: usize,
    pub learned_filler_count: usize,
    pub learned_replacement_count: usize,
    pub estimated_token_savings_pct: f64,
    pub bpe_chars_per_token: f64,
    pub hangul_ratio: f64,
}

impl From<&BpeProfile> for BpeProfileSummary {
    fn from(p: &BpeProfile) -> Self {
        Self {
            schema_version: p.schema_version,
            built_at_unix: p.built_at_unix,
            source: p.source,
            window_hours: p.window_hours,
            tokenizer: p.tokenizer.clone(),
            sessions_scanned: p.stats.sessions_scanned,
            messages_kept: p.stats.messages_kept,
            merge_count: p.merges.len(),
            learned_filler_count: p.learned_fillers.len(),
            learned_replacement_count: p.learned_replacements.len(),
            estimated_token_savings_pct: p.stats.estimated_token_savings_pct,
            bpe_chars_per_token: p.stats.bpe_chars_per_token,
            hangul_ratio: p.stats.hangul_ratio,
        }
    }
}

struct CachedProfile {
    path: PathBuf,
    mtime_secs: u64,
    mtime_nanos: u32,
    len: u64,
    profile: Option<BpeProfile>,
}

static ACTIVE_PROFILE_CACHE: OnceLock<RwLock<Option<CachedProfile>>> = OnceLock::new();

fn env_bool(key: &str) -> Option<bool> {
    let raw = std::env::var(key).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "auto" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn env_f64(key: &str) -> Option<f64> {
    std::env::var(key).ok()?.trim().parse::<f64>().ok()
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok()?.trim().parse::<usize>().ok()
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn detect_platform_label() -> &'static str {
    #[cfg(windows)]
    {
        "windows"
    }
    #[cfg(target_os = "macos")]
    {
        "macos"
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if is_wsl_environment() {
            "wsl"
        } else {
            "linux"
        }
    }
}

#[cfg(unix)]
fn is_wsl_environment() -> bool {
    if std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some() {
        return true;
    }
    std::fs::read_to_string("/proc/version")
        .map(|v| {
            let lower = v.to_ascii_lowercase();
            lower.contains("microsoft") || lower.contains("wsl")
        })
        .unwrap_or(false)
}

#[cfg(not(unix))]
#[allow(dead_code)]
fn is_wsl_environment() -> bool {
    false
}

fn default_include_windows_homes() -> bool {
    #[cfg(windows)]
    {
        true
    }
    #[cfg(not(windows))]
    {
        env_bool("CCTX_SCAN_WINDOWS_HOMES").unwrap_or(false)
    }
}

pub fn resolve_state_dir(override_dir: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = override_dir {
        private_dir(dir)?;
        return Ok(dir.to_path_buf());
    }
    state_dir()
}

pub fn profile_path_in(dir: &Path) -> PathBuf {
    dir.join("bpe-profile.json")
}

pub fn prev_profile_path_in(dir: &Path) -> PathBuf {
    dir.join("bpe-profile.prev.json")
}

fn lock_path_in(dir: &Path) -> PathBuf {
    dir.join("bpe-profile.lock")
}

fn auto_state_path_in(dir: &Path) -> PathBuf {
    dir.join("bpe-auto-state.json")
}

fn profile_disabled_by_env() -> bool {
    if env_bool("CCTX_DISABLE_BPE_PROFILE") == Some(true) {
        return true;
    }
    if let Ok(val) = std::env::var("CCTX_BPE_PROFILE")
        && matches!(
            val.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "none" | "disabled"
        )
    {
        return true;
    }
    false
}

fn ensure_regular_non_symlink_file(
    path: &Path,
    require_owner_perms: bool,
) -> Result<Option<std::fs::Metadata>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if meta.file_type().is_symlink() {
        return Err(invalid(format!(
            "refusing to access symlinked file: {}",
            path.display()
        ))
        .into());
    }
    if !meta.file_type().is_file() {
        return Err(invalid(format!(
            "refusing to access non-regular file: {}",
            path.display()
        ))
        .into());
    }
    #[cfg(unix)]
    if require_owner_perms {
        use std::os::unix::fs::MetadataExt;
        let euid = unsafe { libc::geteuid() };
        if meta.uid() != euid {
            return Err(invalid(format!(
                "refusing to trust file not owned by current user: {}",
                path.display()
            ))
            .into());
        }
        if (meta.mode() & 0o022) != 0 {
            return Err(invalid(format!(
                "refusing to trust group/world-writable file: {}",
                path.display()
            ))
            .into());
        }
    }
    let _ = require_owner_perms;
    Ok(Some(meta))
}

fn read_bounded_regular_file(
    path: &Path,
    max_bytes: usize,
    require_owner_perms: bool,
) -> Result<Option<Vec<u8>>> {
    let Some(meta) = ensure_regular_non_symlink_file(path, require_owner_perms)? else {
        return Ok(None);
    };
    if meta.len() as usize > max_bytes {
        return Err(invalid(format!(
            "file {} exceeds {} byte limit",
            path.display(),
            max_bytes
        ))
        .into());
    }
    let file = File::open(path)?;
    let mut bytes = Vec::with_capacity((meta.len() as usize).min(max_bytes));
    file.take((max_bytes + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(invalid(format!(
            "file {} exceeds {} byte limit",
            path.display(),
            max_bytes
        ))
        .into());
    }
    Ok(Some(bytes))
}

fn resolve_runtime_profile_path() -> Option<PathBuf> {
    if profile_disabled_by_env() {
        return None;
    }
    if let Some(explicit) = std::env::var_os("CCTX_BPE_PROFILE_PATH")
        && !explicit.is_empty()
    {
        let p = PathBuf::from(explicit);
        if !p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir)) {
            return None;
        }
        ensure_regular_non_symlink_file(&p, true).ok().flatten()?;
        return Some(p);
    }
    // Keep unit/integration tests deterministic unless they explicitly opt into a state dir.
    #[cfg(test)]
    std::env::var_os("CCTX_HOME")?;
    state_dir().ok().map(|d| profile_path_in(&d))
}

/// Clears the in-process active profile cache (useful after writes, resets, or in tests).
pub fn invalidate_profile_cache() {
    if let Some(lock) = ACTIVE_PROFILE_CACHE.get()
        && let Ok(mut guard) = lock.write()
    {
        *guard = None;
    }
}

/// Loads the active learned BPE profile from disk with lightweight `mtime` + `len` caching.
/// Always fails open (`None`) on missing, oversized, symlinked, or corrupted profile files.
pub fn load_active_profile() -> Option<BpeProfile> {
    let path = resolve_runtime_profile_path()?;
    load_profile_cached(&path)
}

fn load_profile_cached(path: &Path) -> Option<BpeProfile> {
    let Ok(Some(meta)) = ensure_regular_non_symlink_file(path, true) else {
        invalidate_profile_cache();
        return None;
    };
    let len = meta.len();
    if len == 0 || len as usize > MAX_PROFILE_BYTES {
        invalidate_profile_cache();
        return None;
    }
    let (mtime_secs, mtime_nanos) = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or((0, 0), |d| (d.as_secs(), d.subsec_nanos()));

    let cache = ACTIVE_PROFILE_CACHE.get_or_init(|| RwLock::new(None));
    if let Ok(guard) = cache.read()
        && let Some(cached) = guard.as_ref()
        && cached.path == path
        && cached.mtime_secs == mtime_secs
        && cached.mtime_nanos == mtime_nanos
        && cached.len == len
    {
        return cached.profile.clone();
    }

    let loaded = read_profile_file(path).ok().flatten();
    if let Ok(mut guard) = cache.write() {
        *guard = Some(CachedProfile {
            path: path.to_path_buf(),
            mtime_secs,
            mtime_nanos,
            len,
            profile: loaded.clone(),
        });
    }
    loaded
}

pub fn read_profile_file(path: &Path) -> Result<Option<BpeProfile>> {
    let Some(bytes) = read_bounded_regular_file(path, MAX_PROFILE_BYTES, false)? else {
        return Ok(None);
    };
    let mut profile: BpeProfile = serde_json::from_slice(&bytes)?;
    if profile.schema_version != PROFILE_SCHEMA_VERSION {
        return Ok(None);
    }
    sanitize_loaded_profile(&mut profile);
    Ok(Some(profile))
}

/// Defense-in-depth validation on loaded profiles so tampered or corrupted files on disk
/// can never inject unsafe rewrites, secret tokens, cyclic replacements, or oversized tables.
fn sanitize_loaded_profile(profile: &mut BpeProfile) {
    let tok = profile.tokenizer_profile();
    profile.merges.retain(|m| {
        !m.left.is_empty()
            && !m.right.is_empty()
            && m.merged == format!("{}{}", m.left, m.right)
            && m.merged.chars().count() <= 64
    });
    profile.merges.truncate(MAX_MERGES_LIMIT);

    profile.learned_fillers.retain(|f| {
        let trimmed = f.trim();
        !trimmed.is_empty()
            && trimmed.chars().count() <= 64
            && !contains_protected_keyword(trimmed)
            && !contains_secret_like_token(trimmed)
            && !trimmed.contains('\n')
            && !trimmed.contains('\r')
            && !trimmed.contains('/')
            && !trimmed.contains('\\')
    });
    profile
        .learned_fillers
        .sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    profile.learned_fillers.dedup();
    profile.learned_fillers.truncate(MAX_LEARNED_FILLERS);

    let mut clean_rules: Vec<LearnedReplacement> = Vec::new();
    for rule in std::mem::take(&mut profile.learned_replacements) {
        if !is_safe_phrase(&rule.from)
            || rule.to.trim().is_empty()
            || rule.to.chars().count() > 96
            || rule.from == rule.to
            || contains_protected_keyword(&rule.from)
            || !preserves_protected_keywords(&rule.from, &rule.to)
            || contains_secret_like_token(&rule.to)
            || rule.to.contains('\n')
            || rule.to.contains('\r')
            || rule.to.chars().count() * 4 < rule.from.chars().count()
        {
            continue;
        }
        if estimate_bpe_tokens_for(&rule.from, tok) <= estimate_bpe_tokens_for(&rule.to, tok) {
            continue;
        }
        let cyclic_or_dup = clean_rules.iter().any(|existing| {
            existing.from == rule.from
                || existing.to.contains(&rule.from)
                || rule.to.contains(&existing.from)
        });
        if !cyclic_or_dup {
            clean_rules.push(rule);
            if clean_rules.len() >= MAX_LEARNED_REPLACEMENTS {
                break;
            }
        }
    }
    clean_rules.sort_by(|a, b| b.from.len().cmp(&a.from.len()).then_with(|| a.from.cmp(&b.from)));
    profile.learned_replacements = clean_rules;

    // Clamp auto-build policy fields so corrupted/tampered JSON cannot trigger rebuild storms.
    let p = &mut profile.auto_policy;
    if !p.min_interval_hours.is_finite() {
        p.min_interval_hours = 24.0;
    } else {
        p.min_interval_hours = p.min_interval_hours.clamp(0.5, 720.0);
    }
    p.min_new_sessions = p.min_new_sessions.clamp(1, 500);
    p.min_new_messages = p.min_new_messages.clamp(1, 5_000);
    if !p.drift_threshold.is_finite() {
        p.drift_threshold = 0.18;
    } else {
        p.drift_threshold = p.drift_threshold.clamp(0.05, 0.95);
    }
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("profile path missing parent"))?;
    private_dir(parent)?;
    // Refuse replacing an existing symlink target.
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        return Err(invalid(format!(
            "refusing to overwrite symlink target: {}",
            path.display()
        ))
        .into());
    }
    let mut tmp = tempfile::Builder::new()
        .prefix(".bpe-profile-")
        .tempfile_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.write_all(bytes)?;
    tmp.flush()?;
    // On Windows, transient antivirus/reader sharing violations can briefly fail `MoveFileExW`.
    // Retry a few times with short backoff using the same temp file before surfacing the error.
    let mut current = tmp;
    for attempt in 0..5u8 {
        match current.persist(path) {
            Ok(_) => return Ok(()),
            Err(err) => {
                let retryable = cfg!(windows)
                    && matches!(
                        err.error.kind(),
                        std::io::ErrorKind::PermissionDenied
                            | std::io::ErrorKind::AlreadyExists
                    );
                if retryable && attempt < 4 {
                    current = err.file;
                    std::thread::sleep(std::time::Duration::from_millis(15 * u64::from(attempt + 1)));
                    continue;
                }
                return Err(err.error.into());
            }
        }
    }
    Ok(())
}

fn with_profile_lock<T, F>(dir: &Path, exclusive: bool, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    private_dir(dir)?;
    let lock_path = lock_path_in(dir);
    if let Ok(meta) = std::fs::symlink_metadata(&lock_path)
        && (meta.file_type().is_symlink() || !meta.file_type().is_file())
    {
        return Err(invalid(format!(
            "refusing to lock symlink/non-regular file: {}",
            lock_path.display()
        ))
        .into());
    }
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
    if exclusive {
        lock_file.lock_exclusive()?;
    } else {
        lock_file.lock_shared()?;
    }
    let res = f();
    let _ = lock_file.unlock();
    res
}

fn read_auto_state(dir: &Path) -> AutoBuildState {
    let path = auto_state_path_in(dir);
    read_bounded_regular_file(&path, MAX_AUTO_STATE_BYTES, false)
        .ok()
        .flatten()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn sanitize_error_line(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(256)
        .collect()
}

fn write_auto_state(dir: &Path, state: &AutoBuildState) {
    let mut clean = state.clone();
    if let Some(ref msg) = clean.last_error {
        clean.last_error = Some(sanitize_error_line(msg));
    }
    let path = auto_state_path_in(dir);
    if let Ok(bytes) = serde_json::to_vec(&clean) {
        let _ = write_private_atomic(&path, &bytes);
    }
}

/// Discovers local user homes for Copilot (and optional Claude) session logs on
/// Linux, WSL, and native Windows.
pub fn discover_session_homes(include_windows_homes: bool) -> Vec<PathBuf> {
    let mut homes: Vec<PathBuf> = Vec::with_capacity(6);
    let push_unique = |list: &mut Vec<PathBuf>, p: PathBuf| {
        if !p.as_os_str().is_empty() && !list.contains(&p) {
            list.push(p);
        }
    };

    #[cfg(windows)]
    {
        if let Some(up) = std::env::var_os("USERPROFILE") {
            push_unique(&mut homes, PathBuf::from(up));
        }
        if let (Some(drive), Some(path)) =
            (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH"))
        {
            let mut combined = drive;
            combined.push(path);
            push_unique(&mut homes, PathBuf::from(combined));
        }
        for var in ["HOME", "XDG_STATE_HOME"] {
            if let Some(val) = std::env::var_os(var) {
                push_unique(&mut homes, PathBuf::from(val));
            }
        }
        for var in ["LOCALAPPDATA", "APPDATA"] {
            if let Some(val) = std::env::var_os(var) {
                let p = PathBuf::from(val);
                if p.join(".copilot").is_dir() || p.join(".claude").is_dir() {
                    push_unique(&mut homes, p);
                }
            }
        }
    }

    #[cfg(not(windows))]
    {
        for var in ["HOME", "USERPROFILE", "XDG_STATE_HOME"] {
            if let Some(val) = std::env::var_os(var) {
                push_unique(&mut homes, PathBuf::from(val));
            }
        }
        // On WSL, optionally include mounted Windows user profiles (`/mnt/c/Users/*`, `/mnt/d/Users/*`).
        if include_windows_homes {
            for mount_users in ["/mnt/c/Users", "/mnt/d/Users"] {
                if let Ok(users) = std::fs::read_dir(mount_users) {
                    for entry in users.flatten().take(MAX_WINDOWS_HOME_ENTRIES) {
                        let name = entry.file_name();
                        let name_str = name.to_string_lossy();
                        if matches!(
                            name_str.as_ref(),
                            "Public"
                                | "Default"
                                | "Default User"
                                | "All Users"
                                | "desktop.ini"
                                | "DefaultAppPool"
                        ) {
                            continue;
                        }
                        let path = entry.path();
                        if path.is_dir() {
                            push_unique(&mut homes, path);
                        }
                    }
                }
            }
        }
    }

    let _ = include_windows_homes;
    if homes.is_empty() {
        homes.push(PathBuf::from("."));
    }
    homes
}

/// Normalizes a Windows drive path (`C:\Users\alice\...`) or Windows WSL UNC path
/// (`\\wsl.localhost\Ubuntu\home\...` / `\\wsl$\Ubuntu\home\...`) to Linux mount syntax
/// when running inside Linux/WSL. Rejects path traversal (`..`) and control characters
/// by returning `None`.
pub fn normalize_cross_platform_dir_checked(path: &Path) -> Option<PathBuf> {
    let raw = path.to_string_lossy();
    if raw.contains('\0') || raw.chars().any(|c| c.is_control()) {
        return None;
    }
    let candidate = {
        #[cfg(not(windows))]
        {
            if let Some(s) = path.to_str() {
                let lower = s.to_ascii_lowercase();
                if let Some(after_unc) = lower
                    .strip_prefix(r"\\wsl.localhost\")
                    .or_else(|| lower.strip_prefix(r"\\wsl$\"))
                    .or_else(|| lower.strip_prefix("//wsl.localhost/"))
                    .or_else(|| lower.strip_prefix("//wsl$/"))
                {
                    let offset = s.len() - after_unc.len();
                    let rest_orig = &s[offset..];
                    let norm_rest = rest_orig.replace('\\', "/");
                    // Drop the `<distro>` component and map to `/<linux_path>`.
                    if let Some((_, linux_tail)) = norm_rest.split_once('/') {
                        PathBuf::from(format!("/{linux_tail}"))
                    } else {
                        PathBuf::from("/")
                    }
                } else {
                    let b = s.as_bytes();
                    if b.len() >= 3
                        && b[0].is_ascii_alphabetic()
                        && b[1] == b':'
                        && (b[2] == b'\\' || b[2] == b'/')
                    {
                        let drive = (b[0] as char).to_ascii_lowercase();
                        let rest = s[3..].replace('\\', "/");
                        PathBuf::from(format!("/mnt/{drive}/{rest}"))
                    } else {
                        path.to_path_buf()
                    }
                }
            } else {
                path.to_path_buf()
            }
        }
        #[cfg(windows)]
        {
            path.to_path_buf()
        }
    };
    if candidate
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return None;
    }
    Some(candidate)
}

/// Backward-compatible wrapper returning a normalized path (or original path on non-traversing input).
pub fn normalize_cross_platform_dir(path: &Path) -> PathBuf {
    normalize_cross_platform_dir_checked(path).unwrap_or_else(|| path.to_path_buf())
}

#[derive(Clone, Debug)]
struct SessionFileEntry {
    session_id: String,
    kind: SessionFileKind,
    mtime_secs: u64,
    path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionFileKind {
    CopilotEvents,
    ClaudeProject,
}

fn register_session_file(
    by_key: &mut BTreeMap<String, SessionFileEntry>,
    key: String,
    kind: SessionFileKind,
    path: PathBuf,
    cutoff_unix: u64,
) {
    let Ok(Some(meta)) = ensure_regular_non_symlink_file(&path, false) else {
        return;
    };
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    if cutoff_unix > 0 && mtime_secs > 0 && mtime_secs < cutoff_unix {
        return;
    }
    match by_key.get(&key) {
        Some(prev) if prev.mtime_secs >= mtime_secs => {}
        _ => {
            by_key.insert(
                key.clone(),
                SessionFileEntry {
                    session_id: key,
                    kind,
                    mtime_secs,
                    path,
                },
            );
        }
    }
}

fn is_jsonl_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
}

fn find_events_jsonl_in_dir(dir: &Path) -> Option<PathBuf> {
    let direct = dir.join("events.jsonl");
    if ensure_regular_non_symlink_file(&direct, false)
        .ok()
        .flatten()
        .is_some()
    {
        return Some(direct);
    }
    // Windows filesystems or exported archives may uppercase filenames (`EVENTS.JSONL`).
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten().take(32) {
            if entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case("events.jsonl")
            {
                let p = entry.path();
                if ensure_regular_non_symlink_file(&p, false)
                    .ok()
                    .flatten()
                    .is_some()
                {
                    return Some(p);
                }
            }
        }
    }
    None
}

fn scan_copilot_session_dir(
    by_key: &mut BTreeMap<String, SessionFileEntry>,
    dir: &Path,
    cutoff_unix: u64,
) {
    if let Some(direct) = find_events_jsonl_in_dir(dir) {
        let sid = dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "direct".to_string());
        register_session_file(
            by_key,
            format!("copilot:{sid}"),
            SessionFileKind::CopilotEvents,
            direct,
            cutoff_unix,
        );
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten().take(MAX_DIR_ENTRIES) {
            let p = entry.path();
            if let Ok(sm) = std::fs::symlink_metadata(&p)
                && sm.file_type().is_symlink()
            {
                continue;
            }
            if p.is_dir() {
                if let Some(events) = find_events_jsonl_in_dir(&p) {
                    let sid = entry.file_name().to_string_lossy().into_owned();
                    register_session_file(
                        by_key,
                        format!("copilot:{sid}"),
                        SessionFileKind::CopilotEvents,
                        events,
                        cutoff_unix,
                    );
                }
            } else if is_jsonl_path(&p) {
                let sid = entry.file_name().to_string_lossy().into_owned();
                register_session_file(
                    by_key,
                    format!("copilot:{sid}"),
                    SessionFileKind::CopilotEvents,
                    p,
                    cutoff_unix,
                );
            }
        }
    }
}

fn collect_session_files(
    homes: &[PathBuf],
    explicit_session_dirs: &[PathBuf],
    source: BpeSource,
    cutoff_unix: u64,
) -> Vec<SessionFileEntry> {
    let mut by_key: BTreeMap<String, SessionFileEntry> = BTreeMap::new();

    if !explicit_session_dirs.is_empty() {
        for dir in explicit_session_dirs {
            if let Some(norm) = normalize_cross_platform_dir_checked(dir) {
                scan_copilot_session_dir(&mut by_key, &norm, cutoff_unix);
            }
        }
    } else {
        for home in homes {
            let copilot_dir = home.join(".copilot").join("session-state");
            if copilot_dir.is_dir() {
                scan_copilot_session_dir(&mut by_key, &copilot_dir, cutoff_unix);
            }
            if source == BpeSource::All {
                let claude_dir = home.join(".claude").join("projects");
                if let Ok(projects) = std::fs::read_dir(&claude_dir) {
                    for proj in projects.flatten().take(MAX_DIR_ENTRIES) {
                        if let Ok(files) = std::fs::read_dir(proj.path()) {
                            for f in files.flatten().take(MAX_DIR_ENTRIES) {
                                let p = f.path();
                                if is_jsonl_path(&p) {
                                    let sid = format!(
                                        "claude:{}/{}",
                                        proj.file_name().to_string_lossy(),
                                        f.file_name().to_string_lossy()
                                    );
                                    register_session_file(
                                        &mut by_key,
                                        sid,
                                        SessionFileKind::ClaudeProject,
                                        p,
                                        cutoff_unix,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let mut files: Vec<SessionFileEntry> = by_key.into_values().collect();
    files.sort_by(|a, b| {
        b.mtime_secs
            .cmp(&a.mtime_secs)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    files.truncate(MAX_SESSION_FILES);
    files
}

/// Checks whether a rebuild is warranted using fast file-metadata watermarks before parsing JSONL.
fn check_rebuild_needed(
    existing: Option<&BpeProfile>,
    files: &[SessionFileEntry],
    policy: &AutoBuildPolicy,
    now_unix: u64,
    drift_score: Option<f64>,
) -> RebuildCheck {
    let total_sessions_in_window = files.len();
    let latest_event_mtime_secs = files.iter().map(|f| f.mtime_secs).max().unwrap_or(0);

    let Some(prev) = existing else {
        let needed = total_sessions_in_window >= policy.min_new_sessions.min(1);
        return RebuildCheck {
            needed,
            reason: if needed {
                "no existing bpe-profile.json; initial build needed".to_string()
            } else {
                "no session logs found in window".to_string()
            },
            changed_sessions: total_sessions_in_window,
            total_sessions_in_window,
            latest_event_mtime_secs,
            hours_since_last_build: None,
            drift_score,
        };
    };

    let elapsed_secs = now_unix.saturating_sub(prev.built_at_unix);
    let hours_since = elapsed_secs as f64 / 3600.0;
    let mtime_changed = files
        .iter()
        .filter(|f| f.mtime_secs > prev.watermark.latest_event_mtime_secs)
        .count();
    let count_growth = total_sessions_in_window.saturating_sub(prev.watermark.session_count);
    let changed_sessions = mtime_changed.max(count_growth);

    if let Some(drift) = drift_score
        && drift >= policy.drift_threshold
        && changed_sessions >= 1
    {
        return RebuildCheck {
            needed: true,
            reason: format!(
                "question-pattern drift {:.1}% >= threshold {:.1}% ({changed_sessions} new/updated sessions)",
                drift * 100.0,
                policy.drift_threshold * 100.0
            ),
            changed_sessions,
            total_sessions_in_window,
            latest_event_mtime_secs,
            hours_since_last_build: Some(round2(hours_since)),
            drift_score,
        };
    }

    if changed_sessions == 0 {
        return RebuildCheck {
            needed: false,
            reason: "no sessions modified since last build".to_string(),
            changed_sessions,
            total_sessions_in_window,
            latest_event_mtime_secs,
            hours_since_last_build: Some(round2(hours_since)),
            drift_score,
        };
    }

    if changed_sessions >= policy.min_new_sessions && hours_since >= policy.min_interval_hours {
        return RebuildCheck {
            needed: true,
            reason: format!(
                "{changed_sessions} sessions updated (>= {}) and {:.1}h elapsed (>= {:.1}h)",
                policy.min_new_sessions, hours_since, policy.min_interval_hours
            ),
            changed_sessions,
            total_sessions_in_window,
            latest_event_mtime_secs,
            hours_since_last_build: Some(round2(hours_since)),
            drift_score,
        };
    }

    // Burst activity override: 3x session threshold triggers early rebuild even inside cooldown.
    let burst_sessions = policy.min_new_sessions.saturating_mul(3).max(6);
    if changed_sessions >= burst_sessions {
        return RebuildCheck {
            needed: true,
            reason: format!(
                "high session activity: {changed_sessions} sessions updated (>= burst {burst_sessions})"
            ),
            changed_sessions,
            total_sessions_in_window,
            latest_event_mtime_secs,
            hours_since_last_build: Some(round2(hours_since)),
            drift_score,
        };
    }

    RebuildCheck {
        needed: false,
        reason: format!(
            "below threshold ({changed_sessions}/{} sessions, {:.1}/{:.1}h)",
            policy.min_new_sessions, hours_since, policy.min_interval_hours
        ),
        changed_sessions,
        total_sessions_in_window,
        latest_event_mtime_secs,
        hours_since_last_build: Some(round2(hours_since)),
        drift_score,
    }
}

#[derive(Clone, Debug)]
struct ExtractedMessage {
    session_id: String,
    text: String,
}

#[derive(Default)]
struct ExtractedCorpus {
    messages: Vec<ExtractedMessage>,
    sessions_with_messages: BTreeSet<String>,
    raw_messages_seen: usize,
    model_counts: BTreeMap<String, usize>,
}

fn extract_corpus(files: &[SessionFileEntry], cutoff_unix: u64) -> ExtractedCorpus {
    let mut out = ExtractedCorpus::default();
    let mut line = String::with_capacity(4096);

    for entry in files {
        if out.messages.len() >= MAX_MESSAGES_SCANNED {
            break;
        }
        let Ok(file) = File::open(&entry.path) else {
            continue;
        };
        let mut reader = BufReader::with_capacity(64 * 1024, file);
        let mut first_line = true;
        while let Ok(Some(valid_line)) =
            read_bounded_jsonl_line(&mut reader, &mut line, MAX_JSONL_LINE_BYTES)
        {
            if !valid_line {
                first_line = false;
                continue;
            }
            let raw_line = if first_line {
                first_line = false;
                line.trim_start_matches('\u{FEFF}')
            } else {
                line.as_str()
            };
            if raw_line.trim().is_empty() {
                continue;
            }
            match entry.kind {
                SessionFileKind::CopilotEvents => {
                    let is_user_msg = raw_line.contains("\"user.message\"");
                    let is_usage = raw_line.contains("\"session.usage_record\"")
                        || raw_line.contains("\"assistant.usage\"");
                    if !is_user_msg && !is_usage {
                        continue;
                    }
                    let Ok(ev) = serde_json::from_str::<Value>(raw_line) else {
                        continue;
                    };
                    let Some(ev_type) = ev.get("type").and_then(Value::as_str) else {
                        continue;
                    };
                    if let Some(ts) = parse_event_timestamp_unix(&ev)
                        && cutoff_unix > 0
                        && ts < cutoff_unix
                    {
                        continue;
                    }
                    if ev_type == "user.message" {
                        out.raw_messages_seen += 1;
                        let raw_text = ev
                            .pointer("/data/content")
                            .and_then(extract_text_content)
                            .or_else(|| {
                                ev.pointer("/data/transformedContent")
                                    .and_then(extract_text_content)
                            });
                        if let Some(raw) = raw_text
                            && let Some(clean) = sanitize_user_prompt(&raw)
                        {
                            out.sessions_with_messages.insert(entry.session_id.clone());
                            out.messages.push(ExtractedMessage {
                                session_id: entry.session_id.clone(),
                                text: clean,
                            });
                            if out.messages.len() >= MAX_MESSAGES_SCANNED {
                                break;
                            }
                        }
                    } else if (ev_type == "session.usage_record" || ev_type == "assistant.usage")
                        && let Some(model) = ev
                            .pointer("/data/usage/model")
                            .or_else(|| ev.pointer("/data/model"))
                            .and_then(Value::as_str)
                    {
                        let prof = TokenizerProfile::for_model(model).as_str().to_string();
                        *out.model_counts.entry(prof).or_insert(0) += 1;
                    }
                }
                SessionFileKind::ClaudeProject => {
                    if !raw_line.contains("\"user\"") && !raw_line.contains("\"model\"") {
                        continue;
                    }
                    let Ok(ev) = serde_json::from_str::<Value>(raw_line) else {
                        continue;
                    };
                    if ev.get("type").and_then(Value::as_str) == Some("user") {
                        let raw_text = ev
                            .pointer("/message/content")
                            .and_then(extract_text_content)
                            .or_else(|| ev.get("content").and_then(extract_text_content));
                        if let Some(raw) = raw_text {
                            out.raw_messages_seen += 1;
                            if let Some(clean) = sanitize_user_prompt(&raw) {
                                out.sessions_with_messages.insert(entry.session_id.clone());
                                out.messages.push(ExtractedMessage {
                                    session_id: entry.session_id.clone(),
                                    text: clean,
                                });
                                if out.messages.len() >= MAX_MESSAGES_SCANNED {
                                    break;
                                }
                            }
                        }
                    } else if let Some(model) = ev.pointer("/message/model").and_then(Value::as_str)
                    {
                        let prof = TokenizerProfile::for_model(model).as_str().to_string();
                        *out.model_counts.entry(prof).or_insert(0) += 1;
                    }
                }
            }
        }
    }
    out
}

/// Extracts plain text from either a JSON string or a content-part array
/// (`[{"type": "text", "text": "..."}]`).
fn extract_text_content(val: &Value) -> Option<String> {
    if let Some(s) = val.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = val.as_array() {
        let mut buf = String::new();
        for item in arr {
            let is_text = item
                .get("type")
                .and_then(Value::as_str)
                .is_none_or(|t| t == "text" || t == "input_text");
            if is_text
                && let Some(t) = item.get("text").and_then(Value::as_str)
            {
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(t);
            }
        }
        if !buf.is_empty() {
            return Some(buf);
        }
    }
    None
}

fn parse_event_timestamp_unix(ev: &Value) -> Option<u64> {
    let raw = ev.get("timestamp")?;
    if let Some(n) = raw.as_u64() {
        return Some(if n > 10_000_000_000 { n / 1000 } else { n });
    }
    if let Some(f) = raw.as_f64()
        && f.is_finite()
        && f > 0.0
    {
        return Some(if f > 10_000_000_000.0 {
            (f / 1000.0) as u64
        } else {
            f as u64
        });
    }
    let s = raw.as_str()?;
    parse_iso8601_unix(s)
}

fn parse_iso8601_unix(s: &str) -> Option<u64> {
    // Minimal RFC 3339 parser (`YYYY-MM-DD[T ]HH:MM:SS[...][Z|+HH:MM|+HHMM]`) without external crates.
    let bytes = s.as_bytes();
    if bytes.len() < 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: i64 = s.get(5..7)?.parse().ok()?;
    let day: i64 = s.get(8..10)?.parse().ok()?;
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let min: i64 = s.get(14..16)?.parse().ok()?;
    let sec_raw: i64 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&min)
        || !(0..=60).contains(&sec_raw)
    {
        return None;
    }
    let sec = sec_raw.min(59);
    // Howard Hinnant's civil_from_days inverse.
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let utc = days * 86_400 + hour * 3600 + min * 60 + sec;
    // Handle explicit timezone offset (`+HH:MM`, `-HH:MM`, `+HHMM`, `-HHMM`) if present.
    let tail = &s[19..];
    let mut offset_secs: i64 = 0;
    if let Some(pos) = tail.rfind(['+', '-']) {
        let sign = if tail.as_bytes()[pos] == b'+' { 1 } else { -1 };
        let tz = &tail[pos + 1..];
        let parsed = if tz.len() >= 5 && tz.as_bytes().get(2) == Some(&b':') {
            tz[0..2]
                .parse::<i64>()
                .ok()
                .zip(tz[3..5].parse::<i64>().ok())
        } else if tz.len() >= 4 && tz[0..4].bytes().all(|b| b.is_ascii_digit()) {
            tz[0..2]
                .parse::<i64>()
                .ok()
                .zip(tz[2..4].parse::<i64>().ok())
        } else {
            None
        };
        if let Some((oh, om)) = parsed
            && oh <= 23
            && om <= 59
        {
            offset_secs = sign * (oh * 3600 + om * 60);
        }
    }
    let adjusted = utc - offset_secs;
    u64::try_from(adjusted).ok()
}

/// Strips XML wrapper blocks, fenced code blocks, secrets, paths, and excessive noise
/// so learned rules never leak sensitive data or overfit to file paths/IDs.
pub fn sanitize_user_prompt(raw: &str) -> Option<String> {
    let mut text = strip_xml_blocks(
        raw,
        &[
            "system_reminder",
            "system-reminder",
            "attachment",
            "attachments",
            "context",
            "file_content",
            "environment_context",
            "workspace_info",
            "current_datetime",
            "mode_changed_notice",
            "system_notification",
        ],
    );
    text = strip_fenced_code_blocks(&text);
    // Reject obvious raw secret dumps or machine-generated JSON blobs.
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.starts_with('{') || trimmed.starts_with('[') {
        return None;
    }
    if contains_secret_like_token(trimmed) {
        return None;
    }

    let mut kept_tokens: Vec<String> = Vec::new();
    for tok in trimmed.split_whitespace() {
        if is_noisy_token(tok) {
            continue;
        }
        kept_tokens.push(tok.to_string());
    }
    let joined = kept_tokens.join(" ");
    let clipped: String = joined.chars().take(MAX_MESSAGE_CHARS).collect();
    let final_str = clipped.trim();
    if final_str.chars().count() < 6 {
        return None;
    }
    Some(final_str.to_string())
}

fn strip_xml_blocks(input: &str, tags: &[&str]) -> String {
    // Pre-clip to MAX_XML_SCAN_BYTES on a valid UTF-8 boundary to avoid O(N^2) adversarial payloads.
    let mut clipped_end = input.len().min(MAX_XML_SCAN_BYTES);
    while clipped_end > 0 && !input.is_char_boundary(clipped_end) {
        clipped_end -= 1;
    }
    let slice = &input[..clipped_end];
    let lower = slice.to_ascii_lowercase();
    let mut out = String::with_capacity(slice.len());
    let mut cursor = 0usize;

    while cursor < slice.len() {
        let Some(rel_lt) = lower[cursor..].find('<') else {
            out.push_str(&slice[cursor..]);
            break;
        };
        let tag_start = cursor + rel_lt;
        let rest_lower = &lower[tag_start + 1..];
        let matched_tag = tags.iter().copied().find(|tag| {
            rest_lower.starts_with(tag)
                && rest_lower[tag.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c == '>' || c.is_whitespace() || c == '/')
        });
        if let Some(tag) = matched_tag {
            out.push_str(&slice[cursor..tag_start]);
            out.push(' ');
            let close_pat = format!("</{tag}>");
            if let Some(rel_close) = lower[tag_start..].find(&close_pat) {
                cursor = tag_start + rel_close + close_pat.len();
            } else if let Some(rel_nl) = lower[tag_start..].find('\n') {
                cursor = tag_start + rel_nl + 1;
            } else {
                cursor = slice.len();
            }
        } else {
            out.push_str(&slice[cursor..=tag_start]);
            cursor = tag_start + 1;
        }
    }
    out
}

fn strip_fenced_code_blocks(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_fence = false;
    for line in input.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Detects secrets, cloud API keys, database credentials, JWTs, or PII patterns.
/// Shared by both prompt ingestion sanitization and the built-in self-audit engine.
pub fn detect_sensitive_patterns(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    let markers: &[(&str, &str)] = &[
        ("ghp_", "github_token"),
        ("gho_", "github_token"),
        ("ghu_", "github_token"),
        ("ghs_", "github_token"),
        ("github_pat_", "github_pat"),
        ("sk-proj-", "openai_key"),
        ("sk-ant-", "anthropic_key"),
        ("sk-live-", "stripe_key"),
        ("sk_live_", "stripe_key"),
        ("rk_live_", "stripe_key"),
        ("pk_live_", "stripe_key"),
        ("xoxb-", "slack_token"),
        ("xoxp-", "slack_token"),
        ("xoxa-", "slack_token"),
        ("xoxr-", "slack_token"),
        ("aiza", "gcp_api_key"),
        ("akia", "aws_access_key"),
        ("asia", "aws_sts_key"),
        ("-----begin private key", "private_key"),
        ("-----begin rsa private key", "private_key"),
        ("-----begin openssh private key", "private_key"),
        ("authorization: bearer ", "bearer_token"),
        ("bearer eyj", "bearer_jwt"),
        ("password=", "credential_kv"),
        ("passwd=", "credential_kv"),
        ("api_key=", "credential_kv"),
        ("apikey=", "credential_kv"),
        ("access_key=", "credential_kv"),
        ("client_secret=", "credential_kv"),
        ("secret_key=", "credential_kv"),
        ("auth_token=", "credential_kv"),
        ("postgres://", "db_uri"),
        ("postgresql://", "db_uri"),
        ("mongodb://", "db_uri"),
        ("mongodb+srv://", "db_uri"),
        ("mysql://", "db_uri"),
        ("redis://", "db_uri"),
    ];
    for &(marker, label) in markers {
        if lower.contains(marker) {
            // Avoid false positives on short words like "asia" unless followed by uppercase/digits (AWS STS `ASIA...`).
            if matches!(marker, "asia" | "akia" | "aiza") {
                let has_key_shape = text.split_whitespace().any(|tok| {
                    let t = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric());
                    t.len() >= 16
                        && (t.starts_with("AKIA") || t.starts_with("ASIA") || t.starts_with("AIza"))
                        && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                });
                if !has_key_shape {
                    continue;
                }
            }
            return Some(label);
        }
    }
    if lower.contains("sig=") && lower.contains("&se=") {
        return Some("azure_sas_token");
    }
    // Detect JWT-like 3-part base64url tokens starting with `eyJ`.
    if text.split_whitespace().any(|tok| {
        let t = tok.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';'));
        t.starts_with("eyJ") && t.len() >= 36 && t.matches('.').count() == 2
    }) {
        return Some("jwt_token");
    }
    // Detect Korean resident registration number (`dddddd-ddddddd`) or mobile phone (`010-dddd-dddd`).
    if text.split_whitespace().any(|tok| {
        let t = tok.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';' | '(' | ')'));
        let parts: Vec<&str> = t.split('-').collect();
        (parts.len() == 2
            && parts[0].len() == 6
            && parts[1].len() == 7
            && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit())))
            || (parts.len() == 3
                && parts[0] == "010"
                && parts[1].len() == 4
                && parts[2].len() == 4
                && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit())))
    }) {
        return Some("pii_identifier");
    }
    None
}

fn contains_secret_like_token(text: &str) -> bool {
    detect_sensitive_patterns(text).is_some()
}

fn is_noisy_token(tok: &str) -> bool {
    let clean = tok.trim_matches(|c: char| matches!(c, '"' | '\'' | '(' | ')' | '[' | ']' | ',' | ';'));
    if clean.is_empty() {
        return true;
    }
    let lower = clean.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("file://")
        || lower.starts_with("\\\\")
        || clean.starts_with('/')
        || clean.starts_with("~/")
        || clean.starts_with("./")
        || clean.starts_with("../")
    {
        return true;
    }
    // Windows drive paths like `C:\Users\...` or `D:/dev/...`
    let b = clean.as_bytes();
    if b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'\\' || b[2] == b'/')
    {
        return true;
    }
    // Path-like tokens containing slashes or backslashes
    if clean.contains('/') || clean.contains('\\') {
        return true;
    }
    // Email addresses
    if clean.contains('@') && clean.contains('.') {
        return true;
    }
    // Long hex digests or UUIDs
    if clean.len() >= 12
        && clean
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-')
        && clean.chars().filter(|c| c.is_ascii_hexdigit()).count() >= 10
    {
        return true;
    }
    // Long opaque tokens (base64 / JWT / secrets)
    if clean.len() >= 36
        && clean
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '=' | '+'))
    {
        return true;
    }
    false
}

/// Trains a deterministic character-level Byte-Pair Encoding (BPE) merge table on the corpus.
pub fn train_corpus_bpe(messages: &[String], max_merges: usize, min_pair_freq: usize) -> Vec<BpeMergeRule> {
    let cap = max_merges.min(MAX_MERGES_LIMIT);
    if cap == 0 || messages.is_empty() {
        return Vec::new();
    }
    let min_freq = min_pair_freq.max(2);

    let mut word_counts: HashMap<String, usize> = HashMap::new();
    for msg in messages {
        for raw_word in msg.split_whitespace() {
            let trimmed = raw_word.trim_matches(|c: char| {
                matches!(c, '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | '.' | '?' | '!' | ':' | ';')
            });
            if trimmed.chars().count() < 2 || trimmed.chars().count() > 48 {
                continue;
            }
            *word_counts.entry(trimmed.to_string()).or_insert(0) += 1;
        }
    }

    if word_counts.is_empty() {
        return Vec::new();
    }

    // Represent each unique word as a sequence of symbols plus end-of-word marker `</w>`.
    // Bound vocabulary to top MAX_BPE_VOCAB_WORDS by frequency so worst-case training stays <25ms.
    let mut vocab: Vec<(Vec<String>, usize)> = word_counts
        .into_iter()
        .map(|(word, freq)| {
            let mut syms: Vec<String> = word.chars().map(|c| c.to_string()).collect();
            syms.push("</w>".to_string());
            (syms, freq)
        })
        .collect();
    vocab.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    vocab.truncate(MAX_BPE_VOCAB_WORDS);

    let mut merges: Vec<BpeMergeRule> = Vec::with_capacity(cap);
    for _ in 0..cap {
        let mut pair_counts: HashMap<(&str, &str), usize> = HashMap::new();
        for (syms, freq) in &vocab {
            if syms.len() < 2 {
                continue;
            }
            for win in syms.windows(2) {
                *pair_counts.entry((win[0].as_str(), win[1].as_str())).or_insert(0) += *freq;
            }
        }
        let best = pair_counts
            .into_iter()
            .filter(|(_, count)| *count >= min_freq)
            .max_by(|(p1, c1), (p2, c2)| c1.cmp(c2).then_with(|| p2.cmp(p1)));

        let Some(((left, right), frequency)) = best else {
            break;
        };
        let left_s = left.to_string();
        let right_s = right.to_string();
        let merged = format!("{left_s}{right_s}");

        for (syms, _) in &mut vocab {
            if syms.len() < 2 {
                continue;
            }
            let mut i = 0;
            let mut next: Vec<String> = Vec::with_capacity(syms.len());
            while i < syms.len() {
                if i + 1 < syms.len() && syms[i] == left_s && syms[i + 1] == right_s {
                    next.push(merged.clone());
                    i += 2;
                } else {
                    next.push(syms[i].clone());
                    i += 1;
                }
            }
            *syms = next;
        }

        merges.push(BpeMergeRule {
            left: left_s,
            right: right_s,
            merged,
            frequency,
        });
    }

    merges
}

/// Reusable BPE encoder built once per merge table to avoid rebuilding rank maps per message.
pub struct BpeEncoder<'a> {
    rank: HashMap<(&'a str, &'a str), usize>,
}

impl<'a> BpeEncoder<'a> {
    pub fn new(merges: &'a [BpeMergeRule]) -> Self {
        let rank = merges
            .iter()
            .enumerate()
            .map(|(idx, m)| ((m.left.as_str(), m.right.as_str()), idx))
            .collect();
        Self { rank }
    }

    pub fn token_count(&self, text: &str) -> usize {
        if text.trim().is_empty() {
            return 0;
        }
        let mut total_tokens = 0usize;
        for raw_word in text.split_whitespace() {
            let mut syms: Vec<String> = raw_word.chars().take(64).map(|c| c.to_string()).collect();
            if syms.is_empty() {
                continue;
            }
            syms.push("</w>".to_string());
            if !self.rank.is_empty() {
                loop {
                    let mut best_rank = usize::MAX;
                    let mut best_pos = None;
                    for i in 0..syms.len().saturating_sub(1) {
                        if let Some(&r) = self.rank.get(&(syms[i].as_str(), syms[i + 1].as_str()))
                            && r < best_rank
                        {
                            best_rank = r;
                            best_pos = Some(i);
                        }
                    }
                    let Some(pos) = best_pos else {
                        break;
                    };
                    let left = syms[pos].clone();
                    let right = syms[pos + 1].clone();
                    let merged = format!("{left}{right}");
                    let mut next = Vec::with_capacity(syms.len());
                    let mut i = 0;
                    while i < syms.len() {
                        if i + 1 < syms.len() && syms[i] == left && syms[i + 1] == right {
                            next.push(merged.clone());
                            i += 2;
                        } else {
                            next.push(syms[i].clone());
                            i += 1;
                        }
                    }
                    syms = next;
                }
            }
            total_tokens += syms.len();
        }
        total_tokens
    }
}

/// Encodes text using a learned BPE merge table and returns the resulting token count.
pub fn encode_bpe_token_count(merges: &[BpeMergeRule], text: &str) -> usize {
    BpeEncoder::new(merges).token_count(text)
}

/// Measures relative BPE compression drift between a saved profile's baseline `chars_per_token`
/// and a sample of newly arrived user messages. Returns `Some(drift)` in `[0.0, 1.0]`.
pub fn measure_bpe_drift(profile: &BpeProfile, recent_messages: &[String]) -> Option<f64> {
    if profile.merges.is_empty()
        || profile.stats.bpe_chars_per_token <= 0.1
        || recent_messages.is_empty()
    {
        return None;
    }
    let encoder = BpeEncoder::new(&profile.merges);
    let mut total_chars = 0usize;
    let mut total_tokens = 0usize;
    for msg in recent_messages.iter().take(120) {
        total_chars += msg.chars().count();
        total_tokens += encoder.token_count(msg);
    }
    if total_tokens == 0 {
        return None;
    }
    let current_cpt = total_chars as f64 / total_tokens as f64;
    let baseline = profile.stats.bpe_chars_per_token;
    if current_cpt >= baseline {
        return Some(0.0);
    }
    Some(((baseline - current_cpt) / baseline).clamp(0.0, 1.0))
}

const PROTECTED_KEYWORDS: &[&str] = &[
    "하지 마",
    "하지마",
    "수정하지",
    "삭제하지",
    "변경하지",
    "건드리지",
    "읽기 전용",
    "읽기전용",
    "말고",
    "없이",
    "제외",
    "않고",
    "않도록",
    "금지",
    "중단",
    "취소",
    "롤백",
    "do not",
    "don't",
    "never",
    "without",
    "read-only",
    "read only",
    "readonly",
    "must not",
    "exclude",
    "forbid",
    "only",
    "prod",
    "production",
    "dry-run",
    "dry run",
    "rollback",
];

fn contains_protected_keyword(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    PROTECTED_KEYWORDS
        .iter()
        .any(|kw| lower.contains(kw) || text.contains(kw))
}

pub(crate) fn preserves_protected_keywords(from: &str, to: &str) -> bool {
    let from_lower = from.to_ascii_lowercase();
    let to_lower = to.to_ascii_lowercase();
    for kw in PROTECTED_KEYWORDS {
        if (from_lower.contains(kw) || from.contains(kw))
            && !(to_lower.contains(kw) || to.contains(kw))
        {
            return false;
        }
    }
    true
}

fn is_safe_phrase(phrase: &str) -> bool {
    let trimmed = phrase.trim();
    let char_len = trimmed.chars().count();
    if !(4..=96).contains(&char_len) {
        return false;
    }
    if contains_secret_like_token(trimmed) {
        return false;
    }
    if trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('@')
        || trimmed.contains('`')
        || trimmed.contains('\n')
        || trimmed.contains('\r')
        || trimmed.contains('#')
    {
        return false;
    }
    // Reject phrases containing digits (likely issue numbers, versions, dates, or counts).
    if trimmed.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }
    true
}

/// Mines safe, multi-session repeated filler phrases and workflow compression rules
/// from real user prompts.
fn mine_formula_rules(
    messages: &[ExtractedMessage],
    min_freq: usize,
    min_sessions: usize,
    min_savings_tokens: usize,
    tokenizer: TokenizerProfile,
) -> (Vec<String>, Vec<LearnedReplacement>) {
    let req_freq = min_freq.max(2);
    let req_sessions = min_sessions.max(1);
    let req_savings = min_savings_tokens.max(1);

    // 1. Mine safe recurring filler prefixes/suffixes (politeness/hedging phrases only).
    let candidate_fillers = [
        "혹시 ",
        "잠깐 ",
        "한번만 ",
        "한번 ",
        "혹시라도 ",
        "가능하면 ",
        "가능하다면 ",
        "괜찮다면 ",
        "일단은 ",
        "일단 ",
        "부탁드립니다",
        "부탁해요",
        "부탁해",
        "감사합니다",
        "고마워요",
        "고마워",
        "알려줄 수 있을까",
        "알려줄수 있을까",
        "확인해 줄 수 있을까",
        "확인해줄 수 있을까",
        "please check and ",
        "can you take a look at ",
        "could you take a look at ",
        "i want you to ",
        "i need you to ",
        "just wondering if ",
        "if possible, ",
        "if you can, ",
        "thank you so much",
        "thanks in advance",
    ];
    let mut learned_fillers: Vec<String> = Vec::new();
    for filler in candidate_fillers {
        if contains_protected_keyword(filler) {
            continue;
        }
        let filler_lower = filler.to_ascii_lowercase();
        let mut freq = 0usize;
        let mut sessions: BTreeSet<&str> = BTreeSet::new();
        for msg in messages {
            let lower = msg.text.to_ascii_lowercase();
            let count = lower.matches(&filler_lower).count();
            if count > 0 {
                freq += count;
                sessions.insert(msg.session_id.as_str());
            }
        }
        if freq >= req_freq
            && sessions.len() >= req_sessions
            && estimate_bpe_tokens_for(filler, tokenizer) >= req_savings
        {
            learned_fillers.push(filler.to_string());
        }
    }
    learned_fillers.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    learned_fillers.truncate(MAX_LEARNED_FILLERS);

    // 2. Mine recurring 2..=6 word n-grams across sessions directly into minimal transformed spans
    // so non-transformable 1-off n-grams never consume memory or evict real recurring patterns.
    let mut minimal_rules: HashMap<(String, String), (usize, BTreeSet<String>)> = HashMap::new();
    for msg in messages {
        let (norm, _, _) = normalize_bpe_operators(&msg.text);
        let words: Vec<&str> = norm
            .split_whitespace()
            .take(MAX_TOKENS_PER_MSG)
            .map(|w| w.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | '.' | '?' | '!')))
            .filter(|w| !w.is_empty())
            .collect();
        if words.len() < 2 {
            continue;
        }
        let mut seen_in_msg: BTreeSet<(String, String)> = BTreeSet::new();
        for n in 2..=6.min(words.len()) {
            for win in words.windows(n) {
                let phrase = win.join(" ");
                if !is_safe_phrase(&phrase) || contains_protected_keyword(&phrase) {
                    continue;
                }
                let Some(compact) = synthesize_compact_formula(&phrase) else {
                    continue;
                };
                let (min_from, min_to) = trim_common_word_affixes(&phrase, &compact);
                if min_from.is_empty()
                    || min_to.is_empty()
                    || min_from == min_to
                    || !is_safe_phrase(&min_from)
                    || contains_protected_keyword(&min_from)
                    || !preserves_protected_keywords(&min_from, &min_to)
                {
                    continue;
                }
                let key = (min_from, min_to);
                if !seen_in_msg.insert(key.clone()) {
                    continue;
                }
                if !minimal_rules.contains_key(&key) && minimal_rules.len() >= MAX_NGRAM_ENTRIES {
                    // Evict single-occurrence cold entries before capping to preserve heavy hitters.
                    minimal_rules.retain(|_, (cnt, _)| *cnt > 1);
                    if minimal_rules.len() >= MAX_NGRAM_ENTRIES {
                        continue;
                    }
                }
                let entry = minimal_rules
                    .entry(key)
                    .or_insert_with(|| (0, BTreeSet::new()));
                entry.0 += 1;
                entry.1.insert(msg.session_id.clone());
            }
        }
    }

    let mut candidates: Vec<LearnedReplacement> = Vec::new();
    for ((min_from, min_to), (freq, sessions)) in minimal_rules {
        if freq < req_freq || sessions.len() < req_sessions {
            continue;
        }
        let raw_tok = estimate_bpe_tokens_for(&min_from, tokenizer);
        let builtin = crate::prompt_formula::clause_to_formula_builtin(&min_from);
        let builtin_tok = estimate_bpe_tokens_for(&builtin, tokenizer).min(raw_tok);
        let comp_tok = estimate_bpe_tokens_for(&min_to, tokenizer);
        // Require net-new savings beyond what built-in formula rules already achieve.
        if builtin_tok <= comp_tok {
            continue;
        }
        let saved = builtin_tok - comp_tok;
        if saved < req_savings {
            continue;
        }
        // Reject overly aggressive truncation that discards too much content.
        if min_to.chars().count() * 4 < min_from.chars().count() {
            continue;
        }
        candidates.push(LearnedReplacement {
            from: min_from,
            to: min_to,
            frequency: freq,
            session_count: sessions.len(),
            tokens_saved: saved,
        });
    }

    // Rank by total impact (`frequency * tokens_saved`), then deduplicate overlapping sub-phrases.
    candidates.sort_by(|a, b| {
        (b.frequency * b.tokens_saved)
            .cmp(&(a.frequency * a.tokens_saved))
            .then_with(|| a.from.len().cmp(&b.from.len()))
            .then_with(|| a.from.cmp(&b.from))
    });

    let mut selected: Vec<LearnedReplacement> = Vec::new();
    for cand in candidates {
        if selected.len() >= MAX_LEARNED_REPLACEMENTS {
            break;
        }
        // Skip redundant sub-phrases, high-overlap shingles, or rules whose output would
        // trigger another selected rule cyclically.
        let overlaps = selected.iter().any(|existing| {
            existing.from.contains(&cand.from)
                || cand.from.contains(&existing.from)
                || existing.to.contains(&cand.from)
                || cand.to.contains(&existing.from)
                || word_jaccard_overlap(&existing.from, &cand.from) >= 0.5
        });
        if !overlaps {
            selected.push(cand);
        }
    }

    // Longest-first application order at runtime prevents partial-match interference.
    selected.sort_by(|a, b| b.from.len().cmp(&a.from.len()).then_with(|| a.from.cmp(&b.from)));
    (learned_fillers, selected)
}

/// Strips identical leading and trailing words shared by `from` and `to` so a rule targets
/// only the minimal span that actually changed, preventing sliding-window shingle duplicates.
fn trim_common_word_affixes(from: &str, to: &str) -> (String, String) {
    let fw: Vec<&str> = from.split_whitespace().collect();
    let tw: Vec<&str> = to.split_whitespace().collect();
    let mut start = 0usize;
    while start < fw.len() && start < tw.len() && fw[start] == tw[start] {
        start += 1;
    }
    let mut f_end = fw.len();
    let mut t_end = tw.len();
    while f_end > start && t_end > start && fw[f_end - 1] == tw[t_end - 1] {
        f_end -= 1;
        t_end -= 1;
    }
    (fw[start..f_end].join(" "), tw[start..t_end].join(" "))
}

fn word_jaccard_overlap(a: &str, b: &str) -> f64 {
    let sa: BTreeSet<&str> = a.split_whitespace().collect();
    let sb: BTreeSet<&str> = b.split_whitespace().collect();
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let inter = sa.intersection(&sb).count() as f64;
    let union = sa.union(&sb).count() as f64;
    inter / union
}

/// Deterministically compresses a repeated natural-language workflow phrase into a shorter
/// BPE-aligned formula expression using single-token ASCII operators (`->`, `&`, `=>`).
/// Returns `None` if no safe, high-confidence compression applies.
fn synthesize_compact_formula(phrase: &str) -> Option<String> {
    let mut s = phrase.trim().to_string();
    let original = s.clone();

    if contains_hangul(&s) {
        let ko_transforms = [
            ("확인해 볼 수 있을까", "확인"),
            ("확인해볼 수 있을까", "확인"),
            ("확인해 볼래", "확인"),
            ("확인해봐", "확인"),
            ("확인해서 ", "확인 -> "),
            ("확인하고 ", "확인 -> "),
            ("검토해서 ", "검토 -> "),
            ("검토하고 ", "검토 -> "),
            ("분석해서 ", "분석 -> "),
            ("분석하고 ", "분석 -> "),
            ("조사해서 ", "조사 -> "),
            ("조사하고 ", "조사 -> "),
            ("정리해서 ", "정리 -> "),
            ("정리하고 ", "정리 -> "),
            ("수정해서 ", "수정 -> "),
            ("수정하고 ", "수정 -> "),
            ("개선해서 ", "개선 -> "),
            ("개선하고 ", "개선 -> "),
            ("보강해서 ", "보강 -> "),
            ("보강하고 ", "보강 -> "),
            ("적용해서 ", "적용 -> "),
            ("적용하고 ", "적용 -> "),
            ("구현해서 ", "구현 -> "),
            ("구현하고 ", "구현 -> "),
            ("테스트해서 ", "테스트 -> "),
            ("테스트하고 ", "테스트 -> "),
            ("검증해서 ", "검증 -> "),
            ("검증하고 ", "검증 -> "),
            ("비평해서 ", "비평 -> "),
            ("비평하고 ", "비평 -> "),
            ("조치해서 ", "조치 -> "),
            ("조치하고 ", "조치 -> "),
            ("완료되면 ", "완료 -> "),
            ("성공할때 까지 ", "until 성공 -> "),
            ("성공할 때까지 ", "until 성공 -> "),
            ("남을때 까지 ", "until 잔여 -> "),
            ("남을 때까지 ", "until 잔여 -> "),
            ("진행한다음 ", "진행 -> "),
            ("진행한 다음 ", "진행 -> "),
            ("보고해 줄래", "보고"),
            ("보고해줄래", "보고"),
            ("보고해 줘", "보고"),
            ("보고해줘", "보고"),
            ("정리해 줄래", "정리"),
            ("정리해줄래", "정리"),
            ("정리해 줘", "정리"),
            ("정리해줘", "정리"),
            ("정리해봐", "정리"),
            ("수정해 줘", "수정"),
            ("수정해줘", "수정"),
            ("개선해 줘", "개선"),
            ("개선해줘", "개선"),
            ("검토해 줘", "검토"),
            ("검토해줘", "검토"),
            ("분석해 줘", "분석"),
            ("분석해줘", "분석"),
            ("조사해 줘", "조사"),
            ("조사해줘", "조사"),
            ("보강해 줘", "보강"),
            ("보강해줘", "보강"),
            ("구현해 줘", "구현"),
            ("구현해줘", "구현"),
            ("테스트해 줘", "테스트"),
            ("테스트해줘", "테스트"),
            ("알려줄래", "보고"),
            ("알려줘", "보고"),
            ("진행해줘", "진행"),
            ("진행하자", "진행"),
            ("진행해", "진행"),
            ("해줄 수 있어", ""),
            ("할 수 있을까", ""),
            ("할수 있을까", ""),
            ("해볼 수 있을까", ""),
            ("해볼수 있을까", ""),
            ("하도록 하자", ""),
            ("하도록 해", ""),
            ("뿐만 아니라 ", "& "),
            ("뿐만아니라 ", "& "),
            ("에 대해서 ", " "),
            ("에 대한 ", " "),
            ("를 기준으로 ", " 기준 "),
            ("을 기준으로 ", " 기준 "),
            ("위주로 ", "중심 "),
            ("전반적으로 ", ""),
        ];
        for (from, to) in ko_transforms {
            if s.contains(from) {
                s = s.replace(from, to);
            }
        }
    } else {
        let en_transforms = [
            (" and then ", " -> "),
            (" and also ", " & "),
            (" as well as ", " & "),
            (" in order to ", " -> "),
            (" make sure that ", " ensure "),
            (" make sure ", " ensure "),
            (" check whether ", " check "),
            (" check if ", " check "),
            (" let me know ", " report "),
            (" tell me ", " report "),
            (" figure out ", " find "),
            (" take a look at ", " inspect "),
            (" look into ", " inspect "),
            (" go ahead and ", " "),
            (" please ", " "),
            (" could you ", " "),
            (" can you ", " "),
        ];
        let mut lower = s.to_ascii_lowercase();
        let mut matched_any = false;
        for (from, to) in en_transforms {
            if lower.contains(from) {
                lower = lower.replace(from, to);
                matched_any = true;
            }
        }
        if matched_any {
            s = lower;
        }
    }

    let mut collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    while collapsed.contains("-> ->") {
        collapsed = collapsed.replace("-> ->", "->");
    }
    while collapsed.contains("& &") {
        collapsed = collapsed.replace("& &", "&");
    }
    collapsed = collapsed
        .trim()
        .trim_start_matches("->")
        .trim_end_matches("->")
        .trim_start_matches('&')
        .trim_end_matches('&')
        .trim()
        .to_string();

    if collapsed.is_empty() || collapsed == original {
        None
    } else {
        Some(collapsed)
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn compute_corpus_fingerprint(messages: &[ExtractedMessage], latest_mtime: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(latest_mtime.to_le_bytes());
    hasher.update((messages.len() as u64).to_le_bytes());
    for msg in messages.iter().take(256) {
        hasher.update(msg.session_id.as_bytes());
        hasher.update(b":");
        hasher.update(msg.text.as_bytes());
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(24);
    for b in &digest[..12] {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

/// Inspects current BPE profile status and whether an automatic rebuild is warranted.
pub fn inspect_bpe_status(config: &BpeBuildConfig) -> Result<BpeStatusReport> {
    let state_dir = resolve_state_dir(config.state_dir_override.as_deref())?;
    let path = profile_path_in(&state_dir);
    let prev_path = prev_profile_path_in(&state_dir);
    let now = config.now_unix.unwrap_or_else(now_unix_secs);
    let window_secs = (config.window_hours.max(1.0) * 3600.0) as u64;
    let cutoff = now.saturating_sub(window_secs);

    let homes = if config.session_dirs.is_empty() {
        discover_session_homes(config.include_windows_homes)
    } else {
        config.session_dirs.clone()
    };
    let files = collect_session_files(&homes, &config.session_dirs, config.source, cutoff);
    let existing = read_profile_file(&path).ok().flatten();
    let policy = existing
        .as_ref()
        .map(|p| p.auto_policy.clone())
        .unwrap_or_default();
    let rebuild_check = check_rebuild_needed(existing.as_ref(), &files, &policy, now, None);

    Ok(BpeStatusReport {
        exists: existing.is_some(),
        active: existing.is_some() && !profile_disabled_by_env(),
        auto_build_enabled: policy.enabled,
        profile_path: path.display().to_string(),
        has_backup: prev_path.is_file(),
        platform: detect_platform_label().to_string(),
        scanned_homes: homes.iter().map(|p| p.display().to_string()).collect(),
        rebuild_check,
        summary: existing.as_ref().map(BpeProfileSummary::from),
    })
}

/// Builds (or conditionally auto-builds) the session-driven BPE profile.
pub fn build_bpe_profile(config: &BpeBuildConfig) -> Result<BpeBuildReport> {
    config.validate()?;
    let state_dir = resolve_state_dir(config.state_dir_override.as_deref())?;
    let path = profile_path_in(&state_dir);
    let prev_path = prev_profile_path_in(&state_dir);
    let now = config.now_unix.unwrap_or_else(now_unix_secs);
    let window_secs = (config.window_hours * 3600.0) as u64;
    let cutoff = now.saturating_sub(window_secs);

    let homes = if config.session_dirs.is_empty() {
        discover_session_homes(config.include_windows_homes)
    } else {
        config.session_dirs.clone()
    };
    let scanned_homes: Vec<String> = homes.iter().map(|p| p.display().to_string()).collect();

    // Non-blocking singleflight guard for conditional `if_needed` auto-builds and bounded
    // concurrency waiter cap for manual builds so simultaneous bursts never pile up threads.
    struct InFlightGuard(bool);
    impl Drop for InFlightGuard {
        fn drop(&mut self) {
            if self.0 {
                BUILD_IN_FLIGHT.store(false, Ordering::Release);
            }
        }
    }
    struct WaiterGuard(bool);
    impl Drop for WaiterGuard {
        fn drop(&mut self) {
            if self.0 {
                ACTIVE_WAITERS.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }
    let acquired_flight = BUILD_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    let _waiter_guard = if !acquired_flight {
        if config.if_needed {
            let existing = read_profile_file(&path).ok().flatten();
            return Ok(BpeBuildReport {
                action: "skipped".to_string(),
                applied: false,
                dry_run: config.dry_run,
                reason: "build already in progress".to_string(),
                profile_path: path.display().to_string(),
                platform: detect_platform_label().to_string(),
                scanned_homes,
                rebuild_check: RebuildCheck {
                    needed: false,
                    reason: "build already in progress".to_string(),
                    changed_sessions: 0,
                    total_sessions_in_window: 0,
                    latest_event_mtime_secs: existing
                        .as_ref()
                        .map_or(0, |p| p.watermark.latest_event_mtime_secs),
                    hours_since_last_build: None,
                    drift_score: None,
                },
                profile: existing,
            });
        }
        let prev_waiters = ACTIVE_WAITERS.fetch_add(1, Ordering::AcqRel);
        if prev_waiters >= MAX_CONCURRENT_WAITERS {
            ACTIVE_WAITERS.fetch_sub(1, Ordering::AcqRel);
            return Err(invalid("too many concurrent BPE build requests; retry shortly").into());
        }
        WaiterGuard(true)
    } else {
        WaiterGuard(false)
    };
    let _flight_guard = InFlightGuard(acquired_flight);

    with_profile_lock(&state_dir, !config.dry_run, || {
        let existing = read_profile_file(&path).ok().flatten();
        let policy = AutoBuildPolicy::default();
        let files = collect_session_files(&homes, &config.session_dirs, config.source, cutoff);

        let mut rebuild_check = check_rebuild_needed(existing.as_ref(), &files, &policy, now, None);

        // If fast metadata check said not needed, still allow drift check on updated sessions
        // when there is at least 1 modified session and `if_needed` is set.
        if config.if_needed && !rebuild_check.needed && rebuild_check.changed_sessions > 0
            && let Some(prev) = existing.as_ref()
        {
            let recent_files: Vec<SessionFileEntry> = files
                .iter()
                .filter(|f| f.mtime_secs > prev.watermark.latest_event_mtime_secs)
                .take(16)
                .cloned()
                .collect();
            let recent_corpus = extract_corpus(&recent_files, prev.built_at_unix);
            let recent_texts: Vec<String> =
                recent_corpus.messages.iter().map(|m| m.text.clone()).collect();
            let drift = measure_bpe_drift(prev, &recent_texts);
            rebuild_check = check_rebuild_needed(existing.as_ref(), &files, &policy, now, drift);
            if !rebuild_check.needed && recent_texts.len() >= policy.min_new_messages {
                rebuild_check.needed = true;
                rebuild_check.reason = format!(
                    "{} new user messages >= threshold {}",
                    recent_texts.len(),
                    policy.min_new_messages
                );
            }
        }

        if config.if_needed && !rebuild_check.needed {
            let mut auto_state = read_auto_state(&state_dir);
            auto_state.last_checked_unix = now;
            write_auto_state(&state_dir, &auto_state);
            return Ok(BpeBuildReport {
                action: "skipped".to_string(),
                applied: false,
                dry_run: config.dry_run,
                reason: rebuild_check.reason.clone(),
                profile_path: path.display().to_string(),
                platform: detect_platform_label().to_string(),
                scanned_homes,
                rebuild_check,
                profile: existing,
            });
        }

        let corpus = extract_corpus(&files, cutoff);
        let texts: Vec<String> = corpus.messages.iter().map(|m| m.text.clone()).collect();

        // In automatic `if_needed` mode, avoid overwriting or creating a near-empty profile
        // when there are fewer than 3 usable user prompts in the window.
        if config.if_needed && texts.len() < 3 {
            let mut auto_state = read_auto_state(&state_dir);
            auto_state.last_checked_unix = now;
            write_auto_state(&state_dir, &auto_state);
            return Ok(BpeBuildReport {
                action: "skipped".to_string(),
                applied: false,
                dry_run: config.dry_run,
                reason: format!("insufficient usable user messages ({} < 3)", texts.len()),
                profile_path: path.display().to_string(),
                platform: detect_platform_label().to_string(),
                scanned_homes,
                rebuild_check,
                profile: existing,
            });
        }

        // Pick dominant tokenizer from observed session models unless caller overrode it.
        let recommended_tokenizer = config.tokenizer.unwrap_or_else(|| {
            corpus
                .model_counts
                .iter()
                .max_by(|(k1, v1), (k2, v2)| v1.cmp(v2).then_with(|| k1.cmp(k2)))
                .map(|(k, _)| TokenizerProfile::for_model(k))
                .unwrap_or(TokenizerProfile::O200k)
        });

        let merges = train_corpus_bpe(&texts, config.max_merges, 2);
        let (learned_fillers, learned_replacements) = mine_formula_rules(
            &corpus.messages,
            config.min_freq,
            config.min_sessions,
            config.min_savings_tokens,
            recommended_tokenizer,
        );

        let latest_mtime = files.iter().map(|f| f.mtime_secs).max().unwrap_or(0);
        let hangul_msgs = texts.iter().filter(|t| contains_hangul(t)).count();
        let hangul_ratio = if texts.is_empty() {
            0.0
        } else {
            round2(hangul_msgs as f64 / texts.len() as f64)
        };

        let mut total_chars = 0usize;
        let mut total_raw_tok = 0usize;
        let mut total_comp_tok = 0usize;
        let mut total_bpe_tok = 0usize;

        let provisional = BpeProfile {
            schema_version: PROFILE_SCHEMA_VERSION,
            built_at_unix: now,
            platform: detect_platform_label().to_string(),
            source: config.source,
            window_hours: config.window_hours,
            tokenizer: recommended_tokenizer.as_str().to_string(),
            watermark: SessionWatermark {
                session_count: corpus.sessions_with_messages.len(),
                message_count: corpus.messages.len(),
                latest_event_mtime_secs: latest_mtime,
                corpus_fingerprint: compute_corpus_fingerprint(&corpus.messages, latest_mtime),
            },
            stats: CorpusStats::default(),
            calibration: TokenizerCalibration {
                samples: corpus.model_counts.values().sum(),
                model_counts: corpus.model_counts,
                recommended_tokenizer: recommended_tokenizer.as_str().to_string(),
                hangul_message_share: hangul_ratio,
            },
            auto_policy: policy,
            merges,
            learned_fillers,
            learned_replacements,
            profile_digest: None,
        };

        let encoder = BpeEncoder::new(&provisional.merges);
        for msg in &texts {
            total_chars += msg.chars().count();
            let raw_t = estimate_bpe_tokens_for(msg, recommended_tokenizer);
            let compressed = crate::prompt_formula::compress_task_prompt_with_profile(
                msg,
                None,
                Some(&provisional),
            );
            let comp_t = estimate_bpe_tokens_for(&compressed, recommended_tokenizer);
            total_raw_tok += raw_t;
            total_comp_tok += comp_t.min(raw_t);
            total_bpe_tok += encoder.token_count(msg);
        }

        let n = texts.len().max(1) as f64;
        let savings_pct = if total_raw_tok > 0 {
            round2(100.0 * total_raw_tok.saturating_sub(total_comp_tok) as f64 / total_raw_tok as f64)
        } else {
            0.0
        };
        let bpe_chars_per_token = if total_bpe_tok > 0 {
            round2(total_chars as f64 / total_bpe_tok as f64)
        } else {
            0.0
        };

        let mut profile = provisional;
        profile.stats = CorpusStats {
            sessions_scanned: files.len(),
            messages_scanned: corpus.raw_messages_seen,
            messages_kept: texts.len(),
            hangul_ratio,
            avg_prompt_chars: if texts.is_empty() { 0.0 } else { round2(total_chars as f64 / n) },
            avg_raw_bpe_tokens: if texts.is_empty() { 0.0 } else { round2(total_raw_tok as f64 / n) },
            avg_compressed_bpe_tokens: if texts.is_empty() { 0.0 } else { round2(total_comp_tok as f64 / n) },
            estimated_token_savings_pct: savings_pct,
            bpe_avg_tokens_per_prompt: if texts.is_empty() { 0.0 } else { round2(total_bpe_tok as f64 / n) },
            bpe_chars_per_token,
        };
        profile.profile_digest = Some(profile.compute_digest());

        let applied = if config.dry_run {
            false
        } else {
            if let Ok(Some(old_bytes)) = read_bounded_regular_file(&path, MAX_PROFILE_BYTES, false)
            {
                let _ = write_private_atomic(&prev_path, &old_bytes);
            }
            let bytes = serde_json::to_vec_pretty(&profile)?;
            if bytes.len() > MAX_PROFILE_BYTES {
                return Err(invalid("generated bpe-profile.json exceeds 512 KiB limit").into());
            }
            write_private_atomic(&path, &bytes)?;
            let mut auto_state = read_auto_state(&state_dir);
            auto_state.last_checked_unix = now;
            auto_state.last_error = None;
            auto_state.last_error_unix = 0;
            write_auto_state(&state_dir, &auto_state);
            invalidate_profile_cache();
            true
        };

        Ok(BpeBuildReport {
            action: if config.dry_run {
                "dry-run".to_string()
            } else {
                "built".to_string()
            },
            applied,
            dry_run: config.dry_run,
            reason: if config.if_needed {
                rebuild_check.reason.clone()
            } else {
                "manual build requested".to_string()
            },
            profile_path: path.display().to_string(),
            platform: detect_platform_label().to_string(),
            scanned_homes,
            rebuild_check,
            profile: Some(profile),
        })
    })
}

/// Removes the persisted custom BPE profile (reverting to built-in rules).
pub fn reset_bpe_profile(state_dir_override: Option<&Path>) -> Result<BpeBuildReport> {
    let state_dir = resolve_state_dir(state_dir_override)?;
    let path = profile_path_in(&state_dir);
    let prev_path = prev_profile_path_in(&state_dir);
    let removed = with_profile_lock(&state_dir, true, || {
        let mut existed = false;
        if let Ok(Some(old_bytes)) = read_bounded_regular_file(&path, MAX_PROFILE_BYTES, false) {
            let _ = write_private_atomic(&prev_path, &old_bytes);
            std::fs::remove_file(&path)?;
            existed = true;
        }
        invalidate_profile_cache();
        Ok(existed)
    })?;
    Ok(BpeBuildReport {
        action: "reset".to_string(),
        applied: removed,
        dry_run: false,
        reason: if removed {
            "removed bpe-profile.json (backup saved to bpe-profile.prev.json)".to_string()
        } else {
            "no bpe-profile.json existed".to_string()
        },
        profile_path: path.display().to_string(),
        platform: detect_platform_label().to_string(),
        scanned_homes: Vec::new(),
        rebuild_check: RebuildCheck {
            needed: false,
            reason: "profile reset".to_string(),
            changed_sessions: 0,
            total_sessions_in_window: 0,
            latest_event_mtime_secs: 0,
            hours_since_last_build: None,
            drift_score: None,
        },
        profile: None,
    })
}

/// Restores the previously backed-up `bpe-profile.prev.json` atomically.
pub fn rollback_bpe_profile(state_dir_override: Option<&Path>) -> Result<BpeBuildReport> {
    let state_dir = resolve_state_dir(state_dir_override)?;
    let path = profile_path_in(&state_dir);
    let prev_path = prev_profile_path_in(&state_dir);
    let restored = with_profile_lock(&state_dir, true, || {
        let Some(prev_profile) = read_profile_file(&prev_path)? else {
            return Err(invalid("no valid bpe-profile.prev.json backup available").into());
        };
        let bytes = serde_json::to_vec_pretty(&prev_profile)?;
        write_private_atomic(&path, &bytes)?;
        invalidate_profile_cache();
        Ok(prev_profile)
    })?;
    Ok(BpeBuildReport {
        action: "rollback".to_string(),
        applied: true,
        dry_run: false,
        reason: "restored bpe-profile.prev.json".to_string(),
        profile_path: path.display().to_string(),
        platform: detect_platform_label().to_string(),
        scanned_homes: Vec::new(),
        rebuild_check: RebuildCheck {
            needed: false,
            reason: "rolled back to previous profile".to_string(),
            changed_sessions: 0,
            total_sessions_in_window: 0,
            latest_event_mtime_secs: restored.watermark.latest_event_mtime_secs,
            hours_since_last_build: None,
            drift_score: None,
        },
        profile: Some(restored),
    })
}

pub fn parse_window_hours(window: &str) -> Result<f64> {
    let trimmed = window.trim().to_ascii_lowercase();
    let hours = match trimmed.as_str() {
        "24h" | "1d" | "day" | "daily" => 24.0,
        "7d" | "week" | "weekly" => 168.0,
        "14d" | "2w" => 336.0,
        "30d" | "month" | "monthly" => 720.0,
        "90d" | "quarter" => 2160.0,
        _ if trimmed.ends_with('h') => trimmed[..trimmed.len() - 1]
            .parse::<f64>()
            .map_err(|_| invalid(format!("invalid window '{window}'")))?,
        _ if trimmed.ends_with('d') => {
            let days = trimmed[..trimmed.len() - 1]
                .parse::<f64>()
                .map_err(|_| invalid(format!("invalid window '{window}'")))?;
            days * 24.0
        }
        _ => return Err(invalid(format!("invalid window '{window}' (use e.g. 24h, 7d, 30d)")).into()),
    };
    if !hours.is_finite() || hours < 1.0 || hours > 8760.0 {
        return Err(invalid("window must resolve between 1 and 8760 hours").into());
    }
    Ok(hours)
}

/// Built-in red-team self-audit engine for `bpe-profile.json` and its state directory.
/// Detects symlink/permission issues, digest tampering, oversized/corrupted JSON,
/// protected-keyword violations, secret/PII leakage, cyclic/non-saving rules,
/// policy bounds drift, and auto-build error backoff with severity ratings.
pub fn audit_bpe_profile(config: &BpeBuildConfig) -> Result<BpeAuditReport> {
    let state_dir = resolve_state_dir(config.state_dir_override.as_deref())?;
    let path = profile_path_in(&state_dir);
    let prev_path = prev_profile_path_in(&state_dir);
    let lock_path = lock_path_in(&state_dir);
    let auto_path = auto_state_path_in(&state_dir);

    let mut findings: Vec<AuditFinding> = Vec::new();
    let mut push = |code: &str, severity: AuditSeverity, category: &str, msg: String, rem: &str| {
        findings.push(AuditFinding {
            code: code.to_string(),
            severity,
            category: category.to_string(),
            message: msg,
            remediation: rem.to_string(),
        });
    };

    // 1. Check all state files for symlinks, non-regular files, and Unix ownership/mode safety.
    for (label, target) in [
        ("bpe-profile.json", &path),
        ("bpe-profile.prev.json", &prev_path),
        ("bpe-profile.lock", &lock_path),
        ("bpe-auto-state.json", &auto_path),
    ] {
        match std::fs::symlink_metadata(target) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.file_type().is_file() {
                    push(
                        "BPE_SYMLINK_FILE",
                        AuditSeverity::Critical,
                        "filesystem_security",
                        format!("{label} is a symlink or non-regular file ({})", target.display()),
                        "Remove the symlink/special file and rebuild with `cctx prompt build-bpe`.",
                    );
                } else {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        let euid = unsafe { libc::geteuid() };
                        if meta.uid() != euid || (meta.mode() & 0o022) != 0 {
                            push(
                                "BPE_UNSAFE_PERMISSIONS",
                                AuditSeverity::Critical,
                                "filesystem_security",
                                format!(
                                    "{label} has unsafe ownership or writable mode {:o}",
                                    meta.mode() & 0o777
                                ),
                                "Run `chmod 600` on state files owned by your user or reset and rebuild.",
                            );
                        }
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                push(
                    "BPE_STAT_ERROR",
                    AuditSeverity::High,
                    "filesystem_security",
                    format!("failed to inspect {label}: {err}"),
                    "Verify permissions on CCTX state directory.",
                );
            }
        }
    }

    // 2. Inspect raw persisted profile (before sanitization) to detect on-disk tampering/defects.
    let mut exists = false;
    let mut digest_valid: Option<bool> = None;
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => {
            exists = true;
            if meta.len() as usize > MAX_PROFILE_BYTES {
                push(
                    "BPE_PROFILE_OVERSIZED",
                    AuditSeverity::High,
                    "dos_bounds",
                    format!(
                        "bpe-profile.json size {} exceeds {} byte limit",
                        meta.len(),
                        MAX_PROFILE_BYTES
                    ),
                    "Reset and rebuild with `cctx prompt build-bpe --reset && cctx prompt build-bpe`.",
                );
            } else if let Ok(Some(bytes)) =
                read_bounded_regular_file(&path, MAX_PROFILE_BYTES, false)
            {
                match serde_json::from_slice::<BpeProfile>(&bytes) {
                    Err(err) => {
                        push(
                            "BPE_PROFILE_CORRUPT_JSON",
                            AuditSeverity::High,
                            "integrity",
                            format!("bpe-profile.json failed JSON parsing: {err}"),
                            "Rollback or rebuild with `cctx prompt build-bpe --rollback` or `cctx prompt build-bpe`.",
                        );
                    }
                    Ok(raw_prof) => {
                        if raw_prof.schema_version != PROFILE_SCHEMA_VERSION {
                            push(
                                "BPE_SCHEMA_MISMATCH",
                                AuditSeverity::High,
                                "integrity",
                                format!(
                                    "unsupported schema_version {} (expected {})",
                                    raw_prof.schema_version, PROFILE_SCHEMA_VERSION
                                ),
                                "Rebuild profile with current cctx binary.",
                            );
                        }
                        if let Some(ref stored) = raw_prof.profile_digest {
                            let expected = raw_prof.compute_digest();
                            let ok = stored == &expected;
                            digest_valid = Some(ok);
                            if !ok {
                                push(
                                    "BPE_DIGEST_MISMATCH",
                                    AuditSeverity::High,
                                    "integrity",
                                    "stored profile_digest does not match computed SHA-256 (file modified on disk)".to_string(),
                                    "Rebuild with `cctx prompt build-bpe` or restore backup with `--rollback`.",
                                );
                            }
                        } else {
                            digest_valid = Some(false);
                            push(
                                "BPE_DIGEST_MISSING",
                                AuditSeverity::Medium,
                                "integrity",
                                "profile_digest is missing (legacy profile built before integrity hashing)".to_string(),
                                "Rebuild with `cctx prompt build-bpe` to attach SHA-256 integrity digest.",
                            );
                        }

                        if raw_prof.merges.len() > MAX_MERGES_LIMIT
                            || raw_prof.learned_fillers.len() > MAX_LEARNED_FILLERS
                            || raw_prof.learned_replacements.len() > MAX_LEARNED_REPLACEMENTS
                        {
                            push(
                                "BPE_TABLE_OVER_LIMIT",
                                AuditSeverity::Medium,
                                "dos_bounds",
                                format!(
                                    "profile tables exceed safe limits (merges={}, fillers={}, rules={})",
                                    raw_prof.merges.len(),
                                    raw_prof.learned_fillers.len(),
                                    raw_prof.learned_replacements.len()
                                ),
                                "Rebuild profile with bounded parameters.",
                            );
                        }

                        let tok = raw_prof.tokenizer_profile();
                        for m in &raw_prof.merges {
                            if let Some(kind) = detect_sensitive_patterns(&m.merged) {
                                push(
                                    "BPE_SENSITIVE_DATA_LEAK",
                                    AuditSeverity::Critical,
                                    "privacy_security",
                                    format!("sensitive pattern ({kind}) detected in BPE merge table"),
                                    "Reset and rebuild profile immediately.",
                                );
                                break;
                            }
                        }
                        for filler in &raw_prof.learned_fillers {
                            if contains_protected_keyword(filler) {
                                push(
                                    "BPE_PROTECTED_KEYWORD_VIOLATION",
                                    AuditSeverity::Critical,
                                    "semantic_safety",
                                    format!("learned filler touches protected keyword: \"{filler}\""),
                                    "Rebuild profile to purge unsafe filler.",
                                );
                            }
                            if let Some(kind) = detect_sensitive_patterns(filler) {
                                push(
                                    "BPE_SENSITIVE_DATA_LEAK",
                                    AuditSeverity::Critical,
                                    "privacy_security",
                                    format!("sensitive pattern ({kind}) detected in learned filler"),
                                    "Reset and rebuild profile immediately.",
                                );
                            }
                            if filler.contains('/') || filler.contains('\\') {
                                push(
                                    "BPE_SENSITIVE_DATA_LEAK",
                                    AuditSeverity::Critical,
                                    "privacy_security",
                                    "path separator detected in learned filler".to_string(),
                                    "Reset and rebuild profile.",
                                );
                            }
                        }
                        for (i, rule) in raw_prof.learned_replacements.iter().enumerate() {
                            if contains_protected_keyword(&rule.from)
                                || !preserves_protected_keywords(&rule.from, &rule.to)
                            {
                                push(
                                    "BPE_PROTECTED_KEYWORD_VIOLATION",
                                    AuditSeverity::Critical,
                                    "semantic_safety",
                                    format!(
                                        "rule violates protected keyword safety: \"{}\" => \"{}\"",
                                        rule.from, rule.to
                                    ),
                                    "Rebuild profile to purge unsafe rule.",
                                );
                            }
                            if let Some(kind) = detect_sensitive_patterns(&rule.from)
                                .or_else(|| detect_sensitive_patterns(&rule.to))
                            {
                                push(
                                    "BPE_SENSITIVE_DATA_LEAK",
                                    AuditSeverity::Critical,
                                    "privacy_security",
                                    format!("sensitive pattern ({kind}) detected in learned rule"),
                                    "Reset and rebuild profile immediately.",
                                );
                            }
                            if rule.from.is_empty()
                                || rule.to.trim().is_empty()
                                || rule.from == rule.to
                            {
                                push(
                                    "BPE_CYCLIC_OR_DUPLICATE_RULE",
                                    AuditSeverity::High,
                                    "rule_soundness",
                                    format!("degenerate or self-referential rule: \"{}\"", rule.from),
                                    "Rebuild profile.",
                                );
                            }
                            for other in &raw_prof.learned_replacements[i + 1..] {
                                if other.from == rule.from
                                    || other.to.contains(&rule.from)
                                    || rule.to.contains(&other.from)
                                {
                                    push(
                                        "BPE_CYCLIC_OR_DUPLICATE_RULE",
                                        AuditSeverity::High,
                                        "rule_soundness",
                                        format!(
                                            "duplicate or cyclic rule interaction between \"{}\" and \"{}\"",
                                            rule.from, other.from
                                        ),
                                        "Rebuild profile to deduplicate rules.",
                                    );
                                    break;
                                }
                            }
                            if rule.to.chars().count() > 96
                                || rule.to.chars().count() * 4 < rule.from.chars().count()
                                || estimate_bpe_tokens_for(&rule.from, tok)
                                    <= estimate_bpe_tokens_for(&rule.to, tok)
                            {
                                push(
                                    "BPE_NON_SAVING_OR_OVERLONG_RULE",
                                    AuditSeverity::Medium,
                                    "rule_soundness",
                                    format!(
                                        "non-saving or over-truncated rule: \"{}\" => \"{}\"",
                                        rule.from, rule.to
                                    ),
                                    "Rebuild profile to prune ineffective rules.",
                                );
                            }
                        }

                        let p = &raw_prof.auto_policy;
                        if !p.min_interval_hours.is_finite()
                            || !(0.5..=720.0).contains(&p.min_interval_hours)
                            || !(1..=500).contains(&p.min_new_sessions)
                            || !(1..=5_000).contains(&p.min_new_messages)
                            || !p.drift_threshold.is_finite()
                            || !(0.05..=0.95).contains(&p.drift_threshold)
                        {
                            push(
                                "BPE_INVALID_AUTO_POLICY",
                                AuditSeverity::Medium,
                                "policy_bounds",
                                "auto_policy contains out-of-range parameters".to_string(),
                                "Rebuild profile to restore safe policy bounds.",
                            );
                        }
                    }
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            push(
                "BPE_PROFILE_NOT_BUILT",
                AuditSeverity::Low,
                "lifecycle",
                "bpe-profile.json has not been built yet (using built-in rules only)".to_string(),
                "Run `cctx prompt build-bpe --window 7d` to personalize compression from Copilot sessions.",
            );
        }
        _ => {}
    }

    // 3. Check auto-build error backoff state.
    let auto_state = read_auto_state(&state_dir);
    let now = config.now_unix.unwrap_or_else(now_unix_secs);
    if auto_state.last_error_unix > 0
        && now.saturating_sub(auto_state.last_error_unix) < ERROR_BACKOFF_SECS
    {
        push(
            "BPE_AUTO_ERROR_BACKOFF",
            AuditSeverity::Medium,
            "auto_build",
            format!(
                "background auto-build is in error backoff: {}",
                auto_state.last_error.as_deref().unwrap_or("unknown error")
            ),
            "Inspect permissions/logs and run `cctx prompt build-bpe` manually to clear backoff.",
        );
    }

    // 4. Check staleness / rebuild readiness as informational Low finding.
    if exists
        && let Ok(status) = inspect_bpe_status(config)
        && status.rebuild_check.needed
    {
        push(
            "BPE_PROFILE_STALE",
            AuditSeverity::Low,
            "freshness",
            format!("profile rebuild recommended: {}", status.rebuild_check.reason),
            "Run `cctx prompt build-bpe --if-needed` or wait for background auto-build.",
        );
    }

    findings.sort_by(|a, b| b.severity.cmp(&a.severity).then_with(|| a.code.cmp(&b.code)));
    let mut counts = AuditCounts::default();
    for f in &findings {
        match f.severity {
            AuditSeverity::Critical => counts.critical += 1,
            AuditSeverity::High => counts.high += 1,
            AuditSeverity::Medium => counts.medium += 1,
            AuditSeverity::Low => counts.low += 1,
        }
    }
    let highest_severity = findings
        .first()
        .map_or("none", |f| f.severity.as_str())
        .to_string();
    let passed = counts.critical == 0 && counts.high == 0 && counts.medium == 0;

    Ok(BpeAuditReport {
        passed,
        highest_severity,
        counts,
        exists,
        digest_valid,
        profile_path: path.display().to_string(),
        platform: detect_platform_label().to_string(),
        findings,
    })
}

pub fn format_audit_report_text(report: &BpeAuditReport) -> String {
    let mut out = format!(
        "BPE self-audit [{}]: {} (highest={}, critical={}, high={}, medium={}, low={})\nPath: {}\n",
        report.platform,
        if report.passed { "PASS" } else { "FAIL" },
        report.highest_severity,
        report.counts.critical,
        report.counts.high,
        report.counts.medium,
        report.counts.low,
        report.profile_path
    );
    for f in &report.findings {
        out.push_str(&format!(
            "  [{}] {} ({}): {} -> {}\n",
            f.severity.as_str().to_uppercase(),
            f.code,
            f.category,
            f.message,
            f.remediation
        ));
    }
    out.trim_end().to_string()
}

pub fn format_build_report_text(report: &BpeBuildReport) -> String {
    let mut out = format!(
        "BPE profile {} [{}] ({})\nPath: {}\nReason: {}\n",
        report.action, report.platform, if report.applied { "applied" } else { "not applied" },
        report.profile_path, report.reason
    );
    if let Some(p) = &report.profile {
        out.push_str(&format!(
            "Source: {} | Window: {:.0}h | Tokenizer: {} | Sessions: {} | Messages: {}/{}\n",
            p.source.as_str(),
            p.window_hours,
            p.tokenizer,
            p.stats.sessions_scanned,
            p.stats.messages_kept,
            p.stats.messages_scanned
        ));
        out.push_str(&format!(
            "BPE merges: {} ({:.2} chars/tok) | Fillers: {} | Rules: {} | Est. savings: {:.1}%\n",
            p.merges.len(),
            p.stats.bpe_chars_per_token,
            p.learned_fillers.len(),
            p.learned_replacements.len(),
            p.stats.estimated_token_savings_pct
        ));
        if !p.learned_replacements.is_empty() {
            out.push_str("Top learned rules:\n");
            for r in p.learned_replacements.iter().take(8) {
                out.push_str(&format!(
                    "  - \"{}\" => \"{}\" (freq={}, sessions={}, -{} tok)\n",
                    r.from, r.to, r.frequency, r.session_count, r.tokens_saved
                ));
            }
        }
    }
    out.trim_end().to_string()
}

pub fn format_status_report_text(report: &BpeStatusReport) -> String {
    let mut out = format!(
        "BPE profile status [{}]: {}\nPath: {}\nAuto-build: {} | Backup: {}\nRebuild needed: {} ({})\n",
        report.platform,
        if report.active {
            "active"
        } else if report.exists {
            "present (disabled via env)"
        } else {
            "not built (built-in rules only)"
        },
        report.profile_path,
        if report.auto_build_enabled { "on" } else { "off" },
        if report.has_backup { "yes" } else { "no" },
        report.rebuild_check.needed,
        report.rebuild_check.reason
    );
    if let Some(s) = &report.summary {
        out.push_str(&format!(
            "Tokenizer: {} | Merges: {} | Fillers: {} | Rules: {} | Est. savings: {:.1}% ({} msgs / {} sessions)\n",
            s.tokenizer,
            s.merge_count,
            s.learned_filler_count,
            s.learned_replacement_count,
            s.estimated_token_savings_pct,
            s.messages_kept,
            s.sessions_scanned
        ));
    }
    out.trim_end().to_string()
}

/// Non-blocking singleflight entry point used by the background daemon idle loop.
pub fn auto_build_if_needed_background() -> Option<BpeBuildReport> {
    let policy = AutoBuildPolicy::default();
    if !policy.enabled || profile_disabled_by_env() {
        return None;
    }
    let state_dir = state_dir().ok()?;
    let now = now_unix_secs();
    let auto_state = read_auto_state(&state_dir);
    if auto_state.last_error_unix > 0
        && now.saturating_sub(auto_state.last_error_unix) < ERROR_BACKOFF_SECS
    {
        return None;
    }
    if BUILD_IN_FLIGHT.load(Ordering::Acquire) {
        return None;
    }

    let config = BpeBuildConfig {
        if_needed: true,
        ..BpeBuildConfig::default()
    };
    match build_bpe_profile(&config) {
        Ok(report) => Some(report),
        Err(err) => {
            let mut st = read_auto_state(&state_dir);
            st.last_checked_unix = now;
            st.last_error_unix = now;
            st.last_error = Some(err.to_string());
            write_auto_state(&state_dir, &st);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_session(dir: &Path, sid: &str, messages: &[(&str, &str)], model: &str) {
        let sdir = dir.join(sid);
        std::fs::create_dir_all(&sdir).unwrap();
        let mut out = String::new();
        for (ts, content) in messages {
            out.push_str(
                &json!({
                    "type": "user.message",
                    "timestamp": ts,
                    "data": { "content": content }
                })
                .to_string(),
            );
            out.push('\n');
        }
        out.push_str(
            &json!({
                "type": "assistant.usage",
                "timestamp": "2026-11-24T01:00:00Z",
                "data": { "model": model, "inputTokens": 500, "outputTokens": 120 }
            })
            .to_string(),
        );
        out.push('\n');
        std::fs::write(sdir.join("events.jsonl"), out).unwrap();
    }

    #[test]
    fn sanitize_strips_xml_code_paths_and_secrets() {
        let raw = "<system_reminder>secret</system_reminder>\n혹시 C:\\Users\\alice\\repo\\app.rs 와 /home/bob/a.py 를 확인해서 정리해줘\n```rust\nfn x() {}\n```\nhttps://example.com/foo";
        let clean = sanitize_user_prompt(raw).expect("cleaned");
        assert!(!clean.contains("system_reminder"));
        assert!(!clean.contains("C:\\Users"));
        assert!(!clean.contains("/home/bob"));
        assert!(!clean.contains("fn x()"));
        assert!(!clean.contains("https://"));
        assert!(clean.contains("확인해서"));

        let secret = "please use ghp_1234567890abcdef1234567890abcdef1234 to login";
        assert!(sanitize_user_prompt(secret).is_none());
    }

    #[test]
    fn build_apply_overlay_rollback_and_if_needed_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        let state = tmp.path().join("state");
        std::fs::create_dir_all(&sessions).unwrap();

        let now = parse_iso8601_unix("2026-11-24T12:00:00Z").unwrap();
        for sid in ["s1", "s2", "s3"] {
            write_session(
                &sessions,
                sid,
                &[
                    (
                        "2026-11-24T10:00:00Z",
                        "혹시 백엔드 지연 원인을 분석해서 정리해줘 부탁해",
                    ),
                    (
                        "2026-11-24T10:05:00Z",
                        "전반적으로 성능 병목을 조사해서 보강하고 보고해줘",
                    ),
                    (
                        "2026-11-24T10:10:00Z",
                        "읽기 전용으로만 확인해 볼래 수정하지 말고",
                    ),
                ],
                "claude-sonnet-4-7",
            );
        }

        let cfg = BpeBuildConfig {
            window_hours: 168.0,
            source: BpeSource::Copilot,
            min_freq: 3,
            min_sessions: 2,
            min_savings_tokens: 1,
            max_merges: 64,
            include_windows_homes: false,
            session_dirs: vec![sessions.clone()],
            dry_run: false,
            if_needed: false,
            tokenizer: None,
            now_unix: Some(now),
            state_dir_override: Some(state.clone()),
        };

        let report = build_bpe_profile(&cfg).expect("build succeeds");
        assert_eq!(report.action, "built");
        assert!(report.applied);
        let profile = report.profile.expect("profile present");
        assert_eq!(profile.tokenizer, "claude-4-7");
        assert!(!profile.merges.is_empty());
        assert!(!profile.learned_fillers.is_empty());
        assert!(!profile.learned_replacements.is_empty());

        // Protected negative constraints must never be dropped by learned rules.
        for rep in &profile.learned_replacements {
            assert!(preserves_protected_keywords(&rep.from, &rep.to));
            assert!(!rep.from.contains("수정하지"));
        }

        // `if_needed` immediately after build must skip when no new sessions arrived.
        let mut auto_cfg = cfg.clone();
        auto_cfg.if_needed = true;
        let skipped = build_bpe_profile(&auto_cfg).expect("if_needed check");
        assert_eq!(skipped.action, "skipped");
        assert!(!skipped.applied);

        // Reset + rollback roundtrip.
        let reset = reset_bpe_profile(Some(&state)).expect("reset");
        assert_eq!(reset.action, "reset");
        assert!(reset.applied);
        assert!(!profile_path_in(&state).exists());

        let rolled = rollback_bpe_profile(Some(&state)).expect("rollback");
        assert_eq!(rolled.action, "rollback");
        assert!(rolled.applied);
        assert!(profile_path_in(&state).exists());
    }

    #[test]
    fn bpe_merges_compress_and_detect_drift() {
        let corpus = vec![
            "성능 최적화 분석해서 보고해줘".to_string(),
            "성능 최적화 조사해서 보고해줘".to_string(),
            "성능 최적화 검토해서 보고해줘".to_string(),
            "성능 최적화 보강해서 보고해줘".to_string(),
        ];
        let merges = train_corpus_bpe(&corpus, 32, 2);
        assert!(!merges.is_empty());
        let raw_chars: usize = corpus[0].chars().count();
        let encoded_toks = encode_bpe_token_count(&merges, &corpus[0]);
        assert!(encoded_toks < raw_chars);

        let mut profile = BpeProfile {
            schema_version: PROFILE_SCHEMA_VERSION,
            built_at_unix: 1,
            platform: "linux".to_string(),
            source: BpeSource::Copilot,
            window_hours: 168.0,
            tokenizer: "o200k".to_string(),
            watermark: SessionWatermark::default(),
            stats: CorpusStats {
                bpe_chars_per_token: (raw_chars as f64) / (encoded_toks as f64),
                ..CorpusStats::default()
            },
            calibration: TokenizerCalibration::default(),
            auto_policy: AutoBuildPolicy::default(),
            merges,
            learned_fillers: Vec::new(),
            learned_replacements: Vec::new(),
            profile_digest: None,
        };
        let same_drift = measure_bpe_drift(&profile, &corpus).unwrap();
        assert!(same_drift < 0.05);

        let shifted = vec![
            "kubernetes helm ingress terraform deployment manifest".to_string(),
            "postgres vacuum analyze replication wal checkpoint".to_string(),
        ];
        profile.stats.bpe_chars_per_token = 3.0;
        let high_drift = measure_bpe_drift(&profile, &shifted).unwrap();
        assert!(high_drift > 0.30);
    }
}
