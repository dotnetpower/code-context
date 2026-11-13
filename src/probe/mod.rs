//! Declarative Rust probe primitives (`cctx probe` / `pctx`) that replace ad-hoc
//! `python3 -c`, `curl`, `jq`, and `sqlite3` inspection scripts with structure-aware,
//! inline-budget-bounded (`<8 KiB`), secret-redacted summaries.

pub mod http_probe;
pub mod image_probe;
pub mod json_probe;
pub mod log_probe;
pub mod table_probe;
pub mod text_index;

use crate::inline_budget::INLINE_BUDGET;
use crate::journal::JournalCommand;
use crate::{Result, invalid};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{BufRead, Read};
use std::path::PathBuf;

/// Maximum expanded URLs per batch HTTP probe to prevent accidental Cartesian explosion.
pub const MAX_BATCH_URLS: usize = 64;

/// Maximum UTF-8 scalar preview characters for individual string leaf values in samples.
pub const MAX_SAMPLE_STRING_CHARS: usize = 160;

#[derive(Clone, Debug, Subcommand)]
pub enum ProbeCommand {
    /// Concurrent HTTP / OpenAPI / Tile / STAC batch prober with secret redaction & schema summary
    Http(Box<HttpProbeArgs>),
    /// Local file, stdin, offloaded `/tmp` output, or URL JSON / JSONL / GeoJSON / STAC inspector
    Json(Box<JsonProbeArgs>),
    /// Zero-config SQLite, Parquet (local or HTTP Range), and CSV/TSV schema & sample inspector
    Table(Box<TableProbeArgs>),
    /// PNG (with scanline exposure/luminance stats), JPEG, WebP, GIF, and SVG asset verifier
    Image(ImageProbeArgs),
    /// Unstructured log, `/tmp` offloaded text, or `@sN` spool BM25 + Trigram RRF inspector
    Log(Box<LogProbeArgs>),
    /// Bounded multi-command shell runner that spools output (`@sN`) and searches chunks in one call
    Batch(Box<BatchProbeArgs>),
    /// Per-workspace compaction continuity journal (`<=2 KiB` state block, BM25 recall & fail-open hook)
    Journal(JournalProbeArgs),
}

#[derive(Clone, Debug, Args)]
pub struct HttpProbeArgs {
    /// One or more HTTP/HTTPS URLs or `{var}` URL templates
    #[arg(required = true)]
    pub urls: Vec<String>,
    /// Template matrix variable `name=v1,v2,...` (repeatable)
    #[arg(long = "var", value_name = "KEY=V1,V2")]
    pub vars: Vec<String>,
    /// Request HTTP method (`GET`, `HEAD`, `POST`)
    #[arg(long, default_value = "GET")]
    pub method: String,
    /// Custom HTTP header `Name: Value` (repeatable)
    #[arg(long = "header", short = 'H', value_name = "HEADER")]
    pub headers: Vec<String>,
    /// Inject query parameter from an environment variable without shell expansion (`param=ENV_VAR`)
    #[arg(long = "env-query", value_name = "PARAM=ENV")]
    pub env_queries: Vec<String>,
    /// Inject header value from an environment variable (`Header-Name=ENV_VAR`; optional `Bearer ` prefix via `Authorization=Bearer:ENV_VAR`)
    #[arg(long = "env-header", value_name = "HEADER=ENV")]
    pub env_headers: Vec<String>,
    /// Optional request body for POST probes
    #[arg(long, allow_hyphen_values = true)]
    pub body: Option<String>,
    /// Per-request timeout in seconds (1..=60)
    #[arg(long, default_value_t = 8)]
    pub timeout: u64,
    /// Concurrent worker limit (1..=16)
    #[arg(long, default_value_t = 6)]
    pub jobs: usize,
    /// Optional dot/bracket selector path applied to JSON responses (e.g. `features[].id`)
    #[arg(long, short = 'p', allow_hyphen_values = true)]
    pub path: Option<String>,
    /// Optional spatial bounding box filter `min_lon,min_lat,max_lon,max_lat`
    #[arg(long, allow_hyphen_values = true)]
    pub bbox: Option<String>,
    /// Optional predicate filter (`field=val`, `field!=val`, `field~=sub`, `field>n`, `field<n`, `field?`)
    #[arg(long = "where", allow_hyphen_values = true)]
    pub where_expr: Option<String>,
    /// Optional frequency counter field path
    #[arg(long = "count-by")]
    pub count_by: Option<String>,
    /// Maximum sample items to include per response (0..=20)
    #[arg(long, default_value_t = 2)]
    pub samples: usize,
    /// Emit structured JSON inside the `<8 KiB` inline budget
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct JsonProbeArgs {
    /// File path (`.json`, `.jsonl`, `.geojson`, `/tmp/*-copilot-tool-output-*.txt`), `-` for stdin, or `http(s)://` URL
    pub source: String,
    /// Optional dot/bracket selector path (e.g. `features[].properties`, `items[0:5]`)
    #[arg(long, short = 'p', allow_hyphen_values = true)]
    pub path: Option<String>,
    /// Optional spatial bounding box filter `min_lon,min_lat,max_lon,max_lat`
    #[arg(long, allow_hyphen_values = true)]
    pub bbox: Option<String>,
    /// Optional predicate filter (`field=val`, `field!=val`, `field~=sub`, `field>n`, `field<n`, `field?`)
    #[arg(long = "where", allow_hyphen_values = true)]
    pub where_expr: Option<String>,
    /// Optional frequency counter field path
    #[arg(long = "count-by")]
    pub count_by: Option<String>,
    /// Maximum sample items to include (0..=25)
    #[arg(long, default_value_t = 3)]
    pub samples: usize,
    /// Timeout in seconds when `source` is an `http(s)://` URL
    #[arg(long, default_value_t = 10)]
    pub timeout: u64,
    /// Emit structured JSON inside the `<8 KiB` inline budget
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct TableProbeArgs {
    /// Path to `.sqlite`/`.db`, `.parquet` (local path or `http(s)://` URL), or `.csv`/`.tsv`
    pub source: String,
    /// Optional table name filter for SQLite databases
    #[arg(long)]
    pub table: Option<String>,
    /// Optional read-only `SELECT` / `WITH` query for SQLite databases
    #[arg(long)]
    pub sql: Option<String>,
    /// Optional spatial bounding box filter `min_lon,min_lat,max_lon,max_lat` (filters Parquet row groups or rows)
    #[arg(long, allow_hyphen_values = true)]
    pub bbox: Option<String>,
    /// Optional predicate filter for CSV/TSV rows
    #[arg(long = "where", allow_hyphen_values = true)]
    pub where_expr: Option<String>,
    /// Optional frequency counter column for CSV/TSV
    #[arg(long = "count-by")]
    pub count_by: Option<String>,
    /// Maximum sample rows per table (0..=20)
    #[arg(long, default_value_t = 3)]
    pub samples: usize,
    /// Timeout in seconds for remote HTTP Range Parquet inspection
    #[arg(long, default_value_t = 10)]
    pub timeout: u64,
    /// Emit structured JSON inside the `<8 KiB` inline budget
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct ImageProbeArgs {
    /// One or more image/SVG file paths or directories
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
    /// Optional expected `WIDTHxHEIGHT` assertion (e.g. `256x256` or `1440x480`)
    #[arg(long)]
    pub expect: Option<String>,
    /// Skip PNG IDAT pixel decompression and inspect headers only
    #[arg(long)]
    pub header_only: bool,
    /// Emit structured JSON inside the `<8 KiB` inline budget
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct LogProbeArgs {
    /// File path (`.log`, `.txt`, `/tmp/*-copilot-tool-output-*.txt`), `-` for stdin, or `@s1` / `@latest` spool handle
    pub source: String,
    /// One or more search queries ranked with Okapi BM25 + Trigram RRF (repeatable)
    #[arg(long = "query", short = 'q')]
    pub queries: Vec<String>,
    /// Optional regex filter applied across chunks
    #[arg(long, short = 'e', allow_hyphen_values = true)]
    pub regex: Option<String>,
    /// Optional severity filter (`fatal`, `error`, `warn`, `info`)
    #[arg(long)]
    pub level: Option<String>,
    /// Expand one specific 0-based chunk ordinal (`#ord`)
    #[arg(long)]
    pub chunk: Option<usize>,
    /// Maximum matching chunks returned per query (`1..=16`)
    #[arg(long, default_value_t = 4)]
    pub limit: usize,
    /// Save ingested file/stdin into the local `@sN` probe spool for zero-cost follow-up queries
    #[arg(long)]
    pub save: bool,
    /// Emit structured JSON inside the `<8 KiB` inline budget
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct BatchProbeArgs {
    /// One or more shell commands to run, spool, and search (`1..=16`, repeatable)
    #[arg(long = "cmd", short = 'c', required = true)]
    pub commands: Vec<String>,
    /// One or more search queries ranked with Okapi BM25 + Trigram RRF across captured output (repeatable)
    #[arg(long = "query", short = 'q')]
    pub queries: Vec<String>,
    /// Optional regex filter applied across captured chunks
    #[arg(long, short = 'e', allow_hyphen_values = true)]
    pub regex: Option<String>,
    /// Optional severity filter (`fatal`, `error`, `warn`, `info`)
    #[arg(long)]
    pub level: Option<String>,
    /// Working directory for spawned commands
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Per-command timeout in seconds (`1..=300`)
    #[arg(long, default_value_t = 30)]
    pub timeout: u64,
    /// Maximum matching chunks returned per query (`1..=16`)
    #[arg(long, default_value_t = 4)]
    pub limit: usize,
    /// Emit structured JSON inside the `<8 KiB` inline budget
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct JournalProbeArgs {
    /// Workspace root for journal storage and recall (defaults to `.`)
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    #[command(subcommand)]
    pub command: Box<JournalCommand>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BBox {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

impl BBox {
    pub fn parse(raw: &str) -> Result<Self> {
        let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
        if parts.len() != 4 {
            return Err(invalid(
                "bbox must have 4 comma-separated numbers: min_lon,min_lat,max_lon,max_lat",
            )
            .into());
        }
        let nums: std::result::Result<Vec<f64>, _> = parts.iter().map(|s| s.parse::<f64>()).collect();
        let nums = nums.map_err(|_| invalid(format!("invalid float in bbox: {raw}")))?;
        if nums.iter().any(|n| !n.is_finite()) {
            return Err(invalid("bbox coordinates must be finite numbers").into());
        }
        let (min_lon, min_lat, max_lon, max_lat) = (nums[0], nums[1], nums[2], nums[3]);
        if min_lon > max_lon || min_lat > max_lat {
            return Err(invalid(format!(
                "bbox min must not exceed max (got [{min_lon}, {min_lat}, {max_lon}, {max_lat}])"
            ))
            .into());
        }
        Ok(Self {
            min_lon,
            min_lat,
            max_lon,
            max_lat,
        })
    }

    pub fn intersects(&self, other: &BBox) -> bool {
        other.min_lon <= self.max_lon
            && other.max_lon >= self.min_lon
            && other.min_lat <= self.max_lat
            && other.max_lat >= self.min_lat
    }
}

#[derive(Clone, Debug)]
pub enum Predicate {
    Exists(String),
    Eq(String, String),
    Ne(String, String),
    Contains(String, String),
    Gt(String, f64),
    Ge(String, f64),
    Lt(String, f64),
    Le(String, f64),
}

impl Predicate {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(invalid("where expression must not be empty").into());
        }
        for (op, kind) in [
            ("!=", "ne"),
            ("~=", "contains"),
            (">=", "ge"),
            ("<=", "le"),
            ("==", "eq"),
            ("=", "eq"),
            (">", "gt"),
            ("<", "lt"),
        ] {
            if let Some((lhs, rhs)) = trimmed.split_once(op) {
                let field = lhs.trim().to_string();
                if field.is_empty() {
                    return Err(invalid(format!("missing field name in where: {raw}")).into());
                }
                let val = rhs.trim().trim_matches('"').trim_matches('\'').to_string();
                return match kind {
                    "eq" => Ok(Self::Eq(field, val)),
                    "ne" => Ok(Self::Ne(field, val)),
                    "contains" => Ok(Self::Contains(field, val)),
                    "gt" | "ge" | "lt" | "le" => {
                        let num = val.parse::<f64>().map_err(|_| {
                            invalid(format!("expected numeric literal in `{raw}`, got `{val}`"))
                        })?;
                        if !num.is_finite() {
                            return Err(invalid("where numeric literal must be finite").into());
                        }
                        Ok(match kind {
                            "gt" => Self::Gt(field, num),
                            "ge" => Self::Ge(field, num),
                            "lt" => Self::Lt(field, num),
                            _ => Self::Le(field, num),
                        })
                    }
                    _ => Err(invalid("unsupported operator").into()),
                };
            }
        }
        if let Some(field) = trimmed.strip_suffix('?') {
            let f = field.trim();
            if f.is_empty() {
                return Err(invalid("missing field before `?`").into());
            }
            return Ok(Self::Exists(f.to_string()));
        }
        Ok(Self::Exists(trimmed.to_string()))
    }

    pub fn field(&self) -> &str {
        match self {
            Self::Exists(f)
            | Self::Eq(f, _)
            | Self::Ne(f, _)
            | Self::Contains(f, _)
            | Self::Gt(f, _)
            | Self::Ge(f, _)
            | Self::Lt(f, _)
            | Self::Le(f, _) => f,
        }
    }

    pub fn matches_value(&self, v: Option<&Value>) -> bool {
        match self {
            Self::Exists(_) => matches!(v, Some(val) if !val.is_null()),
            Self::Eq(_, expected) => value_as_comparable_string(v).as_deref() == Some(expected.as_str()),
            Self::Ne(_, expected) => value_as_comparable_string(v).as_deref() != Some(expected.as_str()),
            Self::Contains(_, needle) => value_as_comparable_string(v)
                .is_some_and(|s| s.to_lowercase().contains(&needle.to_lowercase())),
            Self::Gt(_, n) => value_as_f64(v).is_some_and(|x| x > *n),
            Self::Ge(_, n) => value_as_f64(v).is_some_and(|x| x >= *n),
            Self::Lt(_, n) => value_as_f64(v).is_some_and(|x| x < *n),
            Self::Le(_, n) => value_as_f64(v).is_some_and(|x| x <= *n),
        }
    }
}

pub fn value_as_comparable_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        other => serde_json::to_string(other).ok(),
    }
}

pub fn value_as_f64(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|n| n.is_finite()),
        _ => None,
    }
}

/// Truncates a string cleanly on a UTF-8 scalar boundary with an explicit char-length suffix.
pub fn truncate_utf8_chars(input: &str, max_chars: usize) -> String {
    let mut count = 0usize;
    let mut cut_byte = None;
    for (idx, _) in input.char_indices() {
        if count == max_chars {
            cut_byte = Some(idx);
            break;
        }
        count += 1;
    }
    if let Some(byte_idx) = cut_byte {
        let total_chars = count + input[byte_idx..].chars().count();
        format!("{}…[+{} chars]", &input[..byte_idx], total_chars - max_chars)
    } else {
        input.to_string()
    }
}

/// Truncates multi-byte UTF-8 text to fit strictly within `max_bytes` bytes without panicking.
pub fn clamp_utf8_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let notice = "\n... [probe inline budget reached (<8KiB): narrow with --path, --where, --bbox, or --samples 1]";
    let keep = max_bytes.saturating_sub(notice.len() + 4);
    let mut end = keep.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{notice}", &text[..end])
}

/// Enforces `INLINE_BUDGET` (`7_400` bytes) on structured JSON probe payloads.
pub fn clamp_json_budget(mut payload: Value) -> Result<String> {
    let serialized = serde_json::to_string_pretty(&payload)?;
    if serialized.len() <= INLINE_BUDGET {
        return Ok(serialized);
    }
    let compact = serde_json::to_string(&payload)?;
    if compact.len() <= INLINE_BUDGET {
        return Ok(compact);
    }
    let mut current_len = compact.len();
    for key in ["samples", "results", "tables", "items", "row_groups"] {
        let mut omitted = 0usize;
        if let Some(arr) = payload.get_mut(key).and_then(Value::as_array_mut) {
            while arr.len() > 1 && current_len > INLINE_BUDGET.saturating_sub(120) {
                if let Some(popped) = arr.pop() {
                    let item_bytes = serde_json::to_string(&popped).map_or(0, |s| s.len() + 1);
                    current_len = current_len.saturating_sub(item_bytes);
                    omitted += 1;
                }
            }
        }
        if omitted > 0
            && let Some(obj) = payload.as_object_mut()
        {
            obj.insert("truncated".into(), json!(true));
            obj.insert("omitted_items".into(), json!(omitted));
        }
    }
    let retry = serde_json::to_string(&payload)?;
    if retry.len() <= INLINE_BUDGET {
        return Ok(retry);
    }
    // Final safety valve: drop bulky sample payload while preserving schema & counts.
    if let Some(obj) = payload.as_object_mut() {
        obj.remove("samples");
        obj.insert("truncated".into(), json!(true));
        obj.insert(
            "notice".into(),
            json!("samples omitted to fit <8KiB inline budget"),
        );
    }
    let minimal = serde_json::to_string(&payload)?;
    if minimal.len() <= INLINE_BUDGET {
        return Ok(minimal);
    }
    Ok(serde_json::to_string(&json!({
        "truncated": true,
        "notice": "probe summary exceeded inline budget; narrow with --path, --where, or --samples 0",
        "preview": clamp_utf8_bytes(&minimal, INLINE_BUDGET.saturating_sub(240)),
    }))?)
}

const SENSITIVE_PARAM_NAMES: &[&str] = &[
    "servicekey",
    "api_key",
    "apikey",
    "x-api-key",
    "access_token",
    "refresh_token",
    "id_token",
    "token",
    "secret",
    "client_secret",
    "password",
    "passwd",
    "pwd",
    "auth",
    "authorization",
    "sig",
    "signature",
    "key",
];

pub fn is_sensitive_key_name(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase().replace('-', "_");
    SENSITIVE_PARAM_NAMES
        .iter()
        .any(|needle| lower == needle.replace('-', "_") || lower.ends_with(&format!("_{needle}")))
}

pub fn is_sensitive_field_path(path: &str) -> bool {
    path.split(['.', '[', ']'])
        .filter(|seg| !seg.is_empty())
        .any(is_sensitive_key_name)
}

/// Reads at most `max_bytes` until `\n` (or EOF), preventing OOM on newline-free gigabyte inputs.
pub fn read_bounded_line<R: std::io::BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_bytes: usize,
) -> std::io::Result<usize> {
    buf.clear();
    let mut limited = reader.take((max_bytes as u64) + 1);
    let n = limited.read_until(b'\n', buf)?;
    if buf.len() > max_bytes && buf.last() != Some(&b'\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("single line exceeded maximum length of {max_bytes} bytes"),
        ));
    }
    Ok(n)
}

fn strip_url_userinfo(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let after_scheme = &url[scheme_end + 3..];
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    if let Some((_userinfo, host_port)) = authority.rsplit_once('@') {
        format!(
            "{}://<REDACTED>@{}{}",
            &url[..scheme_end],
            host_port,
            &after_scheme[authority_end..]
        )
    } else {
        url.to_string()
    }
}

/// Redacts URL userinfo, sensitive query parameters, and any explicit secret values.
pub fn redact_url(url: &str, extra_secrets: &[String]) -> String {
    let clean_auth = strip_url_userinfo(url);
    let scrubbed = if let Some((base, query_and_frag)) = clean_auth.split_once('?') {
        let (query, frag) = match query_and_frag.split_once('#') {
            Some((q, f)) => (q, Some(f)),
            None => (query_and_frag, None),
        };
        let parts: Vec<String> = query
            .split('&')
            .map(|pair| {
                if let Some((k, _v)) = pair.split_once('=') {
                    if is_sensitive_key_name(k) {
                        format!("{k}=<REDACTED>")
                    } else {
                        pair.to_string()
                    }
                } else {
                    pair.to_string()
                }
            })
            .collect();
        match frag {
            Some(f) => format!("{base}?{}#{f}", parts.join("&")),
            None => format!("{base}?{}", parts.join("&")),
        }
    } else {
        clean_auth
    };
    scrub_secrets(&scrubbed, extra_secrets)
}

pub fn scrub_secrets(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if secret.is_empty() {
            continue;
        }
        out = out.replace(secret, "<REDACTED>");
        let encoded_upper = percent_encode_hex(secret, true);
        if encoded_upper != *secret && !encoded_upper.is_empty() {
            out = out.replace(&encoded_upper, "<REDACTED>");
        }
        let encoded_lower = percent_encode_hex(secret, false);
        if encoded_lower != *secret && encoded_lower != encoded_upper && !encoded_lower.is_empty() {
            out = out.replace(&encoded_lower, "<REDACTED>");
        }
        if let Ok(json_quoted) = serde_json::to_string(secret)
            && json_quoted.len() >= 2
        {
            let json_inner = &json_quoted[1..json_quoted.len() - 1];
            if json_inner != secret && !json_inner.is_empty() {
                out = out.replace(json_inner, "<REDACTED>");
            }
        }
    }
    out
}

pub fn percent_encode_component(raw: &str) -> String {
    percent_encode_hex(raw, true)
}

fn percent_encode_hex(raw: &str, uppercase: bool) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else if uppercase {
            out.push_str(&format!("%{byte:02X}"));
        } else {
            out.push_str(&format!("%{byte:02x}"));
        }
    }
    out
}

pub fn run_probe(cmd: &ProbeCommand) -> Result<String> {
    if let ProbeCommand::Journal(j) = cmd {
        return crate::journal::run(&j.root, &j.command);
    }
    let raw = match cmd {
        ProbeCommand::Http(args) => http_probe::run(args)?,
        ProbeCommand::Json(args) => json_probe::run(args)?,
        ProbeCommand::Table(args) => table_probe::run(args)?,
        ProbeCommand::Image(args) => image_probe::run(args)?,
        ProbeCommand::Log(args) => log_probe::run_log(args)?,
        ProbeCommand::Batch(args) => log_probe::run_batch(args)?,
        ProbeCommand::Journal(_) => unreachable!(),
    };
    let is_json = match cmd {
        ProbeCommand::Http(a) => a.json,
        ProbeCommand::Json(a) => a.json,
        ProbeCommand::Table(a) => a.json,
        ProbeCommand::Image(a) => a.json,
        ProbeCommand::Log(a) => a.json,
        ProbeCommand::Batch(a) => a.json,
        ProbeCommand::Journal(_) => false,
    };
    if is_json {
        Ok(raw)
    } else {
        Ok(clamp_utf8_bytes(&raw, INLINE_BUDGET))
    }
}
