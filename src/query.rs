use crate::context::{ContextResult, Snippet, check_search_size};
use crate::planner::{self, CandidateExpr};
use crate::policy::WORKER_STACK_BYTES;
use crate::resident::Resident;
use crate::search::{Hit, Search, SearchResult};
use crate::workspace::{normalize, safe_relative};
use crate::{Result, invalid};
use globset::{Glob, GlobSet, GlobSetBuilder};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use memchr::memmem::Finder;
use regex_syntax::hir::{Hir, HirKind, Look};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::Path;
use std::time::Instant;

const CACHE_ENTRIES: usize = 8;
const CACHE_PATTERN_BYTES: usize = 256 * 1024;
const MATCHER_MEMORY_LIMIT: usize = 4 * 1024 * 1024;
const MATCHER_MEMORY_ESTIMATE: usize = MATCHER_MEMORY_LIMIT * 2;
const RESULT_CACHE_ENTRIES: usize = 32;
const RESULT_CACHE_BYTES: usize = 4 * 1024 * 1024;
const CONTEXT_CACHE_ENTRIES: usize = 16;
const CONTEXT_CACHE_BYTES: usize = 4 * 1024 * 1024;
const INDEX_OPERATION_BYTE_EQUIVALENT: usize = 64;
// Tuning queries showed no reliable win once candidate bytes exceeded 75% of
// the eligible corpus. Holdout queries are not used to choose this threshold.
const MAX_CANDIDATE_FRACTION_NUMERATOR: usize = 3;
const MAX_CANDIDATE_FRACTION_DENOMINATOR: usize = 4;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub pattern: String,
    #[serde(default)]
    pub regex: bool,
    #[serde(default)]
    pub ignore_case: bool,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub max_matches_per_file: Option<usize>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct QueryProfile {
    pub cache_hit: bool,
    pub compile_us: u64,
    pub filter_us: u64,
    pub plan_us: u64,
    pub scan_us: u64,
    pub finalize_us: u64,
    pub eligible_files: usize,
    pub eligible_bytes: usize,
    pub candidate_files: usize,
    pub candidate_bytes: usize,
    pub posting_lookups: usize,
    pub set_operations: usize,
    pub used_index: bool,
    pub line_prefilter: bool,
    pub parallel_workers: usize,
    pub plan: String,
    pub fallback: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProfiledResult {
    pub result: SearchResult,
    pub profile: QueryProfile,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CacheKey {
    pattern: String,
    regex: bool,
    ignore_case: bool,
}

struct Compiled {
    matcher: Option<RegexMatcher>,
    per_line: bool,
    manual_line_prefilter: bool,
    plan: Option<CandidateExpr>,
}

struct CacheEntry {
    key: CacheKey,
    compiled: Compiled,
}

struct ResultEntry {
    generation: u64,
    query: Query,
    result: SearchResult,
    owned_bytes: usize,
}

struct ContextEntry {
    generation: u64,
    query: Query,
    radius: usize,
    result: ContextResult,
    owned_bytes: usize,
}

#[derive(Default)]
pub struct QueryCache {
    entries: VecDeque<CacheEntry>,
    pattern_bytes: usize,
    results: VecDeque<ResultEntry>,
    result_bytes: usize,
    contexts: VecDeque<ContextEntry>,
    context_bytes: usize,
}

impl QueryCache {
    fn get_or_compile(&mut self, query: &Query) -> Result<(&Compiled, bool, u64)> {
        let key = CacheKey {
            pattern: query.pattern.clone(),
            regex: query.regex,
            ignore_case: query.ignore_case,
        };
        if let Some(position) = self.entries.iter().position(|entry| entry.key == key) {
            let entry = self
                .entries
                .remove(position)
                .expect("cache position exists");
            self.entries.push_back(entry);
            return Ok((&self.entries.back().expect("cache entry").compiled, true, 0));
        }
        let started = Instant::now();
        let compiled = compile(&key)?;
        let compile_us = micros(started);
        self.pattern_bytes += key.pattern.len();
        self.entries.push_back(CacheEntry { key, compiled });
        while self.entries.len() > CACHE_ENTRIES || self.pattern_bytes > CACHE_PATTERN_BYTES {
            if let Some(entry) = self.entries.pop_front() {
                self.pattern_bytes = self.pattern_bytes.saturating_sub(entry.key.pattern.len());
            }
        }
        Ok((
            &self.entries.back().expect("new cache entry").compiled,
            false,
            compile_us,
        ))
    }

    pub fn stats(&self) -> serde_json::Value {
        serde_json::json!({
            "entries":self.entries.len(),
            "pattern_bytes":self.pattern_bytes,
            "entry_limit":CACHE_ENTRIES,
            "pattern_byte_limit":CACHE_PATTERN_BYTES,
            "result_entries":self.results.len(),
            "result_bytes":self.result_bytes,
            "result_entry_limit":RESULT_CACHE_ENTRIES,
            "result_byte_limit":RESULT_CACHE_BYTES
            ,"context_entries":self.contexts.len()
            ,"context_bytes":self.context_bytes
            ,"context_entry_limit":CONTEXT_CACHE_ENTRIES
            ,"context_byte_limit":CONTEXT_CACHE_BYTES
        })
    }

    pub fn estimated_bytes(&self) -> usize {
        self.pattern_bytes
            .saturating_add(self.result_bytes)
            .saturating_add(self.context_bytes)
            .saturating_add(self.entries.len().saturating_mul(MATCHER_MEMORY_ESTIMATE))
    }

    pub fn retain_generation(&mut self, generation: u64) {
        self.results.retain(|entry| entry.generation == generation);
        self.result_bytes = self.results.iter().map(|entry| entry.owned_bytes).sum();
        self.contexts.retain(|entry| entry.generation == generation);
        self.context_bytes = self.contexts.iter().map(|entry| entry.owned_bytes).sum();
    }

    pub fn clear_memory(&mut self) {
        self.entries.clear();
        self.pattern_bytes = 0;
        self.results.clear();
        self.result_bytes = 0;
        self.contexts.clear();
        self.context_bytes = 0;
    }

    fn get_result(&mut self, generation: u64, query: &Query) -> Option<SearchResult> {
        let position = self
            .results
            .iter()
            .position(|entry| entry.generation == generation && &entry.query == query)?;
        let entry = self.results.remove(position)?;
        let result = entry.result.clone();
        self.results.push_back(entry);
        Some(result)
    }

    fn put_result(&mut self, generation: u64, query: Query, result: SearchResult) {
        let owned_bytes = query_owned_bytes(&query)
            + result.consistency.capacity()
            + result.matches.capacity() * std::mem::size_of::<Hit>()
            + result
                .matches
                .iter()
                .map(|hit| hit.path.capacity() + hit.text.capacity())
                .sum::<usize>();
        if owned_bytes > RESULT_CACHE_BYTES {
            return;
        }
        self.result_bytes += owned_bytes;
        self.results.push_back(ResultEntry {
            generation,
            query,
            result,
            owned_bytes,
        });
        while self.results.len() > RESULT_CACHE_ENTRIES || self.result_bytes > RESULT_CACHE_BYTES {
            if let Some(entry) = self.results.pop_front() {
                self.result_bytes = self.result_bytes.saturating_sub(entry.owned_bytes);
            }
        }
    }

    pub(crate) fn get_context(
        &mut self,
        generation: u64,
        query: &Query,
        radius: usize,
    ) -> Option<ContextResult> {
        let position = self.contexts.iter().position(|entry| {
            entry.generation == generation && &entry.query == query && entry.radius == radius
        })?;
        let entry = self.contexts.remove(position)?;
        let result = entry.result.clone();
        self.contexts.push_back(entry);
        Some(result)
    }

    pub(crate) fn put_context(
        &mut self,
        generation: u64,
        query: Query,
        radius: usize,
        result: ContextResult,
    ) {
        let owned_bytes = query_owned_bytes(&query)
            + result.consistency.capacity()
            + result.search.consistency.capacity()
            + result.search.matches.capacity() * std::mem::size_of::<Hit>()
            + result
                .search
                .matches
                .iter()
                .map(|hit| hit.path.capacity() + hit.text.capacity())
                .sum::<usize>()
            + result.snippets.capacity() * std::mem::size_of::<Snippet>()
            + result
                .snippets
                .iter()
                .map(|snippet| {
                    snippet.path.capacity()
                        + snippet.sha256.capacity()
                        + snippet.lines.capacity() * std::mem::size_of::<String>()
                        + snippet.lines.iter().map(String::capacity).sum::<usize>()
                })
                .sum::<usize>();
        if owned_bytes > CONTEXT_CACHE_BYTES {
            return;
        }
        self.context_bytes += owned_bytes;
        self.contexts.push_back(ContextEntry {
            generation,
            query,
            radius,
            result,
            owned_bytes,
        });
        while self.contexts.len() > CONTEXT_CACHE_ENTRIES
            || self.context_bytes > CONTEXT_CACHE_BYTES
        {
            if let Some(entry) = self.contexts.pop_front() {
                self.context_bytes = self.context_bytes.saturating_sub(entry.owned_bytes);
            }
        }
    }
}

fn default_limit() -> usize {
    200
}

fn query_owned_bytes(query: &Query) -> usize {
    query.pattern.capacity()
        + query.paths.iter().map(String::capacity).sum::<usize>()
        + query.glob.as_ref().map_or(0, String::capacity)
        + std::mem::size_of::<Query>()
}

impl Query {
    pub fn simple(search: &Search) -> Self {
        Self {
            pattern: search.pattern.clone(),
            regex: search.regex,
            ignore_case: search.ignore_case,
            paths: Vec::new(),
            glob: None,
            limit: search.limit,
            max_matches_per_file: None,
        }
    }
}

pub fn search_profiled(
    resident: &Resident,
    query: &Query,
    cache: &mut QueryCache,
) -> Result<ProfiledResult> {
    search_internal(resident, query, cache, true, true)
}

pub fn search_profiled_mode(
    resident: &Resident,
    query: &Query,
    cache: &mut QueryCache,
    allow_plan: bool,
) -> Result<ProfiledResult> {
    search_internal(resident, query, cache, allow_plan, true)
}

pub fn search_fast(
    resident: &Resident,
    query: &Query,
    cache: &mut QueryCache,
) -> Result<SearchResult> {
    if let Some(result) = cache.get_result(resident.stats.generation, query) {
        return Ok(result);
    }
    if !query.regex
        && !query.ignore_case
        && query.paths.is_empty()
        && query.glob.is_none()
        && query.max_matches_per_file.is_none()
    {
        let result = resident.search(&Search {
            pattern: query.pattern.clone(),
            regex: false,
            ignore_case: false,
            limit: query.limit,
        })?;
        cache.put_result(resident.stats.generation, query.clone(), result.clone());
        return Ok(result);
    }
    let result = search_internal(resident, query, cache, true, false)?.result;
    cache.put_result(resident.stats.generation, query.clone(), result.clone());
    Ok(result)
}

fn search_internal(
    resident: &Resident,
    query: &Query,
    cache: &mut QueryCache,
    allow_plan: bool,
    collect_profile: bool,
) -> Result<ProfiledResult> {
    validate(query)?;
    let mut profile = QueryProfile::default();
    let (compiled, cache_hit, compile_us) = cache.get_or_compile(query)?;
    if collect_profile {
        profile.cache_hit = cache_hit;
        profile.compile_us = compile_us;
    }

    let filter_started = collect_profile.then(Instant::now);
    let paths = paths(&query.paths)?;
    let glob = glob(query)?;
    let scoped = !paths.is_empty() || query.glob.is_some();
    if !collect_profile
        && scoped
        && allow_plan
        && let Some(plan) = compiled.plan.as_ref()
    {
        let mut evaluation = plan.evaluate(resident);
        let corpus_bytes = resident.stats.source_bytes;
        let raw_candidate_bytes = resident.candidate_bytes(&evaluation.ids);
        let index_cost = raw_candidate_bytes.saturating_add(
            (evaluation.posting_lookups + evaluation.set_operations)
                .saturating_mul(INDEX_OPERATION_BYTE_EQUIVALENT),
        );
        if index_cost < corpus_bytes
            && raw_candidate_bytes.saturating_mul(MAX_CANDIDATE_FRACTION_DENOMINATOR)
                <= corpus_bytes.saturating_mul(MAX_CANDIDATE_FRACTION_NUMERATOR)
        {
            evaluation
                .ids
                .retain(|id| path_allowed(resident, *id, &paths, &glob, query.glob.is_some()));
            evaluation.ids.sort_unstable_by(|a, b| {
                resident.documents[*a]
                    .path
                    .cmp(&resident.documents[*b].path)
            });
            let filtered_bytes = resident.candidate_bytes(&evaluation.ids);
            let line_prefilter = query.regex
                && compiled.manual_line_prefilter
                && plan.atom_count() <= 16
                && filtered_bytes.saturating_mul(2) < corpus_bytes;
            let workers = parallel_workers(filtered_bytes, evaluation.ids.len(), line_prefilter);
            let (matches, total) = if workers > 1 {
                scan_parallel(resident, query, compiled, &evaluation.ids, workers)?
            } else {
                let mut matches = Vec::new();
                let mut total = 0;
                scan(
                    resident,
                    query,
                    compiled,
                    &evaluation.ids,
                    line_prefilter.then_some(compiled.plan.as_ref()).flatten(),
                    &mut matches,
                    &mut total,
                )?;
                (matches, total)
            };
            let result = SearchResult {
                matches,
                total_matches: total,
                truncated: total > query.limit,
                consistency: format!(
                    "RAM generation {}; saved-file snapshot, watcher-synchronized before shared requests; not a filesystem transaction",
                    resident.stats.generation
                ),
            };
            check_search_size(&result)?;
            return Ok(ProfiledResult {
                result,
                profile: QueryProfile::default(),
            });
        }
    }
    let eligible = if scoped {
        let mut ids: Vec<_> = resident
            .paths
            .values()
            .copied()
            .filter(|id| path_allowed(resident, *id, &paths, &glob, query.glob.is_some()))
            .collect();
        ids.sort_unstable();
        ids
    } else {
        Vec::new()
    };
    let eligible_files = if scoped {
        eligible.len()
    } else {
        resident.stats.files
    };
    let eligible_bytes = if scoped {
        resident.candidate_bytes(&eligible)
    } else {
        resident.stats.source_bytes
    };
    if collect_profile {
        profile.eligible_files = eligible_files;
        profile.eligible_bytes = eligible_bytes;
        profile.filter_us = elapsed_us(filter_started);
    }

    let plan_started = collect_profile.then(Instant::now);
    let mut ids = if scoped {
        eligible.clone()
    } else {
        resident.paths.values().copied().collect()
    };
    let mut path_sorted = !scoped;
    let mut candidate_bytes = eligible_bytes;
    let mut line_prefilter = false;
    if let Some(plan) = if allow_plan {
        compiled.plan.as_ref()
    } else {
        None
    } {
        let evaluation = plan.evaluate(resident);
        let candidates = if scoped {
            intersect(&eligible, &evaluation.ids)
        } else {
            evaluation.ids
        };
        let planned_candidate_bytes = resident.candidate_bytes(&candidates);
        let index_cost = planned_candidate_bytes.saturating_add(
            (evaluation.posting_lookups + evaluation.set_operations)
                .saturating_mul(INDEX_OPERATION_BYTE_EQUIVALENT),
        );
        if collect_profile {
            profile.plan = truncate(&plan.describe(), 512);
            profile.posting_lookups = evaluation.posting_lookups;
            profile.set_operations = evaluation.set_operations;
        }
        let selective = planned_candidate_bytes.saturating_mul(MAX_CANDIDATE_FRACTION_DENOMINATOR)
            <= eligible_bytes.saturating_mul(MAX_CANDIDATE_FRACTION_NUMERATOR);
        if index_cost < eligible_bytes && selective {
            ids = candidates;
            candidate_bytes = planned_candidate_bytes;
            path_sorted = false;
            line_prefilter = query.regex
                && compiled.manual_line_prefilter
                && plan.atom_count() <= 16
                && planned_candidate_bytes.saturating_mul(2) < eligible_bytes;
            if collect_profile {
                profile.used_index = true;
            }
        } else if collect_profile {
            profile.fallback =
                Some("candidate plan cost/selectivity did not beat tuned full-scan gate".into());
        }
    } else if collect_profile {
        profile.plan = "all".into();
        profile.fallback = Some(
            if allow_plan {
                "no sound indexed literal condition"
            } else {
                "candidate planner disabled for baseline"
            }
            .into(),
        );
    }
    if !path_sorted {
        ids.sort_unstable_by(|a, b| {
            resident.documents[*a]
                .path
                .cmp(&resident.documents[*b].path)
        });
    }
    if collect_profile {
        profile.candidate_files = ids.len();
        profile.candidate_bytes = candidate_bytes;
        profile.line_prefilter = line_prefilter;
        profile.plan_us = elapsed_us(plan_started);
    }

    let scan_started = collect_profile.then(Instant::now);
    let workers = parallel_workers(candidate_bytes, ids.len(), line_prefilter);
    if collect_profile {
        profile.parallel_workers = workers;
    }
    let (matches, total) = if workers > 1 {
        scan_parallel(resident, query, compiled, &ids, workers)?
    } else {
        let mut matches = Vec::new();
        let mut total = 0;
        scan(
            resident,
            query,
            compiled,
            &ids,
            line_prefilter.then_some(compiled.plan.as_ref()).flatten(),
            &mut matches,
            &mut total,
        )?;
        (matches, total)
    };
    if collect_profile {
        profile.scan_us = elapsed_us(scan_started);
    }

    let finalize_started = collect_profile.then(Instant::now);
    let result = SearchResult {
        matches,
        total_matches: total,
        truncated: total > query.limit,
        consistency: format!(
            "RAM generation {}; saved-file snapshot, watcher-synchronized before shared requests; not a filesystem transaction",
            resident.stats.generation
        ),
    };
    check_search_size(&result)?;
    if collect_profile {
        profile.finalize_us = elapsed_us(finalize_started);
    }
    Ok(ProfiledResult { result, profile })
}

fn compile(key: &CacheKey) -> Result<Compiled> {
    if !key.regex && !key.ignore_case {
        return Ok(Compiled {
            matcher: None,
            per_line: false,
            manual_line_prefilter: false,
            plan: Some(CandidateExpr::Literal(key.pattern.as_bytes().to_vec())),
        });
    }
    let syntax = if key.regex {
        key.pattern.clone()
    } else {
        regex_syntax::escape(&key.pattern)
    };
    let hir = regex_syntax::ParserBuilder::new()
        .multi_line(true)
        .case_insensitive(key.ignore_case)
        .build()
        .parse(&syntax)?;
    let per_line = key.regex && text_anchored(&hir);
    let manual_line_prefilter = key.regex && expensive_leading_repetition(&hir);
    let matcher = RegexMatcherBuilder::new()
        .fixed_strings(!key.regex)
        .case_insensitive(key.ignore_case)
        .multi_line(true)
        .line_terminator(Some(b'\n'))
        .size_limit(MATCHER_MEMORY_LIMIT)
        .dfa_size_limit(MATCHER_MEMORY_LIMIT)
        .build(&key.pattern)?;
    Ok(Compiled {
        matcher: Some(matcher),
        per_line,
        manual_line_prefilter,
        plan: planner::build(&hir),
    })
}

fn scan(
    resident: &Resident,
    query: &Query,
    compiled: &Compiled,
    ids: &[usize],
    line_prefilter: Option<&CandidateExpr>,
    matches: &mut Vec<Hit>,
    total: &mut usize,
) -> Result<()> {
    let mut file_lines = Vec::new();
    if let Some(expression) = &compiled.matcher {
        for id in ids {
            let document = &resident.documents[*id];
            file_lines.clear();
            if compiled.per_line || line_prefilter.is_some() {
                scan_lines(document, expression, line_prefilter, &mut file_lines)?;
            } else {
                scan_document(document, expression, &mut file_lines)?;
            }
            *total += file_lines.len();
            record_document(
                document,
                &file_lines,
                query.limit,
                query.max_matches_per_file,
                matches,
            );
        }
    } else {
        let finder = Finder::new(query.pattern.as_bytes());
        for id in ids {
            let document = &resident.documents[*id];
            file_lines.clear();
            let mut previous = None;
            for position in finder.find_iter(document.text.as_bytes()) {
                let line = document
                    .starts
                    .partition_point(|start| (*start as usize) <= position)
                    - 1;
                if previous == Some(line) {
                    continue;
                }
                previous = Some(line);
                file_lines.push(line);
            }
            *total += file_lines.len();
            record_document(
                document,
                &file_lines,
                query.limit,
                query.max_matches_per_file,
                matches,
            );
        }
    }
    Ok(())
}

fn scan_parallel(
    resident: &Resident,
    query: &Query,
    compiled: &Compiled,
    ids: &[usize],
    workers: usize,
) -> Result<(Vec<Hit>, usize)> {
    let chunk_size = ids.len().div_ceil(workers);
    let parts = std::thread::scope(|scope| {
        let handles: Vec<_> = ids
            .chunks(chunk_size)
            .map(|chunk| {
                std::thread::Builder::new()
                    .name("cctx-regex".into())
                    .stack_size(WORKER_STACK_BYTES)
                    .spawn_scoped(scope, move || {
                        let mut matches = Vec::new();
                        let mut total = 0;
                        scan(
                            resident,
                            query,
                            compiled,
                            chunk,
                            None,
                            &mut matches,
                            &mut total,
                        )?;
                        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((matches, total))
                    })
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| invalid("parallel regex worker panicked").into())
                    .and_then(|result| result)
            })
            .collect::<Result<Vec<_>>>()
    })?;
    let total = parts.iter().map(|(_, total)| total).sum();
    let mut matches = Vec::with_capacity(query.limit.min(total));
    for (part, _) in parts {
        let remaining = query.limit.saturating_sub(matches.len());
        matches.extend(part.into_iter().take(remaining));
        if remaining == 0 {
            break;
        }
    }
    Ok((matches, total))
}

fn scan_lines(
    document: &crate::resident::Document,
    expression: &RegexMatcher,
    prefilter: Option<&CandidateExpr>,
    file_lines: &mut Vec<usize>,
) -> Result<()> {
    for line in 0..document.starts.len() {
        let start = document.starts[line] as usize;
        let end = document
            .starts
            .get(line + 1)
            .copied()
            .map(|offset| offset as usize)
            .unwrap_or(document.text.len());
        let bytes = &document.text.as_bytes()[start..end];
        if prefilter.is_some_and(|plan| !plan.matches(bytes)) {
            continue;
        }
        if expression
            .find(bytes)?
            .is_some_and(|found| found.start() < bytes.len() || !bytes.ends_with(b"\n"))
        {
            file_lines.push(line);
        }
    }
    Ok(())
}

fn scan_document(
    document: &crate::resident::Document,
    expression: &RegexMatcher,
    file_lines: &mut Vec<usize>,
) -> Result<()> {
    let mut previous = None;
    expression.find_iter(document.text.as_bytes(), |matched| {
        let offset = matched.start();
        if document.starts.is_empty()
            || (offset == document.text.len() && document.text.ends_with('\n'))
        {
            return true;
        }
        let line = document
            .starts
            .partition_point(|start| (*start as usize) <= offset)
            - 1;
        if previous != Some(line) {
            previous = Some(line);
            file_lines.push(line);
        }
        true
    })?;
    Ok(())
}

pub(crate) fn is_definition_line(line: &str) -> bool {
    let mut trimmed = line.trim_start();
    while let Some(rest) = [
        "pub(crate) ",
        "pub(super) ",
        "pub ",
        "export ",
        "default ",
        "protected ",
        "private ",
        "internal ",
        "public ",
        "static ",
        "async ",
        "unsafe ",
        "const ",
        "abstract ",
        "sealed ",
        "partial ",
        "virtual ",
        "override ",
    ]
    .iter()
    .find_map(|prefix| trimmed.strip_prefix(prefix))
    {
        trimmed = rest.trim_start();
    }
    [
        "fn ",
        "struct ",
        "enum ",
        "trait ",
        "type ",
        "impl ",
        "impl<",
        "mod ",
        "const ",
        "static ",
        "macro_rules!",
        "def ",
        "class ",
        "func ",
        "function ",
        "interface ",
        "namespace ",
    ]
    .iter()
    .any(|keyword| trimmed.starts_with(keyword))
}

fn record_document(
    document: &crate::resident::Document,
    file_lines: &[usize],
    limit: usize,
    max_per_file: Option<usize>,
    matches: &mut Vec<Hit>,
) {
    let remaining = limit.saturating_sub(matches.len());
    if remaining == 0 || file_lines.is_empty() {
        return;
    }
    let cap = max_per_file.unwrap_or(limit).min(remaining);
    if file_lines.len() <= cap {
        for &line in file_lines {
            matches.push(Hit {
                path: document.path.to_string(),
                line: line + 1,
                text: document.line(line).into(),
            });
        }
        return;
    }
    if max_per_file.is_some() {
        let mut selected: Vec<usize> = file_lines
            .iter()
            .copied()
            .filter(|&line| is_definition_line(document.line(line)))
            .take(cap)
            .collect();
        if selected.len() < cap {
            for &line in file_lines {
                if !selected.contains(&line) {
                    selected.push(line);
                    if selected.len() == cap {
                        break;
                    }
                }
            }
            selected.sort_unstable();
        }
        for line in selected {
            matches.push(Hit {
                path: document.path.to_string(),
                line: line + 1,
                text: document.line(line).into(),
            });
        }
    } else {
        for &line in file_lines.iter().take(cap) {
            matches.push(Hit {
                path: document.path.to_string(),
                line: line + 1,
                text: document.line(line).into(),
            });
        }
    }
}

fn validate(query: &Query) -> Result<()> {
    if query.pattern.is_empty()
        || query.pattern.len() > 16 * 1024
        || query.pattern.contains(['\n', '\0'])
        || query.limit == 0
        || query.limit > 1000
        || query
            .max_matches_per_file
            .is_some_and(|cap| cap == 0 || cap > 1000)
    {
        return Err(invalid(
            "nonempty single-line pattern <=16KiB, limit 1..1000, up to 64 paths/64KiB required",
        )
        .into());
    }
    Ok(())
}

pub(crate) fn paths(prefixes: &[String]) -> Result<Vec<std::path::PathBuf>> {
    if prefixes.len() > 64
        || prefixes.iter().any(|path| path.len() > 4096)
        || prefixes.iter().map(String::len).sum::<usize>() > 64 * 1024
    {
        return Err(
            invalid("up to 64 relative paths, 4096 bytes each and 64KiB total required").into(),
        );
    }
    prefixes
        .iter()
        .map(|path| {
            safe_relative(Path::new(path))?;
            Ok(normalize(Path::new(path)))
        })
        .collect()
}

fn glob(query: &Query) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    if let Some(glob) = &query.glob {
        if glob.len() > 1024 {
            return Err(invalid("glob exceeds 1024 bytes").into());
        }
        builder.add(Glob::new(glob)?);
    }
    Ok(builder.build()?)
}

fn path_allowed(
    resident: &Resident,
    id: usize,
    paths: &[std::path::PathBuf],
    glob: &GlobSet,
    has_glob: bool,
) -> bool {
    let path = Path::new(resident.documents[id].path.as_ref());
    path_matches(path, paths)
        && (!has_glob
            || glob.is_match(path)
            || path.file_name().is_some_and(|name| glob.is_match(name)))
}

pub(crate) fn path_matches(path: &Path, paths: &[std::path::PathBuf]) -> bool {
    !path.as_os_str().is_empty()
        && (paths.is_empty() || paths.iter().any(|prefix| path.starts_with(prefix)))
}

fn text_anchored(hir: &Hir) -> bool {
    match hir.kind() {
        HirKind::Look(Look::Start | Look::End) => true,
        HirKind::Capture(capture) => text_anchored(&capture.sub),
        HirKind::Repetition(repeat) => text_anchored(&repeat.sub),
        HirKind::Concat(items) | HirKind::Alternation(items) => items.iter().any(text_anchored),
        _ => false,
    }
}

fn expensive_leading_repetition(hir: &Hir) -> bool {
    match hir.kind() {
        HirKind::Capture(capture) => expensive_leading_repetition(&capture.sub),
        HirKind::Alternation(items) => items.iter().all(expensive_leading_repetition),
        HirKind::Concat(items) => items
            .iter()
            .find(|item| !matches!(item.kind(), HirKind::Look(_) | HirKind::Empty))
            .is_some_and(|item| match item.kind() {
                HirKind::Repetition(repeat) if repeat.min > 0 && repeat.max.is_none() => {
                    matches!(repeat.sub.kind(), HirKind::Class(_))
                }
                _ => false,
            }),
        _ => false,
    }
}

fn intersect(left: &[usize], right: &[usize]) -> Vec<usize> {
    let (mut a, mut b) = (0, 0);
    let mut output = Vec::with_capacity(left.len().min(right.len()));
    while a < left.len() && b < right.len() {
        match left[a].cmp(&right[b]) {
            std::cmp::Ordering::Less => a += 1,
            std::cmp::Ordering::Greater => b += 1,
            std::cmp::Ordering::Equal => {
                output.push(left[a]);
                a += 1;
                b += 1;
            }
        }
    }
    output
}

fn micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn elapsed_us(started: Option<Instant>) -> u64 {
    started.map(micros).unwrap_or(0)
}

fn parallel_workers(candidate_bytes: usize, files: usize, line_prefilter: bool) -> usize {
    if line_prefilter || candidate_bytes < 8 * 1024 * 1024 || files < 128 {
        return 1;
    }
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(8)
        .min(files)
}

fn truncate(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        value.into()
    } else {
        let mut end = limit;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &value[..end])
    }
}
