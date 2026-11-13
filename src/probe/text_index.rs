//! Structure-aware text chunker, secret scrubber, and Okapi BM25 + Trigram
//! Reciprocal Rank Fusion (`RRF_K = 60`) engine shared by `pctx log`, `pctx batch`,
//! and `cctx journal`.

use super::{is_sensitive_key_name, redact_url, scrub_secrets};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Maximum bytes per indexed text chunk (matches FTS chunking sweet spot while fitting multiple hits in `<8KiB`).
pub const DEFAULT_MAX_CHUNK_BYTES: usize = 3_200;

/// Reciprocal Rank Fusion constant (`1 / (RRF_K + rank)`).
pub const RRF_K: f64 = 60.0;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TextChunk {
    pub ord: usize,
    pub source_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub bytes: usize,
    pub body: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RankedHit {
    pub ord: usize,
    pub source_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub score: f64,
    pub matched_all_terms: bool,
    pub typo_corrected: bool,
    pub body: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AnomalyDigest {
    pub fatal_count: usize,
    pub error_count: usize,
    pub warn_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub highlights: Vec<String>,
}

/// Strips ANSI CSI/OSC terminal escape sequences so log chunks neither waste BPE tokens nor break matching.
pub fn strip_ansi_escapes(input: &str) -> String {
    if !input.as_bytes().contains(&0x1b) {
        return input.replace("\r\n", "\n").replace('\r', "\n");
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() != Some(&'\n') {
                out.push('\n');
            }
            continue;
        }
        if ch == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn looks_like_token_prefix(tok: &str) -> bool {
    let trimmed = tok.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';' | ')' | ']' | '}'));
    if trimmed.len() >= 16
        && (trimmed.starts_with("ghp_")
            || trimmed.starts_with("gho_")
            || trimmed.starts_with("ghu_")
            || trimmed.starts_with("ghs_")
            || trimmed.starts_with("github_pat_")
            || trimmed.starts_with("sk-")
            || trimmed.starts_with("xoxb-")
            || trimmed.starts_with("xoxp-")
            || trimmed.starts_with("eyJ"))
    {
        return true;
    }
    if trimmed.len() == 20
        && (trimmed.starts_with("AKIA") || trimmed.starts_with("ASIA"))
        && trimmed.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return true;
    }
    false
}

fn redact_token_preserving_punct(tok: &str) -> String {
    let lead_len = tok
        .chars()
        .take_while(|c| matches!(c, '"' | '\'' | '(' | '[' | '{'))
        .map(char::len_utf8)
        .sum::<usize>();
    let tail_len = tok[lead_len..]
        .chars()
        .rev()
        .take_while(|c| matches!(c, '"' | '\'' | ',' | ';' | ')' | ']' | '}'))
        .map(char::len_utf8)
        .sum::<usize>();
    let end = tok.len().saturating_sub(tail_len);
    format!("{}<REDACTED>{}", &tok[..lead_len], &tok[end..])
}

/// Redacts URLs, Bearer headers, GitHub/OpenAI/AWS tokens, and sensitive `KEY=VALUE` / `"key": "value"` pairs.
pub fn redact_unstructured_text(raw: &str, extra_secrets: &[String]) -> String {
    let cleaned = strip_ansi_escapes(raw);
    let mut out_lines = Vec::new();
    for line in cleaned.split('\n') {
        let mut redacted_line = redact_json_like_pairs(line);
        let tokens: Vec<&str> = redacted_line.split_whitespace().collect();
        let mut rebuilt = Vec::with_capacity(tokens.len());
        let mut redact_next = false;
        for (idx, &tok) in tokens.iter().enumerate() {
            if redact_next {
                rebuilt.push(redact_token_preserving_punct(tok));
                redact_next = false;
                continue;
            }
            let lower = tok.to_ascii_lowercase();
            if (lower == "bearer" || lower == "basic")
                && idx + 1 < tokens.len()
                && idx > 0
                && tokens[idx - 1]
                    .trim_end_matches(':')
                    .to_ascii_lowercase()
                    .contains("auth")
            {
                rebuilt.push(tok.to_string());
                redact_next = true;
                continue;
            }
            if lower == "bearer"
                && idx + 1 < tokens.len()
                && tokens[idx + 1].trim_matches(['"', '\'', ',']).len() >= 12
            {
                rebuilt.push(tok.to_string());
                redact_next = true;
                continue;
            }
            if tok.starts_with("http://") || tok.starts_with("https://") {
                let tail_len = tok
                    .chars()
                    .rev()
                    .take_while(|c| matches!(c, ')' | ']' | ',' | ';' | '"' | '\''))
                    .map(char::len_utf8)
                    .sum::<usize>();
                let core = &tok[..tok.len() - tail_len];
                let tail = &tok[tok.len() - tail_len..];
                rebuilt.push(format!("{}{tail}", redact_url(core, extra_secrets)));
                continue;
            }
            if looks_like_token_prefix(tok) {
                rebuilt.push(redact_token_preserving_punct(tok));
                continue;
            }
            if let Some((k, v)) = tok.split_once('=') {
                let clean_k = k
                    .trim_start_matches('-')
                    .trim_matches(|c: char| matches!(c, '"' | '\'' | '(' | '['));
                if !v.is_empty() && !clean_k.is_empty() && is_sensitive_key_name(clean_k) {
                    rebuilt.push(format!("{k}=<REDACTED>"));
                    continue;
                }
            }
            rebuilt.push(tok.to_string());
        }
        if !tokens.is_empty() {
            // Preserve leading indentation when tokens changed; otherwise keep exact spacing.
            let rebuilt_joined = rebuilt.join(" ");
            let orig_collapsed = tokens.join(" ");
            if rebuilt_joined != orig_collapsed {
                let indent_len = redacted_line.len() - redacted_line.trim_start().len();
                redacted_line = format!("{}{rebuilt_joined}", &redacted_line[..indent_len]);
            }
        }
        out_lines.push(redacted_line);
    }
    let joined = out_lines.join("\n");
    scrub_secrets(&joined, extra_secrets)
}

fn redact_json_like_pairs(line: &str) -> String {
    if !line.contains(':') || !line.contains('"') {
        return line.to_string();
    }
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'"'
            && let Some(key_end_off) = line[i + 1..].find('"')
        {
            let key_end = i + 1 + key_end_off;
            let key = &line[i + 1..key_end];
            let after_key = &line[key_end + 1..];
            let ws1 = after_key.len() - after_key.trim_start().len();
            if after_key[ws1..].starts_with(':') && is_sensitive_key_name(key) {
                let after_colon = &after_key[ws1 + 1..];
                let ws2 = after_colon.len() - after_colon.trim_start().len();
                if after_colon[ws2..].starts_with('"') {
                    let val_slice = &after_colon[ws2 + 1..];
                    let mut escaped = false;
                    let mut val_end_off = None;
                    for (off, ch) in val_slice.char_indices() {
                        if escaped {
                            escaped = false;
                            continue;
                        }
                        if ch == '\\' {
                            escaped = true;
                        } else if ch == '"' {
                            val_end_off = Some(off);
                            break;
                        }
                    }
                    if let Some(vend) = val_end_off {
                        out.push_str(&line[i..=key_end]);
                        out.push_str(&after_key[..ws1 + 1 + ws2]);
                        out.push_str("\"<REDACTED>\"");
                        i = key_end + 1 + ws1 + 1 + ws2 + 1 + vend + 1;
                        continue;
                    }
                }
            }
        }
        let ch = line[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Scans text lines to summarize `FATAL`/`PANIC`, `ERROR`/`FAIL`, and `WARN` counts with representative lines.
pub fn summarize_anomalies(text: &str, max_highlights: usize) -> AnomalyDigest {
    let mut digest = AnomalyDigest::default();
    for (idx, raw_line) in text.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        let line_no = idx + 1;
        let is_fatal = lower.contains("fatal")
            || lower.contains("panic")
            || lower.contains("segfault")
            || lower.contains("sigsegv")
            || lower.contains("oom_kill");
        let is_error = !is_fatal
            && (lower.contains("error")
                || lower.contains("exception")
                || lower.contains("traceback")
                || lower.starts_with("fail ")
                || lower.starts_with("failed")
                || lower.contains(" failed:")
                || lower.contains("[fail]"));
        let is_warn = !is_fatal && !is_error && (lower.contains("warn") || lower.contains("deprecated"));

        if is_fatal {
            digest.fatal_count += 1;
            if digest.highlights.len() < max_highlights {
                digest.highlights.push(format!(
                    "L{line_no} [FATAL]: {}",
                    super::truncate_utf8_chars(trimmed, 140)
                ));
            }
        } else if is_error {
            digest.error_count += 1;
            if digest.highlights.len() < max_highlights {
                digest.highlights.push(format!(
                    "L{line_no} [ERROR]: {}",
                    super::truncate_utf8_chars(trimmed, 140)
                ));
            }
        } else if is_warn {
            digest.warn_count += 1;
            if digest.highlights.len() < max_highlights && digest.fatal_count == 0 && digest.error_count == 0 {
                digest.highlights.push(format!(
                    "L{line_no} [WARN]: {}",
                    super::truncate_utf8_chars(trimmed, 140)
                ));
            }
        }
    }
    digest
}

fn extract_heading_title(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix('#') {
        let hashes = 1 + rest.chars().take_while(|&c| c == '#').count();
        if hashes <= 6 {
            let text = trimmed[hashes..].trim();
            if !text.is_empty() {
                return Some(super::truncate_utf8_chars(text, 100));
            }
        }
    }
    if (trimmed.starts_with("diff --git ")
        || trimmed.starts_with("commit ")
        || (trimmed.starts_with("=== ") && trimmed.ends_with(" ===")))
        && trimmed.len() <= 140
    {
        return Some(trimmed.to_string());
    }
    None
}

fn is_fence_delimiter(line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn is_stack_continuation(line: &str) -> bool {
    if line.starts_with("  ") || line.starts_with('\t') {
        let t = line.trim_start();
        return t.starts_with("at ")
            || t.starts_with("File \"")
            || t.starts_with("--> ")
            || t.starts_with("|")
            || t.starts_with("= note:")
            || t.starts_with("= help:")
            || t.starts_with("Caused by:");
    }
    let t = line.trim_start();
    t.starts_with("Caused by:") || t.starts_with("--> ")
}

/// Splits text into line-accurate chunks bounded by `max_chunk_bytes`, keeping Markdown
/// heading sections, fenced code blocks, and stack-trace continuations whole whenever they fit.
pub fn chunk_structured_text(
    source_label: &str,
    text: &str,
    max_chunk_bytes: usize,
) -> Vec<TextChunk> {
    let cap = max_chunk_bytes.max(256);
    if text.is_empty() {
        return Vec::new();
    }
    let raw_lines: Vec<&str> = text.split('\n').collect();
    // Group atomic blocks first: fenced code blocks or error+stacktrace runs that fit in `cap`.
    let mut blocks: Vec<(usize, usize, Option<String>, String)> = Vec::new();
    let mut current_heading: Option<String> = None;
    let mut i = 0usize;
    while i < raw_lines.len() {
        let line = raw_lines[i];
        if let Some(h) = extract_heading_title(line) {
            current_heading = Some(h);
            // Start a fresh boundary before major headings by marking this single line with the new heading
            blocks.push((i + 1, i + 1, current_heading.clone(), line.to_string()));
            i += 1;
            continue;
        }
        if let Some(delim) = is_fence_delimiter(line) {
            let start = i;
            let mut j = i + 1;
            let mut total_bytes = line.len();
            while j < raw_lines.len() {
                total_bytes += 1 + raw_lines[j].len();
                if is_fence_delimiter(raw_lines[j]) == Some(delim) {
                    j += 1;
                    break;
                }
                j += 1;
            }
            if total_bytes <= cap {
                let joined = raw_lines[start..j].join("\n");
                blocks.push((start + 1, j, current_heading.clone(), joined));
                i = j;
                continue;
            }
        }
        // Group a primary line with any immediate stack-trace continuation lines if they fit
        let start = i;
        let mut j = i + 1;
        let mut total_bytes = line.len();
        while j < raw_lines.len() && is_stack_continuation(raw_lines[j]) {
            let next_bytes = total_bytes + 1 + raw_lines[j].len();
            if next_bytes > cap {
                break;
            }
            total_bytes = next_bytes;
            j += 1;
        }
        if line.len() > cap {
            // Split oversized single line cleanly on UTF-8 boundaries
            let mut rem = line;
            while !rem.is_empty() {
                let mut end = cap.min(rem.len());
                while end > 0 && !rem.is_char_boundary(end) {
                    end -= 1;
                }
                if end == 0 {
                    end = rem.chars().next().map_or(rem.len(), char::len_utf8);
                }
                blocks.push((start + 1, start + 1, current_heading.clone(), rem[..end].to_string()));
                rem = &rem[end..];
            }
        } else {
            let joined = raw_lines[start..j].join("\n");
            blocks.push((start + 1, j, current_heading.clone(), joined));
        }
        i = j;
    }

    let mut chunks = Vec::new();
    let mut buf = String::new();
    let mut chunk_start = 1usize;
    let mut chunk_end = 1usize;
    let mut chunk_title: Option<String> = None;

    for (b_start, b_end, b_heading, b_text) in blocks {
        let is_new_heading = extract_heading_title(&b_text).is_some() && !buf.is_empty();
        let needed = if buf.is_empty() {
            b_text.len()
        } else {
            buf.len() + 1 + b_text.len()
        };
        if !buf.is_empty() && (needed > cap || is_new_heading) {
            let body = std::mem::take(&mut buf);
            chunks.push(TextChunk {
                ord: chunks.len(),
                source_label: source_label.to_string(),
                title: chunk_title.take(),
                start_line: chunk_start,
                end_line: chunk_end,
                bytes: body.len(),
                body,
            });
        }
        if buf.is_empty() {
            chunk_start = b_start;
            chunk_title = b_heading;
            buf.push_str(&b_text);
        } else {
            buf.push('\n');
            buf.push_str(&b_text);
        }
        chunk_end = b_end;
    }
    if !buf.is_empty() {
        chunks.push(TextChunk {
            ord: chunks.len(),
            source_label: source_label.to_string(),
            title: chunk_title,
            start_line: chunk_start,
            end_line: chunk_end,
            bytes: buf.len(),
            body: buf,
        });
    }
    chunks
}

fn stem_ascii_token(lower: &str) -> String {
    if lower.len() >= 6 && lower.ends_with("ing") {
        return lower[..lower.len() - 3].to_string();
    }
    if lower.len() >= 5 && lower.ends_with("ed") {
        return lower[..lower.len() - 2].to_string();
    }
    if lower.len() >= 5 && lower.ends_with("es") {
        return lower[..lower.len() - 2].to_string();
    }
    if lower.len() >= 5 && lower.ends_with('s') && !lower.ends_with("ss") {
        return lower[..lower.len() - 1].to_string();
    }
    lower.to_string()
}

fn strip_korean_josa(word: &str) -> &str {
    const JOSAS: &[&str] = &[
        "에서는", "으로부터", "에게서", "이라도", "에서", "으로", "에게", "까지", "부터", "처럼",
        "보다", "하고", "이나", "이며", "이고", "은", "는", "이", "가", "을", "를", "에", "의",
        "와", "과", "로", "도", "만",
    ];
    let char_count = word.chars().count();
    if char_count < 3 {
        return word;
    }
    for josa in JOSAS {
        if let Some(stem) = word.strip_suffix(josa)
            && stem.chars().count() >= 2
        {
            return stem;
        }
    }
    word
}

fn is_hangul_syllable(c: char) -> bool {
    ('\u{AC00}'..='\u{D7A3}').contains(&c)
}

/// Tokenizes text into normalized BM25 terms (identifier sub-parts, ASCII stems, Hangul stems + bigrams).
pub fn tokenize_bm25(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let flush = |buf: &mut String, out: &mut Vec<String>| {
        if buf.is_empty() {
            return;
        }
        let raw = std::mem::take(buf);
        emit_token_variants(&raw, out);
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || is_hangul_syllable(ch) || matches!(ch, '_' | '-' | '/' | '.') {
            current.push(ch);
        } else {
            flush(&mut current, &mut out);
        }
    }
    flush(&mut current, &mut out);
    out
}

/// Tokenizes a user query into primary distinct normalized terms (without over-expanding bigrams that break all-term checks).
pub fn tokenize_query_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut seen = BTreeSet::new();
    for part in query.split(|c: char| !(c.is_ascii_alphanumeric() || is_hangul_syllable(c) || c == '_' || c == '-')) {
        let trimmed = part.trim_matches(|c| c == '_' || c == '-');
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.chars().any(is_hangul_syllable) {
            let stem = strip_korean_josa(trimmed).to_string();
            if stem.chars().count() >= 2 && seen.insert(stem.clone()) {
                terms.push(stem);
            }
        } else {
            let lower = trimmed.to_ascii_lowercase();
            if lower.len() >= 2 {
                let stem = stem_ascii_token(&lower);
                if seen.insert(stem.clone()) {
                    terms.push(stem);
                }
            }
        }
    }
    terms
}

fn emit_token_variants(raw: &str, out: &mut Vec<String>) {
    let trimmed = raw.trim_matches(|c: char| matches!(c, '.' | '/' | '_' | '-'));
    if trimmed.is_empty() {
        return;
    }
    let full_lower = trimmed.to_ascii_lowercase();
    if (trimmed.contains('/') || trimmed.contains('.') || trimmed.contains('_') || trimmed.contains('-'))
        && full_lower.len() >= 3
    {
        out.push(full_lower);
    }
    for seg in trimmed.split(['/', '.', '_', '-']) {
        if seg.is_empty() {
            continue;
        }
        if seg.chars().any(is_hangul_syllable) {
            let stem = strip_korean_josa(seg);
            let chars: Vec<char> = stem.chars().collect();
            if !chars.is_empty() {
                out.push(stem.to_string());
                if chars.len() >= 3 {
                    for win in chars.windows(2) {
                        out.push(win.iter().collect());
                    }
                }
            }
            continue;
        }
        // Split camelCase segments as well as preserving full segment
        let seg_lower = seg.to_ascii_lowercase();
        if seg_lower.len() >= 2 {
            out.push(stem_ascii_token(&seg_lower));
        }
        let mut camel_parts: Vec<String> = Vec::new();
        let mut part = String::new();
        let mut prev_lower = false;
        for ch in seg.chars() {
            if prev_lower && ch.is_ascii_uppercase() && !part.is_empty() {
                camel_parts.push(std::mem::take(&mut part));
            }
            prev_lower = ch.is_ascii_lowercase();
            part.push(ch.to_ascii_lowercase());
        }
        if !part.is_empty() {
            camel_parts.push(part);
        }
        if camel_parts.len() > 1 {
            for cp in camel_parts {
                if cp.len() >= 2 {
                    out.push(stem_ascii_token(&cp));
                }
            }
        }
    }
}

/// Extracts unique character trigrams (lowercased) for substring, path, and partial-symbol matching.
pub fn extract_trigrams(text: &str) -> BTreeSet<[char; 3]> {
    let mut set = BTreeSet::new();
    let mut win = ['\0'; 3];
    let mut fill = 0usize;
    for ch in text.chars() {
        let norm = if ch.is_whitespace() {
            ' '
        } else {
            ch.to_ascii_lowercase()
        };
        if norm == ' ' && fill > 0 && win[fill - 1] == ' ' {
            continue;
        }
        if fill < 3 {
            win[fill] = norm;
            fill += 1;
            if fill == 3 {
                set.insert(win);
            }
        } else {
            win[0] = win[1];
            win[1] = win[2];
            win[2] = norm;
            set.insert(win);
        }
    }
    set
}

fn edit_distance_at_most_one(a: &str, b: &str) -> bool {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    if ac.len().abs_diff(bc.len()) > 1 {
        return false;
    }
    if ac.len() == bc.len() {
        let diffs = ac.iter().zip(bc.iter()).filter(|(x, y)| x != y).count();
        return diffs <= 1;
    }
    let (short, long) = if ac.len() < bc.len() { (&ac, &bc) } else { (&bc, &ac) };
    let mut si = 0usize;
    let mut li = 0usize;
    let mut skipped = false;
    while si < short.len() && li < long.len() {
        if short[si] == long[li] {
            si += 1;
            li += 1;
        } else if !skipped {
            skipped = true;
            li += 1;
        } else {
            return false;
        }
    }
    true
}

struct IndexedDoc {
    tf: HashMap<String, u32>,
    doc_len: f64,
    trigrams: BTreeSet<[char; 3]>,
}

pub struct TextIndex<'a> {
    chunks: &'a [TextChunk],
    docs: Vec<IndexedDoc>,
    df: HashMap<String, usize>,
    tri_df: HashMap<[char; 3], usize>,
    avg_dl: f64,
}

impl<'a> TextIndex<'a> {
    pub fn build(chunks: &'a [TextChunk]) -> Self {
        let mut docs = Vec::with_capacity(chunks.len());
        let mut df: HashMap<String, usize> = HashMap::new();
        let mut tri_df: HashMap<[char; 3], usize> = HashMap::new();
        let mut total_dl = 0.0f64;

        for chunk in chunks {
            let mut tf: HashMap<String, u32> = HashMap::new();
            let body_tokens = tokenize_bm25(&chunk.body);
            for tok in &body_tokens {
                *tf.entry(tok.clone()).or_insert(0) += 1;
            }
            let mut title_weight_len = 0usize;
            if let Some(title) = &chunk.title {
                let title_tokens = tokenize_bm25(title);
                title_weight_len = title_tokens.len();
                for tok in title_tokens {
                    *tf.entry(tok).or_insert(0) += 2;
                }
            }
            for term in tf.keys() {
                *df.entry(term.clone()).or_insert(0) += 1;
            }
            let combined = match &chunk.title {
                Some(t) => format!("{t}\n{}", chunk.body),
                None => chunk.body.clone(),
            };
            let trigrams = extract_trigrams(&combined);
            for &tri in &trigrams {
                *tri_df.entry(tri).or_insert(0) += 1;
            }
            let dl = (body_tokens.len() + title_weight_len * 2).max(1) as f64;
            total_dl += dl;
            docs.push(IndexedDoc {
                tf,
                doc_len: dl,
                trigrams,
            });
        }
        let avg_dl = if docs.is_empty() {
            1.0
        } else {
            (total_dl / docs.len() as f64).max(1.0)
        };
        Self {
            chunks,
            docs,
            df,
            tri_df,
            avg_dl,
        }
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<RankedHit> {
        if self.chunks.is_empty() || limit == 0 || query.trim().is_empty() {
            return Vec::new();
        }
        let raw_terms = tokenize_query_terms(query);
        let hits = self.search_with_terms(query, &raw_terms, limit, false);
        if !hits.is_empty() {
            return hits;
        }
        // Typo-correction fallback (edit distance <= 1 against indexed vocabulary for terms >= 4 chars)
        let mut corrected = Vec::new();
        let mut any_changed = false;
        for term in &raw_terms {
            if self.df.contains_key(term) || term.chars().count() < 4 {
                corrected.push(term.clone());
                continue;
            }
            let best = self
                .df
                .iter()
                .filter(|(cand, _)| edit_distance_at_most_one(term, cand))
                .max_by_key(|(_, count)| *count)
                .map(|(cand, _)| cand.clone());
            if let Some(replacement) = best {
                any_changed = true;
                corrected.push(replacement);
            } else {
                corrected.push(term.clone());
            }
        }
        if any_changed {
            let fallback_query = corrected.join(" ");
            return self.search_with_terms(&fallback_query, &corrected, limit, true);
        }
        Vec::new()
    }

    fn search_with_terms(
        &self,
        query_for_tri: &str,
        terms: &[String],
        limit: usize,
        typo_corrected: bool,
    ) -> Vec<RankedHit> {
        let n = self.docs.len() as f64;
        const K1: f64 = 1.2;
        const B: f64 = 0.75;

        // List A: Okapi BM25 ranking
        let mut bm25_scores: Vec<(usize, bool, f64)> = Vec::new();
        if !terms.is_empty() {
            for (idx, doc) in self.docs.iter().enumerate() {
                let mut score = 0.0f64;
                let mut matched_terms = 0usize;
                for term in terms {
                    let Some(&freq) = doc.tf.get(term) else {
                        continue;
                    };
                    matched_terms += 1;
                    let df = *self.df.get(term).unwrap_or(&1) as f64;
                    let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                    let tf = freq as f64;
                    let norm = tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * (doc.doc_len / self.avg_dl)));
                    score += idf * norm;
                }
                if matched_terms > 0 {
                    let all = matched_terms == terms.len();
                    bm25_scores.push((idx, all, score));
                }
            }
        }
        bm25_scores.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| b.2.total_cmp(&a.2))
                .then_with(|| a.0.cmp(&b.0))
        });

        // List B: IDF-weighted Trigram ranking (captures substrings, file paths, error codes)
        let q_trigrams: Vec<[char; 3]> = extract_trigrams(query_for_tri.trim())
            .into_iter()
            .filter(|tri| !tri.iter().all(|c| *c == ' '))
            .collect();
        let mut tri_scores: Vec<(usize, f64)> = Vec::new();
        if !q_trigrams.is_empty() {
            let min_tri_hits = if q_trigrams.len() <= 2 {
                q_trigrams.len()
            } else {
                (q_trigrams.len() / 2).max(2)
            };
            for (idx, doc) in self.docs.iter().enumerate() {
                let mut hit_count = 0usize;
                let mut tri_score = 0.0f64;
                for tri in &q_trigrams {
                    if doc.trigrams.contains(tri) {
                        hit_count += 1;
                        let df = *self.tri_df.get(tri).unwrap_or(&1) as f64;
                        tri_score += ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                    }
                }
                if hit_count >= min_tri_hits {
                    tri_scores.push((idx, tri_score));
                }
            }
        }
        tri_scores.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        // Fuse via Reciprocal Rank Fusion: score = sum(1 / (RRF_K + rank))
        let mut fused: BTreeMap<usize, (f64, bool)> = BTreeMap::new();
        for (rank0, &(idx, all, _)) in bm25_scores.iter().enumerate() {
            let rrf = 1.0 / (RRF_K + (rank0 + 1) as f64);
            let entry = fused.entry(idx).or_insert((0.0, false));
            entry.0 += rrf;
            entry.1 |= all;
        }
        for (rank0, &(idx, _)) in tri_scores.iter().enumerate() {
            let rrf = 1.0 / (RRF_K + (rank0 + 1) as f64);
            let entry = fused.entry(idx).or_insert((0.0, false));
            entry.0 += rrf;
        }

        let mut ranked: Vec<(usize, bool, f64)> = fused
            .into_iter()
            .map(|(idx, (score, all))| (idx, all, score))
            .collect();
        ranked.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| b.2.total_cmp(&a.2))
                .then_with(|| a.0.cmp(&b.0))
        });
        ranked
            .into_iter()
            .take(limit)
            .map(|(idx, matched_all_terms, score)| {
                let ch = &self.chunks[idx];
                RankedHit {
                    ord: ch.ord,
                    source_label: ch.source_label.clone(),
                    title: ch.title.clone(),
                    start_line: ch.start_line,
                    end_line: ch.end_line,
                    score: (score * 10_000.0).round() / 10_000.0,
                    matched_all_terms,
                    typo_corrected,
                    body: ch.body.clone(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_code_fences_and_stacktraces_intact_across_chunks() {
        let doc = format!(
            "# Overview\n{}\n## Code Example\n```rust\nfn retry_backoff(attempt: u32) -> u64 {{\n    (1_u64 << attempt).min(60)\n}}\n```\n## Crash Log\nerror[E0308]: mismatched types\n  --> src/engine.rs:42:9\n   |\n42 |     let x: u32 = \"bad\";\n   = note: expected `u32`, found `&str`\n",
            "filler line for section one\n".repeat(30)
        );
        let chunks = chunk_structured_text("guide.md", &doc, 600);
        let fence_chunk = chunks
            .iter()
            .find(|c| c.body.contains("fn retry_backoff"))
            .expect("fence chunk");
        assert!(fence_chunk.body.contains("```rust"));
        assert!(fence_chunk.body.ends_with("```"));
        assert_eq!(fence_chunk.title.as_deref(), Some("Code Example"));

        let err_chunk = chunks
            .iter()
            .find(|c| c.body.contains("error[E0308]"))
            .expect("error chunk");
        assert!(err_chunk.body.contains("--> src/engine.rs:42:9"));
        assert!(err_chunk.body.contains("expected `u32`, found `&str`"));
    }

    #[test]
    fn bm25_and_trigram_rrf_ranks_identifiers_hangul_and_typos() {
        let text = "\
# Section Alpha\n\
Normal startup complete on port 8080.\n\
# Compaction State\n\
컴팩션에서 핵심 의사결정과 src/resident.rs 파일 수정 내역을 보존합니다.\n\
# Auth Failure\n\
FATAL: authentication rejected for client E_CONN_RESET_99 on upstream.\n";
        let chunks = chunk_structured_text("session.log", text, 180);
        let index = TextIndex::build(&chunks);

        // Korean query with different josa inflections
        let ko_hits = index.search("컴팩션 의사결정을 보존", 2);
        assert!(!ko_hits.is_empty());
        assert!(ko_hits[0].body.contains("src/resident.rs"));

        // Substring path / code via trigram fusion
        let tri_hits = index.search("resident.rs", 2);
        assert!(!tri_hits.is_empty());
        assert_eq!(tri_hits[0].title.as_deref(), Some("Compaction State"));

        // Long typo ("authentcation") already resolves on pass 1 via trigram RRF overlap
        let long_typo_hits = index.search("authentcation", 2);
        assert!(!long_typo_hits.is_empty());
        assert!(long_typo_hits[0].body.contains("E_CONN_RESET_99"));

        // Short 4-char 1-edit typo ("fatl" -> "fatal") resolves via vocabulary Levenshtein <= 1 fallback
        let typo_hits = index.search("fatl", 2);
        assert!(!typo_hits.is_empty());
        assert!(typo_hits[0].typo_corrected);
        assert!(typo_hits[0].body.contains("E_CONN_RESET_99"));
    }

    #[test]
    fn redacts_tokens_bearer_headers_json_keys_and_ansi_codes() {
        let raw = "\x1b[31mERROR\x1b[0m Authorization: Bearer sk-proj-1234567890abcdefghij \
API_KEY=super_secret_val {\"access_token\": \"tok_live_99887766\"} https://admin:pw123@api.example.com/v1?token=xyz";
        let scrubbed = redact_unstructured_text(raw, &[]);
        assert!(!scrubbed.contains("\x1b["));
        assert!(!scrubbed.contains("sk-proj-1234567890abcdefghij"));
        assert!(!scrubbed.contains("super_secret_val"));
        assert!(!scrubbed.contains("tok_live_99887766"));
        assert!(!scrubbed.contains("pw123"));
        assert!(!scrubbed.contains("token=xyz"));
        assert!(scrubbed.contains("<REDACTED>"));
    }
}
