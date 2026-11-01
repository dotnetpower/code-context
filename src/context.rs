use crate::search::{Hit, Search, SearchResult, search};
use crate::workspace::{MAX_RESPONSE, Workspace};
use crate::{Result, invalid};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRange {
    pub path: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snippet {
    pub path: String,
    pub start: usize,
    pub end: usize,
    pub sha256: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    pub ranges: Vec<ReadRange>,
    #[serde(default = "default_split_ranges")]
    pub split_ranges: bool,
    #[serde(default)]
    pub page: bool,
}

fn default_split_ranges() -> bool {
    true
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRequest {
    #[serde(flatten)]
    pub search: Search,
    #[serde(default = "default_radius")]
    pub radius: usize,
}

fn default_radius() -> usize {
    3
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextResult {
    pub search: SearchResult,
    pub snippets: Vec<Snippet>,
    pub consistency: String,
}

pub fn read_many(workspace: &Workspace, ranges: &[ReadRange]) -> Result<Vec<Snippet>> {
    validate_ranges(ranges)?;
    let allowed = workspace.files()?;
    let mut loaded = BTreeMap::new();
    let mut snippets = Vec::new();
    for range in ranges {
        if !loaded.contains_key(&range.path) {
            let bytes = workspace.read_allowed(&range.path, &allowed)?;
            let hash = format!("{:x}", Sha256::digest(&bytes));
            let text = String::from_utf8(bytes)?;
            let lines: Vec<String> = text.split_terminator('\n').map(str::to_owned).collect();
            loaded.insert(range.path.clone(), (hash, lines));
        }
        let (hash, lines) = &loaded[&range.path];
        if range.start > lines.len() {
            return Err(invalid(format!(
                "READ_RANGE_OUT_OF_BOUNDS root={} path={} requested={}..{} lines={}: start exceeds EOF; locate current lines with code_context instead of repeating this range",
                workspace.root.display(), range.path, range.start, range.end, lines.len()
            )).into());
        }
        let end = range.end.min(lines.len());
        snippets.push(Snippet {
            path: range.path.clone(),
            start: range.start,
            end,
            sha256: hash.clone(),
            lines: lines[range.start - 1..end].to_vec(),
        });
    }
    check_snippets_size(&snippets)?;
    Ok(snippets)
}

fn ensure_size_bound(bound: usize) -> Result<()> {
    if bound > MAX_RESPONSE {
        return Err(ResponseBudgetError.into());
    }

    Ok(())
}

#[derive(Debug)]
struct ResponseBudgetError;
impl std::fmt::Display for ResponseBudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RESPONSE_BUDGET_EXCEEDED: response exceeds 256 KiB; narrow the request. For context reduce limit/radius, set max_matches_per_file=2, or select one file; for reads use page=true or fewer ranges (<=150 lines per call). No partial success was returned.")
    }
}
impl std::error::Error for ResponseBudgetError {}
pub(crate) fn is_response_budget(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<ResponseBudgetError>().is_some()
}

pub(crate) fn string_bound(value: &str) -> usize {
    2 + value.len()
        + value
            .bytes()
            .map(|byte| {
                if byte < 0x20 {
                    5
                } else if matches!(byte, b'"' | b'\\') {
                    1
                } else {
                    0
                }
            })
            .sum::<usize>()
}

pub(crate) fn check_search_size(result: &SearchResult) -> Result<()> {
    let matches = result
        .matches
        .iter()
        .map(|hit| 96 + string_bound(&hit.path) + string_bound(&hit.text))
        .sum::<usize>();
    ensure_size_bound(192 + string_bound(&result.consistency) + matches)
}

pub(crate) fn check_snippets_size(snippets: &[Snippet]) -> Result<()> {
    let size = snippets
        .iter()
        .map(|snippet| {
            192 + string_bound(&snippet.path)
                + string_bound(&snippet.sha256)
                + snippet
                    .lines
                    .iter()
                    .map(|line| 2 + string_bound(line))
                    .sum::<usize>()
        })
        .sum::<usize>();
    ensure_size_bound(64 + size)
}

fn check_context_size(result: &ContextResult) -> Result<()> {
    let search_matches = result
        .search
        .matches
        .iter()
        .map(|hit| 96 + string_bound(&hit.path) + string_bound(&hit.text))
        .sum::<usize>();
    let snippets = result
        .snippets
        .iter()
        .map(|snippet| {
            192 + string_bound(&snippet.path)
                + string_bound(&snippet.sha256)
                + snippet
                    .lines
                    .iter()
                    .map(|line| 2 + string_bound(line))
                    .sum::<usize>()
        })
        .sum::<usize>();
    ensure_size_bound(
        512 + string_bound(&result.consistency)
            + string_bound(&result.search.consistency)
            + search_matches
            + snippets,
    )
}

pub(crate) fn validate_ranges(ranges: &[ReadRange]) -> Result<()> {
    if ranges.is_empty() || ranges.len() > 64 {
        return Err(invalid("provide 1..64 ranges").into());
    }
    for range in ranges {
        if range.start == 0 || range.end < range.start || range.end - range.start >= 500 {
            return Err(invalid(format!(
                "READ_RANGE_LIMIT path={} requested={}..{}: ranges are 1-based inclusive, maximum 500 lines. Split this range into explicitly labeled batches of at most 500 lines; do not repeat the oversized request.",
                range.path, range.start, range.end
            )).into());
        }
    }
    Ok(())
}

pub fn context(workspace: &Workspace, request: &ContextRequest) -> Result<ContextResult> {
    validate_context(request)?;
    assemble(
        search(workspace, &request.search)?,
        request.radius,
        |ranges| read_many(workspace, ranges),
    )
}

pub(crate) fn validate_context(request: &ContextRequest) -> Result<()> {
    if request.radius > 100 || request.search.limit > 64 {
        return Err(invalid(
            "context radius must be 0..100 and search limit 1..64; use code_read for larger ranges",
        )
        .into());
    }
    Ok(())
}

fn is_python_top_header(line: &str) -> bool {
    [
        "def ",
        "async def ",
        "class ",
        "type ",
        "@",
        "if __name__",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

fn is_end_keyword_language_path(path: &str) -> bool {
    [".rb", ".ex", ".exs", ".lua", ".sh", ".bash", ".zsh", ".jl", ".cr"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

fn is_makefile_or_justfile_path(path: &str) -> bool {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        file_name,
        "Makefile" | "makefile" | "GNUmakefile" | "Justfile" | "justfile"
    ) || path.ends_with(".mk")
        || path.ends_with(".just")
}

fn is_dockerfile_path(path: &str) -> bool {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    file_name == "Dockerfile"
        || file_name == "Containerfile"
        || file_name.starts_with("Dockerfile.")
        || file_name.starts_with("Containerfile.")
        || path.ends_with(".dockerfile")
}

fn is_makefile_target_header(line: &str) -> bool {
    if leading_indent_cols(line) > 0 {
        return false;
    }
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('#')
        || trimmed.starts_with('@')
        || trimmed.starts_with("ifeq")
        || trimmed.starts_with("ifneq")
        || trimmed.starts_with("ifdef")
        || trimmed.starts_with("ifndef")
        || trimmed.starts_with("else")
        || trimmed.starts_with("endif")
        || trimmed.starts_with("include ")
        || trimmed.starts_with("-include ")
    {
        return false;
    }
    if let Some((before_colon, after_colon)) = trimmed.split_once(':') {
        !before_colon.is_empty()
            && !before_colon.contains(['=', ' ', '\t'])
            && !after_colon.starts_with('=')
    } else {
        false
    }
}

fn is_sql_statement_start(line: &str) -> bool {
    if leading_indent_cols(line) > 2 {
        return false;
    }
    let trimmed = crate::query::trim_line_start(line);
    let upper = trimmed.get(..36).unwrap_or(trimmed).to_ascii_uppercase();
    [
        "CREATE ",
        "ALTER ",
        "DROP ",
        "INSERT INTO ",
        "UPDATE ",
        "DELETE FROM ",
        "SELECT ",
        "WITH ",
    ]
    .iter()
    .any(|prefix| upper.starts_with(prefix))
}

fn is_sql_statement_end(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.ends_with(';')
        || trimmed.eq_ignore_ascii_case("GO")
        || trimmed.ends_with("$$;")
        || trimmed.ends_with("END;")
        || trimmed.ends_with("end;")
}

fn leading_indent_cols(line: &str) -> usize {
    let line = line.strip_prefix('\u{feff}').unwrap_or(line);
    let mut cols = 0usize;
    for b in line.bytes() {
        match b {
            b' ' => cols += 1,
            b'\t' => cols += 4,
            _ => break,
        }
    }
    cols
}

fn is_frontend_block_header(trimmed: &str) -> bool {
    [
        "useEffect(",
        "useLayoutEffect(",
        "useMemo(",
        "useCallback(",
        "React.useEffect(",
        "React.useLayoutEffect(",
        "describe(",
        "describe.each(",
        "test(",
        "test.each(",
        "test.describe(",
        "it(",
        "it.each(",
        "beforeEach(",
        "afterEach(",
        "configResolved(",
        "configureServer(",
        "transform(",
        "resolveId(",
        "handleHotUpdate(",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
}

fn is_block_definition_header(line: &str) -> bool {
    let trimmed = crate::query::trim_line_start(line);
    if trimmed.is_empty() || leading_indent_cols(line) > 4 {
        return false;
    }
    crate::query::is_definition_line(line) || is_frontend_block_header(trimmed)
}

fn is_leading_doc_or_attr(line: &str) -> bool {
    if leading_indent_cols(line) > 4 {
        return false;
    }
    let trimmed = crate::query::trim_line_start(line);
    trimmed.starts_with("#[")
        || trimmed.starts_with("# ")
        || trimmed.starts_with("//")
        || trimmed.starts_with("-- ")
        || trimmed.starts_with("/**")
        || trimmed.starts_with("/*")
        || trimmed == "*"
        || trimmed.starts_with("* ")
        || trimmed.starts_with("*/")
        || trimmed.starts_with('@')
        || trimmed.starts_with("\"use client\"")
        || trimmed.starts_with("'use client'")
        || trimmed.starts_with("\"use server\"")
        || trimmed.starts_with("'use server'")
}

fn is_sfc_section_open(line: &str) -> bool {
    let trimmed = crate::query::trim_line_start(line);
    leading_indent_cols(line) == 0
        && (trimmed == "<script>"
            || trimmed.starts_with("<script ")
            || trimmed == "<template>"
            || trimmed.starts_with("<template ")
            || trimmed == "<style>"
            || trimmed.starts_with("<style ")
            || trimmed == "---")
}

fn is_sfc_section_close(line: &str) -> bool {
    let trimmed = crate::query::trim_line_start(line).trim_end();
    leading_indent_cols(line) == 0
        && (trimmed.starts_with("</script>")
            || trimmed.starts_with("</template>")
            || trimmed.starts_with("</style>")
            || trimmed == "---")
}

fn is_frontend_component_path(path: &str) -> bool {
    path.ends_with(".tsx")
        || path.ends_with(".jsx")
        || path.ends_with(".vue")
        || path.ends_with(".svelte")
        || path.ends_with(".astro")
}

fn is_multiline_jsx_or_call_open(line: &str) -> bool {
    let trimmed = line.trim_end();
    if trimmed.ends_with(['(', ',', '{', '<', '='])
        || trimmed.ends_with("=>")
        || trimmed.ends_with("=> (")
    {
        return true;
    }
    let start = crate::query::trim_line_start(trimmed);
    start.starts_with('<')
        && !start.starts_with("</")
        && !start.starts_with("<!--")
        && !trimmed.ends_with('>')
}

fn clamp_snippet_to_block(snippet: &mut Snippet, hits: &[Hit], radius: usize) {
    let is_doc = crate::document::is_document_path(&snippet.path);
    if (!is_doc && radius < 6) || (is_doc && radius == 0) || snippet.lines.len() <= 2 {
        return;
    }
    let mut min_hit = usize::MAX;
    let mut max_hit = 0usize;
    for hit in hits {
        if hit.path == snippet.path && hit.line >= snippet.start && hit.line <= snippet.end {
            min_hit = min_hit.min(hit.line);
            max_hit = max_hit.max(hit.line);
        }
    }
    if min_hit > max_hit {
        return;
    }
    let min_idx = min_hit - snippet.start;
    let max_idx = max_hit - snippet.start;
    let mut new_start_idx = 0usize;
    let mut new_end_idx = snippet.lines.len() - 1;
    if crate::document::is_document_path(&snippet.path)
        && let Some((doc_start, doc_end)) = crate::document::clamp_document_snippet(
            &snippet.path,
            &snippet.lines,
            min_idx,
            max_idx,
        )
    {
        new_start_idx = doc_start;
        new_end_idx = doc_end;
    } else if snippet.path.ends_with(".py") || snippet.path.ends_with(".pyi") {
        let is_py_header = |line: &str| {
            leading_indent_cols(line) <= 4 && is_python_top_header(line.trim_start())
        };
        for idx in (1..=min_idx).rev() {
            if is_py_header(&snippet.lines[idx]) {
                let mut header_start = idx;
                while header_start > 0 {
                    let prev = &snippet.lines[header_start - 1];
                    let prev_trimmed = prev.trim_start();
                    if (leading_indent_cols(prev) <= 4 && prev_trimmed.starts_with('@'))
                        || (header_start >= 2
                            && (prev_trimmed.starts_with(')') || leading_indent_cols(prev) > 0)
                            && snippet.lines[..header_start]
                                .iter()
                                .rev()
                                .take(6)
                                .any(|l| leading_indent_cols(l) <= 4 && l.trim_start().starts_with('@')))
                    {
                        header_start -= 1;
                    } else {
                        break;
                    }
                }
                if header_start > 0 && snippet.lines[header_start - 1].trim().is_empty() {
                    new_start_idx = header_start;
                    break;
                }
            }
        }
        for idx in (max_idx + 1)..snippet.lines.len() {
            if is_py_header(&snippet.lines[idx])
                && snippet.lines[idx - 1].trim().is_empty()
            {
                let mut end_idx = idx - 1;
                while end_idx > max_idx && snippet.lines[end_idx].trim().is_empty() {
                    end_idx -= 1;
                }
                new_end_idx = end_idx;
                break;
            }
        }
    } else if snippet.path.ends_with(".sql") {
        for idx in (1..=min_idx).rev() {
            if is_sql_statement_start(&snippet.lines[idx])
                && (snippet.lines[idx - 1].trim().is_empty()
                    || is_sql_statement_end(&snippet.lines[idx - 1])
                    || snippet.lines[idx - 1].trim_start().starts_with("--"))
            {
                let mut header_start = idx;
                while header_start > 0
                    && snippet.lines[header_start - 1].trim_start().starts_with("--")
                {
                    header_start -= 1;
                }
                new_start_idx = header_start;
                break;
            }
        }
        for idx in max_idx..snippet.lines.len() {
            if is_sql_statement_end(&snippet.lines[idx]) {
                new_end_idx = idx;
                break;
            }
            if idx > max_idx
                && is_sql_statement_start(&snippet.lines[idx])
                && snippet.lines[idx - 1].trim().is_empty()
            {
                let mut end_idx = idx - 1;
                while end_idx > max_idx && snippet.lines[end_idx].trim().is_empty() {
                    end_idx -= 1;
                }
                new_end_idx = end_idx;
                break;
            }
        }
    } else if is_makefile_or_justfile_path(&snippet.path) {
        for idx in (1..=min_idx).rev() {
            if is_makefile_target_header(&snippet.lines[idx]) {
                let mut header_start = idx;
                while header_start > 0
                    && snippet.lines[header_start - 1].trim_start().starts_with('#')
                {
                    header_start -= 1;
                }
                if header_start == 0 || snippet.lines[header_start - 1].trim().is_empty() {
                    new_start_idx = header_start;
                    break;
                }
            }
        }
        for idx in (max_idx + 1)..snippet.lines.len() {
            if is_makefile_target_header(&snippet.lines[idx])
                && snippet.lines[idx - 1].trim().is_empty()
            {
                let mut end_idx = idx - 1;
                while end_idx > max_idx && snippet.lines[end_idx].trim().is_empty() {
                    end_idx -= 1;
                }
                new_end_idx = end_idx;
                break;
            }
        }
    } else if is_dockerfile_path(&snippet.path) {
        let is_from_line = |line: &str| {
            leading_indent_cols(line) == 0
                && line
                    .trim_start()
                    .get(..5)
                    .is_some_and(|p| p.eq_ignore_ascii_case("FROM "))
        };
        for idx in (1..=min_idx).rev() {
            if is_from_line(&snippet.lines[idx]) {
                let mut header_start = idx;
                while header_start > 0
                    && snippet.lines[header_start - 1].trim_start().starts_with('#')
                {
                    header_start -= 1;
                }
                new_start_idx = header_start;
                break;
            }
        }
        for idx in (max_idx + 1)..snippet.lines.len() {
            if is_from_line(&snippet.lines[idx]) {
                let mut end_idx = idx - 1;
                while end_idx > max_idx
                    && (snippet.lines[end_idx].trim().is_empty()
                        || snippet.lines[end_idx].trim_start().starts_with('#'))
                {
                    end_idx -= 1;
                }
                new_end_idx = end_idx;
                break;
            }
        }
    } else {
        let is_sfc = snippet.path.ends_with(".vue")
            || snippet.path.ends_with(".svelte")
            || snippet.path.ends_with(".astro");
        let is_end_lang = is_end_keyword_language_path(&snippet.path);
        let is_block_close = |line: &str| {
            if leading_indent_cols(line) > 4 {
                return false;
            }
            let trimmed = line.trim();
            trimmed == "}"
                || trimmed == "};"
                || trimmed == "});"
                || trimmed == "})"
                || trimmed == "]);"
                || trimmed == "])"
                || trimmed == "));"
                || trimmed == "))"
                || trimmed.starts_with("} satisfies ")
                || trimmed.starts_with("} as ")
                || trimmed.starts_with("}; //")
                || trimmed.starts_with("}); //")
                || (is_end_lang
                    && (trimmed == "end"
                        || trimmed == "fi"
                        || trimmed == "done"
                        || trimmed == "esac"))
        };
        for idx in (1..=min_idx).rev() {
            if is_sfc && is_sfc_section_open(&snippet.lines[idx]) {
                new_start_idx = idx;
                break;
            }
            if is_sfc && idx < min_idx && is_sfc_section_close(&snippet.lines[idx]) {
                let mut candidate = idx + 1;
                while candidate < min_idx && snippet.lines[candidate].trim().is_empty() {
                    candidate += 1;
                }
                new_start_idx = candidate;
                break;
            }
            if is_block_definition_header(&snippet.lines[idx]) {
                let mut header_start = idx;
                while header_start > 0 && is_leading_doc_or_attr(&snippet.lines[header_start - 1]) {
                    header_start -= 1;
                }
                if header_start > 0
                    && (snippet.lines[header_start - 1].trim().is_empty()
                        || (is_sfc && is_sfc_section_open(&snippet.lines[header_start - 1])))
                {
                    new_start_idx = header_start;
                    break;
                }
            }
            if idx < min_idx && is_block_close(&snippet.lines[idx]) {
                let mut candidate = idx + 1;
                while candidate < min_idx && snippet.lines[candidate].trim().is_empty() {
                    candidate += 1;
                }
                new_start_idx = candidate;
                break;
            }
        }
        if new_start_idx == 0 && min_idx > 0 && is_block_close(&snippet.lines[0]) {
            let mut candidate = 1;
            while candidate < min_idx && snippet.lines[candidate].trim().is_empty() {
                candidate += 1;
            }
            new_start_idx = candidate;
        }
        for idx in max_idx..snippet.lines.len().saturating_sub(1) {
            if is_sfc && idx > max_idx && is_sfc_section_close(&snippet.lines[idx]) {
                new_end_idx = idx;
                break;
            }
            if is_sfc && idx > max_idx && is_sfc_section_open(&snippet.lines[idx]) {
                let mut end_idx = idx - 1;
                while end_idx > max_idx && snippet.lines[end_idx].trim().is_empty() {
                    end_idx -= 1;
                }
                new_end_idx = end_idx;
                break;
            }
            if is_block_close(&snippet.lines[idx])
                && (snippet.lines[idx + 1].trim().is_empty()
                    || (is_sfc && is_sfc_section_close(&snippet.lines[idx + 1])))
            {
                new_end_idx = idx;
                break;
            }
            if idx > max_idx
                && is_block_definition_header(&snippet.lines[idx])
                && snippet.lines[idx - 1].trim().is_empty()
            {
                let mut end_idx = idx - 1;
                while end_idx > max_idx && snippet.lines[end_idx].trim().is_empty() {
                    end_idx -= 1;
                }
                new_end_idx = end_idx;
                break;
            }
        }
    }
    while new_start_idx < min_idx && snippet.lines[new_start_idx].trim().is_empty() {
        new_start_idx += 1;
    }
    while new_end_idx > max_idx && snippet.lines[new_end_idx].trim().is_empty() {
        new_end_idx -= 1;
    }
    if (new_start_idx > 0 || new_end_idx + 1 < snippet.lines.len())
        && new_start_idx <= min_idx
        && max_idx <= new_end_idx
    {
        snippet.lines.truncate(new_end_idx + 1);
        if new_start_idx > 0 {
            snippet.lines.drain(..new_start_idx);
        }
        snippet.start += new_start_idx;
        snippet.end = snippet.start + snippet.lines.len() - 1;
    }
}

pub(crate) fn assemble(
    result: SearchResult,
    radius: usize,
    read: impl FnOnce(&[ReadRange]) -> Result<Vec<Snippet>>,
) -> Result<ContextResult> {
    assemble_inner(result, radius, true, read)
}

pub(crate) fn assemble_trusted(
    result: SearchResult,
    radius: usize,
    read: impl FnOnce(&[ReadRange]) -> Result<Vec<Snippet>>,
) -> Result<ContextResult> {
    assemble_inner(result, radius, false, read)
}

fn assemble_inner(
    result: SearchResult,
    radius: usize,
    verify_lines: bool,
    read: impl FnOnce(&[ReadRange]) -> Result<Vec<Snippet>>,
) -> Result<ContextResult> {
    let has_definition_hit = radius >= 6
        && result.matches.iter().any(|hit| {
            !crate::document::is_document_path(&hit.path)
                && (crate::query::is_definition_line(&hit.text)
                    || is_python_top_header(hit.text.trim_start()))
        });
    let mut ranges: Vec<ReadRange> = Vec::with_capacity(result.matches.len());
    for hit in &result.matches {
        let (above, below, max_span) = if has_definition_hit {
            let is_doc = crate::document::is_document_path(&hit.path);
            let is_def = crate::query::is_definition_line(&hit.text)
                || is_python_top_header(hit.text.trim_start())
                || (is_doc && crate::document::is_document_definition_line(&hit.text));
            if is_def {
                if is_doc {
                    (1usize, radius.min(10), 24usize)
                } else if is_frontend_component_path(&hit.path) {
                    (1usize, radius.min(7), 20usize)
                } else {
                    (1usize, radius.min(5), 16usize)
                }
            } else if is_doc {
                (radius.min(3), radius.min(5), 16usize)
            } else if result.matches.len() <= 8 && is_multiline_jsx_or_call_open(&hit.text) {
                (0usize, 2usize, 4usize)
            } else {
                (0usize, 0usize, 2usize)
            }
        } else {
            (radius, radius, 500usize)
        };
        let start = hit.line.saturating_sub(above).max(1);
        let end = hit.line.saturating_add(below);
        if let Some(last) = ranges.last_mut()
            && last.path == hit.path
            && start <= last.end.saturating_add(if has_definition_hit { 2 } else { 1 })
            && end - last.start < max_span
        {
            last.end = last.end.max(end);
        } else {
            ranges.push(ReadRange {
                path: hit.path.clone(),
                start,
                end,
            });
        }
    }
    let mut snippets = if ranges.is_empty() {
        Vec::new()
    } else {
        read(&ranges)?
    };
    for snippet in &mut snippets {
        clamp_snippet_to_block(snippet, &result.matches, radius);
    }
    coalesce_adjacent_snippets(&mut snippets);
    if verify_lines {
        for hit in &result.matches {
            let current = snippets
                .iter()
                .find(|s| s.path == hit.path && s.start <= hit.line && s.end >= hit.line)
                .and_then(|s| s.lines.get(hit.line - s.start));
            if current != Some(&hit.text) {
                return Err(
                    invalid("file changed between search and read; retry the request").into(),
                );
            }
        }
    }
    let consistency = format!(
        "{}; matched lines checked against read; hashes describe read versions; revalidate live files before editing",
        result.consistency
    );
    let output = ContextResult {
        search: result,
        snippets,
        consistency,
    };
    check_context_size(&output)?;
    Ok(output)
}

fn coalesce_adjacent_snippets(snippets: &mut Vec<Snippet>) {
    if snippets.len() <= 1 {
        return;
    }
    let mut merged: Vec<Snippet> = Vec::with_capacity(snippets.len());
    for current in snippets.drain(..) {
        if let Some(prev) = merged.last_mut()
            && prev.path == current.path
            && prev.sha256 == current.sha256
            && current.start >= prev.start
            && current.start <= prev.end.saturating_add(1)
            && current.end.max(prev.end) - prev.start < 500
        {
            if current.end > prev.end {
                let skip = (prev.end + 1).saturating_sub(current.start);
                if skip < current.lines.len() {
                    prev.lines.extend_from_slice(&current.lines[skip..]);
                    prev.end = current.end;
                }
            }
        } else {
            merged.push(current);
        }
    }
    *snippets = merged;
}
