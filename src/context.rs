use crate::search::{Search, SearchResult, search};
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
        f.write_str("RESPONSE_BUDGET_EXCEEDED: response exceeds 256 KiB; narrow the request. For context reduce limit/radius or select one file; for reads use page=true or fewer ranges. No partial success was returned.")
    }
}
impl std::error::Error for ResponseBudgetError {}
pub(crate) fn is_response_budget(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<ResponseBudgetError>().is_some()
}

fn string_bound(value: &str) -> usize {
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

pub(crate) fn assemble(
    result: SearchResult,
    radius: usize,
    read: impl FnOnce(&[ReadRange]) -> Result<Vec<Snippet>>,
) -> Result<ContextResult> {
    let mut ranges: Vec<ReadRange> = Vec::new();
    for hit in &result.matches {
        let start = hit.line.saturating_sub(radius).max(1);
        let end = hit.line.saturating_add(radius);
        if let Some(last) = ranges.last_mut()
            && last.path == hit.path
            && start <= last.end.saturating_add(1)
            && end - last.start < 500
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
    let snippets = if ranges.is_empty() {
        Vec::new()
    } else {
        read(&ranges)?
    };
    for hit in &result.matches {
        let current = snippets
            .iter()
            .find(|s| s.path == hit.path && s.start <= hit.line && s.end >= hit.line)
            .and_then(|s| s.lines.get(hit.line - s.start));
        if current != Some(&hit.text) {
            return Err(invalid("file changed between search and read; retry the request").into());
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
