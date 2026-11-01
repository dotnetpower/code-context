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
const ELIGIBLE_CACHE_ENTRIES: usize = 16;
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
    finder: Option<memchr::memmem::Finder<'static>>,
    per_line: bool,
    manual_line_prefilter: bool,
    plan: Option<CandidateExpr>,
}

struct CacheEntry {
    key: CacheKey,
    compiled: std::sync::Arc<Compiled>,
}

struct ResultEntry {
    generation: u64,
    query: Query,
    result: std::sync::Arc<SearchResult>,
    owned_bytes: usize,
}

struct ContextEntry {
    generation: u64,
    query: Query,
    radius: usize,
    result: std::sync::Arc<ContextResult>,
    owned_bytes: usize,
}

struct EligibleEntry {
    generation: u64,
    paths: Vec<String>,
    glob: Option<String>,
    ids: Vec<usize>,
    bytes: usize,
}

#[derive(Default)]
pub struct QueryCache {
    entries: VecDeque<CacheEntry>,
    pattern_bytes: usize,
    results: VecDeque<ResultEntry>,
    result_bytes: usize,
    contexts: VecDeque<ContextEntry>,
    context_bytes: usize,
    eligible_cache: VecDeque<EligibleEntry>,
}

impl QueryCache {
    fn get_or_compile(&mut self, query: &Query) -> Result<(std::sync::Arc<Compiled>, bool, u64)> {
        if let Some(position) = self.entries.iter().position(|entry| {
            entry.key.regex == query.regex
                && entry.key.ignore_case == query.ignore_case
                && entry.key.pattern == query.pattern
        }) {
            if position + 1 != self.entries.len() {
                let entry = self
                    .entries
                    .remove(position)
                    .expect("cache position exists");
                self.entries.push_back(entry);
            }
            return Ok((
                self.entries.back().expect("cache entry").compiled.clone(),
                true,
                0,
            ));
        }
        let key = CacheKey {
            pattern: query.pattern.clone(),
            regex: query.regex,
            ignore_case: query.ignore_case,
        };
        let started = Instant::now();
        let compiled = std::sync::Arc::new(compile(&key)?);
        let compile_us = micros(started);
        self.pattern_bytes += key.pattern.len();
        self.entries.push_back(CacheEntry {
            key,
            compiled: compiled.clone(),
        });
        while self.entries.len() > CACHE_ENTRIES || self.pattern_bytes > CACHE_PATTERN_BYTES {
            if let Some(entry) = self.entries.pop_front() {
                self.pattern_bytes = self.pattern_bytes.saturating_sub(entry.key.pattern.len());
            }
        }
        Ok((compiled, false, compile_us))
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
        self.eligible_cache
            .retain(|entry| entry.generation == generation);
    }

    pub fn clear_memory(&mut self) {
        self.entries.clear();
        self.pattern_bytes = 0;
        self.results.clear();
        self.result_bytes = 0;
        self.contexts.clear();
        self.context_bytes = 0;
        self.eligible_cache.clear();
    }

    fn get_result(&mut self, generation: u64, query: &Query) -> Option<std::sync::Arc<SearchResult>> {
        let position = self
            .results
            .iter()
            .position(|entry| entry.generation == generation && &entry.query == query)?;
        if position + 1 == self.results.len() || self.results.len() < RESULT_CACHE_ENTRIES / 2 {
            return Some(self.results[position].result.clone());
        }
        let entry = self.results.remove(position)?;
        let result = entry.result.clone();
        self.results.push_back(entry);
        Some(result)
    }

    fn put_result(&mut self, generation: u64, query: Query, result: std::sync::Arc<SearchResult>) {
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
    ) -> Option<std::sync::Arc<ContextResult>> {
        let position = self.contexts.iter().position(|entry| {
            entry.generation == generation && &entry.query == query && entry.radius == radius
        })?;
        if position + 1 == self.contexts.len() || self.contexts.len() < CONTEXT_CACHE_ENTRIES / 2 {
            return Some(self.contexts[position].result.clone());
        }
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
        result: std::sync::Arc<ContextResult>,
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

pub(crate) fn search_fast_arc(
    resident: &Resident,
    query: &Query,
    cache: &mut QueryCache,
) -> Result<std::sync::Arc<SearchResult>> {
    if let Some(result) = cache.get_result(resident.stats.generation, query) {
        return Ok(result);
    }
    if !query.regex
        && !query.ignore_case
        && query.paths.is_empty()
        && query.glob.is_none()
        && query.max_matches_per_file.is_none()
    {
        let result = std::sync::Arc::new(resident.search(&Search {
            pattern: query.pattern.clone(),
            regex: false,
            ignore_case: false,
            limit: query.limit,
        })?);
        cache.put_result(resident.stats.generation, query.clone(), result.clone());
        return Ok(result);
    }
    let result = std::sync::Arc::new(search_internal(resident, query, cache, true, false)?.result);
    cache.put_result(resident.stats.generation, query.clone(), result.clone());
    Ok(result)
}

pub fn search_fast(
    resident: &Resident,
    query: &Query,
    cache: &mut QueryCache,
) -> Result<SearchResult> {
    let arc = search_fast_arc(resident, query, cache)?;
    Ok(std::sync::Arc::try_unwrap(arc).unwrap_or_else(|shared| (*shared).clone()))
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
    let (compiled_arc, cache_hit, compile_us) = cache.get_or_compile(query)?;
    let compiled = compiled_arc.as_ref();
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
                .retain(|id| path_allowed(resident, *id, &paths, &glob));
            if !resident.ids_are_path_sorted {
                evaluation.ids.sort_unstable_by(|a, b| {
                    resident.documents[*a]
                        .path
                        .cmp(&resident.documents[*b].path)
                });
            }
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
    let (eligible, eligible_bytes) = if scoped {
        if let Some(pos) = cache.eligible_cache.iter().position(|entry| {
            entry.generation == resident.stats.generation
                && entry.paths == query.paths
                && entry.glob == query.glob
        }) {
            let entry = cache
                .eligible_cache
                .remove(pos)
                .expect("eligible cache hit");
            let out = (entry.ids.clone(), entry.bytes);
            cache.eligible_cache.push_back(entry);
            out
        } else {
            let mut ids: Vec<_> = resident
                .paths
                .values()
                .copied()
                .filter(|id| path_allowed(resident, *id, &paths, &glob))
                .collect();
            ids.sort_unstable();
            let bytes = resident.candidate_bytes(&ids);
            if query.paths.len() <= 8 {
                if cache.eligible_cache.len() >= ELIGIBLE_CACHE_ENTRIES {
                    cache.eligible_cache.pop_front();
                }
                cache.eligible_cache.push_back(EligibleEntry {
                    generation: resident.stats.generation,
                    paths: query.paths.clone(),
                    glob: query.glob.clone(),
                    ids: ids.clone(),
                    bytes,
                });
            }
            (ids, bytes)
        }
    } else {
        (Vec::new(), resident.stats.source_bytes)
    };
    let eligible_files = if scoped {
        eligible.len()
    } else {
        resident.stats.files
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
    if let Some(plan) = if allow_plan && (!scoped || eligible.len() > 2 || collect_profile) {
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
            path_sorted = resident.ids_are_path_sorted;
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
            finder: Some(memchr::memmem::Finder::new(key.pattern.as_bytes()).into_owned()),
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
        finder: None,
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
        let fallback_finder;
        let finder = match &compiled.finder {
            Some(f) => f.as_ref(),
            None => {
                fallback_finder = Finder::new(query.pattern.as_bytes());
                fallback_finder
            }
        };
        for id in ids {
            let document = &resident.documents[*id];
            if document.starts.is_empty() {
                continue;
            }
            file_lines.clear();
            let bytes = document.text.as_bytes();
            let mut search_pos = 0usize;
            if matches.len() >= query.limit {
                while search_pos < bytes.len() {
                    let Some(rel) = finder.find(&bytes[search_pos..]) else {
                        break;
                    };
                    let position = search_pos + rel;
                    *total += 1;
                    if let Some(nl) = memchr::memchr(b'\n', &bytes[position..]) {
                        search_pos = position + nl + 1;
                    } else {
                        break;
                    }
                }
                continue;
            }
            let mut line_cursor = 0usize;
            while search_pos < bytes.len() {
                let Some(rel) = finder.find(&bytes[search_pos..]) else {
                    break;
                };
                let position = search_pos + rel;
                let line = crate::resident::find_line_gallop(&document.starts, line_cursor, position);
                file_lines.push(line);
                line_cursor = line + 1;
                search_pos = document
                    .starts
                    .get(line_cursor)
                    .copied()
                    .map(|start| start as usize)
                    .unwrap_or(bytes.len());
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

fn partition_ids_by_bytes<'a>(
    resident: &Resident,
    ids: &'a [usize],
    workers: usize,
) -> Vec<&'a [usize]> {
    if workers <= 1 || ids.len() <= 1 {
        return vec![ids];
    }
    let total_bytes: usize = ids
        .iter()
        .map(|&id| resident.documents[id].text.len().max(1))
        .sum();
    let target_per_worker = total_bytes.div_ceil(workers).max(1);
    let mut slices = Vec::with_capacity(workers);
    let mut start = 0usize;
    let mut acc = 0usize;
    for (idx, &id) in ids.iter().enumerate() {
        acc += resident.documents[id].text.len().max(1);
        let remaining_items = ids.len() - (idx + 1);
        let remaining_workers = workers.saturating_sub(slices.len() + 1);
        if (acc >= target_per_worker && slices.len() + 1 < workers)
            || (remaining_workers > 0 && remaining_items == remaining_workers)
        {
            slices.push(&ids[start..=idx]);
            start = idx + 1;
            acc = 0;
        }
    }
    if start < ids.len() {
        slices.push(&ids[start..]);
    }
    slices
}

fn scan_parallel(
    resident: &Resident,
    query: &Query,
    compiled: &Compiled,
    ids: &[usize],
    workers: usize,
) -> Result<(Vec<Hit>, usize)> {
    let chunks = partition_ids_by_bytes(resident, ids, workers);
    let parts = std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
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
    if prefilter.is_some_and(|plan| !plan.matches(document.text.as_bytes())) {
        return Ok(());
    }
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
    if document.starts.is_empty() {
        return Ok(());
    }
    let mut line_cursor = 0usize;
    let mut next_line_start = 0usize;
    expression.find_iter(document.text.as_bytes(), |matched| {
        let offset = matched.start();
        if offset == document.text.len() && document.text.ends_with('\n') {
            return true;
        }
        if offset < next_line_start {
            return true;
        }
        let line = crate::resident::find_line_gallop(&document.starts, line_cursor, offset);
        file_lines.push(line);
        line_cursor = line + 1;
        next_line_start = document
            .starts
            .get(line_cursor)
            .copied()
            .map(|start| start as usize)
            .unwrap_or(usize::MAX);
        true
    })?;
    Ok(())
}

pub(crate) fn trim_line_start(line: &str) -> &str {
    line.strip_prefix('\u{feff}').unwrap_or(line).trim_start()
}

fn strip_modifier(trimmed: &str) -> Option<&str> {
    let bytes = trimmed.as_bytes();
    match bytes.first()? {
        b'p' => {
            if let Some(rest) = trimmed.strip_prefix("pub(")
                && let Some((_, after_paren)) = rest.split_once(')')
            {
                let after_paren = after_paren.trim_start();
                if !after_paren.is_empty() {
                    return Some(after_paren);
                }
            }
            [
                "pub ",
                "protected ",
                "private ",
                "public ",
                "partial ",
            ]
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix))
        }
        b'e' => {
            if let Some(rest) = trimmed.strip_prefix("export ") {
                return Some(rest);
            }
            if let Some(rest) = trimmed.strip_prefix("external ") {
                return Some(rest);
            }
            if let Some(rest) = trimmed.strip_prefix("extern ") {
                let rest = rest.trim_start();
                if let Some(after_quote) = rest.strip_prefix('"')
                    && let Some((_, after_abi)) = after_quote.split_once('"')
                {
                    let after_abi = after_abi.trim_start();
                    if !after_abi.is_empty() {
                        return Some(after_abi);
                    }
                } else if rest.starts_with("fn ")
                    || rest.starts_with("type ")
                    || rest.starts_with("static ")
                {
                    return Some(rest);
                }
            }
            None
        }
        b'd' => ["default ", "declare ", "data "]
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix)),
        b'i' => ["internal ", "inline ", "infix "]
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix)),
        b's' => {
            if let Some(rest) = trimmed
                .strip_prefix("sealed ")
                .or_else(|| trimmed.strip_prefix("suspend "))
            {
                return Some(rest);
            }
            if let Some(rest) = trimmed.strip_prefix("static ") {
                let next = rest.trim_start();
                if next.starts_with("fn ")
                    || next.starts_with("async ")
                    || next.starts_with("class ")
                    || next.starts_with("function")
                    || next.starts_with("readonly ")
                    || next.starts_with("fun ")
                    || next.starts_with("func ")
                {
                    return Some(rest);
                }
            }
            None
        }
        b'a' => ["async ", "abstract "]
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix)),
        b'u' => trimmed.strip_prefix("unsafe "),
        b'c' => {
            if let Some(rest) = trimmed.strip_prefix("companion ") {
                return Some(rest);
            }
            if let Some(rest) = trimmed.strip_prefix("convenience ") {
                return Some(rest);
            }
            if let Some(rest) = trimmed.strip_prefix("const ") {
                let next = rest.trim_start();
                if next.starts_with("fn ")
                    || next.starts_with("unsafe ")
                    || next.starts_with("extern ")
                    || next.starts_with("enum ")
                {
                    return Some(rest);
                }
            }
            None
        }
        b'v' => ["virtual ", "value "]
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix)),
        b'o' => ["override ", "open ", "operator "]
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix)),
        b'r' => ["readonly ", "reified ", "required "]
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix)),
        b'f' => ["final ", "fileprivate "]
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix)),
        b'l' => ["local ", "lateinit "]
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix)),
        b'm' => trimmed.strip_prefix("mutating "),
        b'n' => trimmed.strip_prefix("nonmutating "),
        b't' => trimmed.strip_prefix("tailrec "),
        _ => None,
    }
}

pub(crate) fn is_reexport_from_line(trimmed: &str) -> bool {
    let trimmed = trim_line_start(trimmed);
    let Some(rest) = trimmed.strip_prefix("export ") else {
        return false;
    };
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("type ").map(str::trim_start).unwrap_or(rest);
    if !(rest.starts_with('*') || rest.starts_with('{')) {
        return false;
    }
    rest.contains(" from '")
        || rest.contains(" from \"")
        || rest.contains("}from '")
        || rest.contains("}from \"")
}

fn is_cjs_require_import(trimmed: &str) -> bool {
    if trimmed.starts_with("require(") {
        return true;
    }
    if !(trimmed.starts_with("const ") || trimmed.starts_with("let ") || trimmed.starts_with("var "))
    {
        return false;
    }
    if let Some((_, rhs)) = trimmed.split_once('=') {
        let rhs = rhs.trim_start();
        rhs.starts_with("require(")
    } else {
        false
    }
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

fn is_react_component_or_hook_binding(after_keyword: &str) -> bool {
    let ident: &str = after_keyword
        .trim_start()
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .next()
        .unwrap_or("");
    if ident.is_empty() {
        return false;
    }
    let bytes = ident.as_bytes();
    let is_pascal = bytes[0].is_ascii_uppercase()
        && bytes.iter().any(|b| b.is_ascii_lowercase());
    let is_hook = ident.starts_with("use")
        && bytes.get(3).is_some_and(u8::is_ascii_uppercase);
    is_pascal || is_hook
}

fn is_vite_plugin_hook_line(trimmed: &str) -> bool {
    [
        "configResolved(",
        "configureServer(",
        "configurePreviewServer(",
        "transformIndexHtml(",
        "handleHotUpdate(",
        "resolveId(",
        "transform(",
        "load(",
        "buildStart(",
        "buildEnd(",
        "generateBundle(",
        "writeBundle(",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
}

fn is_sql_ddl_definition_line(trimmed: &str) -> bool {
    let upper = trimmed.get(..36).unwrap_or(trimmed).to_ascii_uppercase();
    [
        "CREATE TABLE ",
        "CREATE TEMP TABLE ",
        "CREATE TEMPORARY TABLE ",
        "CREATE VIEW ",
        "CREATE OR REPLACE VIEW ",
        "CREATE MATERIALIZED VIEW ",
        "CREATE INDEX ",
        "CREATE UNIQUE INDEX ",
        "CREATE FUNCTION ",
        "CREATE OR REPLACE FUNCTION ",
        "CREATE PROCEDURE ",
        "CREATE OR REPLACE PROCEDURE ",
        "CREATE TRIGGER ",
        "CREATE TYPE ",
        "CREATE DOMAIN ",
        "CREATE SCHEMA ",
        "ALTER TABLE ",
    ]
    .iter()
    .any(|prefix| upper.starts_with(prefix))
}

pub(crate) fn is_definition_line(line: &str) -> bool {
    definition_priority(line) > 0
}

pub(crate) fn definition_priority(line: &str) -> u8 {
    let raw = trim_line_start(line);
    if raw.is_empty() || is_reexport_from_line(raw) || is_cjs_require_import(raw) {
        return 0;
    }
    let indent = leading_indent_cols(line);
    let mut trimmed = raw;
    let mut had_export_or_declare = false;
    while let Some(rest) = strip_modifier(trimmed) {
        if trimmed.starts_with("export ")
            || trimmed.starts_with("default ")
            || trimmed.starts_with("declare ")
            || trimmed.starts_with("pub ")
            || trimmed.starts_with("pub(")
            || trimmed.starts_with("public ")
        {
            had_export_or_declare = true;
        }
        trimmed = rest.trim_start();
    }
    let Some(&first) = trimmed.as_bytes().first() else {
        return 0;
    };
    if matches!(first, b'c' | b'l' | b'v') {
        let is_const = trimmed.starts_with("const ") || trimmed.starts_with("val ");
        if let Some(after_kw) = trimmed
            .strip_prefix("const ")
            .or_else(|| trimmed.strip_prefix("let "))
            .or_else(|| trimmed.strip_prefix("var "))
            .or_else(|| trimmed.strip_prefix("val "))
        {
            let after_trimmed = after_kw.trim_start();
            if after_trimmed.starts_with("_ ") || after_trimmed.starts_with("_=") {
                return 0;
            }
            let is_component_or_fn = is_react_component_or_hook_binding(after_trimmed)
                || after_trimmed.contains("=>")
                || after_trimmed.contains("memo(")
                || after_trimmed.contains("forwardRef(")
                || after_trimmed.contains("createContext(")
                || after_trimmed.contains("lazy(")
                || after_trimmed.contains("defineConfig(")
                || after_trimmed.contains("createSlice(")
                || after_trimmed.contains("createStore(")
                || after_trimmed.contains("defineStore(")
                || after_trimmed.contains("createServerFn(")
                || after_trimmed.contains("createFileRoute(")
                || after_trimmed.contains("createRootRoute(")
                || after_trimmed.contains("createRoute(")
                || after_trimmed.contains("createLazyFileRoute(")
                || after_trimmed.contains("defineProps")
                || after_trimmed.contains("defineEmits")
                || after_trimmed.contains("defineModel")
                || after_trimmed.contains("$state(")
                || after_trimmed.contains("$state.raw(")
                || after_trimmed.contains("$derived(")
                || after_trimmed.contains("$derived.by(")
                || after_trimmed.contains("$props(")
                || after_trimmed.contains("createSignal(")
                || after_trimmed.contains("createResource(")
                || after_trimmed.contains("createMemo(")
                || after_trimmed.contains("useActionState(")
                || after_trimmed.contains("useOptimistic(")
                || after_trimmed.contains("queryOptions(")
                || after_trimmed.contains("infiniteQueryOptions(")
                || after_trimmed.contains("atom(")
                || after_trimmed.contains("pgTable(")
                || after_trimmed.contains("mysqlTable(")
                || after_trimmed.contains("sqliteTable(")
                || after_trimmed.contains("z.object(")
                || after_trimmed.contains("z.enum(")
                || after_trimmed.contains("v.object(");
            if had_export_or_declare {
                return if indent == 0 { 4 } else { 3 };
            }
            if indent <= 2 && is_component_or_fn {
                return 3;
            }
            if is_const && indent == 0 {
                return 2;
            }
            return 0;
        }
    }
    if indent <= 4 && is_vite_plugin_hook_line(trimmed) {
        return 2;
    }
    if indent <= 2 && is_sql_ddl_definition_line(trimmed) {
        return 3;
    }
    if indent == 0 && (trimmed.starts_with("func (") || trimmed.starts_with("func(")) {
        return 4;
    }
    let keywords: &[&str] = match first {
        b'f' => &[
            "fn ",
            "fun ",
            "func ",
            "func(",
            "function ",
            "function*",
            "function(",
            "forwardRef(",
            "forwardRef<",
            "fragment ",
        ],
        b's' => &[
            "struct ",
            "static ",
            "scalar ",
            "subscription ",
            "service ",
            "schema ",
            "schema{",
            "scope :",
            "sub ",
            "subscript(",
        ],
        b'e' => &[
            "enum ",
            "extension ",
            "extend type ",
            "extend interface ",
            "extend input ",
            "event ",
        ],
        b't' => &[
            "trait ",
            "type ",
            "table ",
            "terraform ",
            "terraform{",
            "targetScope ",
        ],
        b'i' => &[
            "impl ",
            "impl<",
            "interface ",
            "input ",
            "init(",
            "init?(",
        ],
        b'm' => &[
            "mod ",
            "macro_rules!",
            "macro ",
            "module ",
            "mixin ",
            "memo(",
            "memo<",
            "model ",
            "mutation ",
            "message ",
        ],
        b'c' => &[
            "class ",
            "createContext(",
            "createContext<",
            "createSlice(",
            "createStore(",
            "createServerFn(",
            "createServerFn<",
            "createFileRoute(",
            "createRootRoute(",
            "createRoute(",
            "createLazyFileRoute(",
            "createSignal(",
            "createEffect(",
            "createResource(",
            "createMemo(",
            "cmake_minimum_required(",
        ],
        b'd' => &[
            "def ",
            "defp ",
            "defmodule ",
            "defmacro ",
            "defmacrop ",
            "defguard ",
            "defguardp ",
            "defprotocol ",
            "defimpl ",
            "defstruct",
            "defexception ",
            "defdelegate ",
            "defineConfig(",
            "defineVitestConfig(",
            "defineNuxtConfig(",
            "definePageMeta(",
            "defineEventHandler(",
            "defineNitroPlugin(",
            "defineAppConfig(",
            "defineStore(",
            "defineProps",
            "defineEmits",
            "defineModel",
            "defineExpose",
            "defineOptions",
            "defineSlots",
            "datasource ",
            "directive @",
            "data \"",
        ],
        b'n' => &["namespace "],
        b'l' => &[
            "lazy(",
            "lazy<",
            "loader(",
            "clientLoader(",
            "locals ",
            "locals{",
        ],
        b'g' => &[
            "global ",
            "global{",
            "generator ",
            "generateMetadata(",
            "generateStaticParams(",
            "generateViewport(",
        ],
        b'q' => &["query ", "queryOptions(", "infiniteQueryOptions("],
        b'a' => &[
            "actor ",
            "associatedtype ",
            "action(",
            "clientAction(",
            "attr_reader ",
            "attr_writer ",
            "attr_accessor ",
            "add_executable(",
            "add_library(",
        ],
        b'p' => &[
            "package ",
            "protocol ",
            "proc ",
            "property ",
            "param ",
            "provider \"",
            "project(",
        ],
        b'o' => &[
            "object ",
            "oneof ",
            "output \"",
            "output ",
        ],
        b'r' => &[
            "resource \"",
            "resource ",
            "rpc ",
            "root_type ",
        ],
        b'u' => &["union ", "useActionState(", "useOptimistic(", "useFormStatus("],
        b'v' => &["variable \""],
        b'F' => &["FROM "],
        b'E' => &["ENTRYPOINT "],
        b'C' => &["CMD "],
        b'R' => &[
            "React.memo(",
            "React.memo<",
            "React.forwardRef(",
            "React.forwardRef<",
            "React.lazy(",
            "React.lazy<",
            "React.createContext(",
            "React.createContext<",
        ],
        b'@' => &[
            "@keyframes ",
            "@layer ",
            "@mixin ",
            "@function ",
            "@utility ",
            "@theme ",
            "@theme{",
            "@custom-media ",
            "@Component(",
            "@Injectable(",
            "@Directive(",
            "@Pipe(",
            "@NgModule(",
            "@Controller(",
            "@Module(",
        ],
        b':' => &[":root ", ":root{"],
        b'$' => &["$effect(", "$effect.pre("],
        b'-' => {
            if indent <= 4
                && trimmed.starts_with("--")
                && let Some((prop, val)) = trimmed[2..].split_once(':')
                && !prop.is_empty()
                && !val.trim().is_empty()
                && prop
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return 3;
            }
            &[]
        }
        _ => &[],
    };
    if keywords.iter().any(|keyword| trimmed.starts_with(keyword)) {
        return if had_export_or_declare || indent <= 2 { 3 } else { 2 };
    }
    if had_export_or_declare
        && (trimmed.starts_with("create(")
            || trimmed.starts_with("create<")
            || trimmed.starts_with("styled(")
            || trimmed.starts_with("styled.")
            || trimmed.starts_with("cva("))
    {
        return 3;
    }
    0
}

pub(crate) fn is_comment_only_line(line: &str, is_doc_file: bool) -> bool {
    let trimmed = trim_line_start(line);
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with("*/")
        || trimmed == "*"
        || trimmed.starts_with("* ")
        || trimmed.starts_with("-- ")
    {
        return true;
    }
    if !is_doc_file
        && trimmed.starts_with('#')
        && !trimmed.starts_with("#[")
        && !trimmed.starts_with("#![")
        && !trimmed.starts_with("#include")
        && !trimmed.starts_with("#define")
        && !trimmed.starts_with("#ifdef")
        && !trimmed.starts_with("#ifndef")
        && !trimmed.starts_with("#endif")
        && !trimmed.starts_with("#pragma")
        && !trimmed.starts_with("#region")
        && !trimmed.starts_with("#endregion")
        && !trimmed.starts_with("#error")
        && !trimmed.starts_with("#warning")
        && !trimmed.starts_with("#!")
    {
        return true;
    }
    false
}

fn is_low_signal_usage_line(line: &str) -> bool {
    let trimmed = trim_line_start(line).trim_end();
    (trimmed.starts_with("</") && trimmed.ends_with('>'))
        || trimmed.starts_with("console.log(")
        || trimmed.starts_with("console.debug(")
        || trimmed.starts_with("console.info(")
        || trimmed.starts_with("console.warn(")
        || trimmed.starts_with("console.error(")
        || trimmed.starts_with("console.trace(")
        || trimmed.starts_with("logger.debug(")
        || trimmed.starts_with("logger.trace(")
        || trimmed.starts_with("tracing::debug!(")
        || trimmed.starts_with("tracing::trace!(")
        || trimmed.starts_with("log::debug!(")
        || trimmed.starts_with("log::trace!(")
        || trimmed.starts_with("println!(")
        || trimmed.starts_with("eprintln!(")
        || trimmed.starts_with("dbg!(")
        || trimmed == "debugger;"
        || trimmed == "debugger"
        || trimmed.starts_with("todo!(")
        || trimmed.starts_with("unimplemented!(")
}

pub(crate) fn is_import_line(line: &str) -> bool {
    let trimmed = trim_line_start(line);
    trimmed.starts_with("use ")
        || trimmed.starts_with("import ")
        || trimmed.starts_with("import(")
        || trimmed.starts_with("import (")
        || trimmed.starts_with("import\"")
        || trimmed.starts_with("import \"")
        || trimmed.starts_with("import'")
        || trimmed.starts_with("import '")
        || trimmed.starts_with("import{")
        || trimmed.starts_with("import*")
        || trimmed.starts_with("from ")
        || trimmed.starts_with("#include")
        || trimmed.starts_with("using ")
        || trimmed.starts_with("extern crate ")
        || trimmed.starts_with("require ")
        || trimmed.starts_with("require_relative ")
        || trimmed.starts_with("require_once ")
        || trimmed.starts_with("require_once(")
        || trimmed.starts_with("include_once ")
        || trimmed.starts_with("include_once(")
        || trimmed.starts_with("alias ")
        || trimmed.starts_with("export '")
        || trimmed.starts_with("part '")
        || trimmed.starts_with("part of ")
        || trimmed.starts_with("inherit ")
        || trimmed.starts_with("} from ")
        || trimmed.starts_with("}from ")
        || trimmed.starts_with("@import ")
        || trimmed.starts_with("@import\"")
        || trimmed.starts_with("@import'")
        || trimmed.starts_with("@use ")
        || trimmed.starts_with("@forward ")
        || trimmed.starts_with("/// <reference ")
        || trimmed.starts_with("///<reference ")
        || trimmed.starts_with("/// <amd-")
        || is_reexport_from_line(trimmed)
        || is_cjs_require_import(trimmed)
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
        let is_doc_file = crate::document::is_document_path(&document.path);
        let mut def_candidates: Vec<(u8, usize)> = file_lines
            .iter()
            .copied()
            .filter_map(|line| {
                let text = document.line(line);
                let mut prio = if is_doc_file {
                    crate::document::document_definition_priority(text)
                        .max(definition_priority(text))
                } else {
                    definition_priority(text)
                };
                if prio == 0 && !is_doc_file && line > 0 {
                    let prev_trim = trim_line_start(document.line(line - 1));
                    if (prev_trim.starts_with("#[test")
                        || prev_trim.starts_with("#[tokio::test")
                        || prev_trim.starts_with("#[rstest")
                        || prev_trim.starts_with("#[proc_macro"))
                        && trim_line_start(text).contains("fn ")
                    {
                        prio = 3;
                    }
                }
                if prio > 0 && leading_indent_cols(text) == 0 && !document.path.starts_with("tests/") {
                    prio = prio.saturating_add(1);
                }
                (prio > 0).then_some((prio, line))
            })
            .collect();
        def_candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let mut selected: Vec<usize> = def_candidates
            .into_iter()
            .take(cap)
            .map(|(_, line)| line)
            .collect();
        if selected.len() < cap {
            let non_imports: Vec<usize> = file_lines
                .iter()
                .copied()
                .filter(|&line| !is_import_line(document.line(line)))
                .collect();
            let high_signal: Vec<usize> = non_imports
                .iter()
                .copied()
                .filter(|&line| {
                    let text = document.line(line);
                    !(is_low_signal_usage_line(text)
                        || is_comment_only_line(text, is_doc_file)
                        || is_doc_file && crate::document::is_document_boilerplate_line(text))
                })
                .collect();
            let pool: &[usize] = if !high_signal.is_empty() {
                &high_signal
            } else if !non_imports.is_empty() {
                &non_imports
            } else {
                file_lines
            };
            // Anchor-aware selection (first + last + evenly strided middle),
            // preventing top-of-file imports from starving bottom-of-file call sites.
            if let Some(&first) = pool.first()
                && !selected.contains(&first)
            {
                selected.push(first);
            }
            if selected.len() < cap
                && pool.len() > 2
                && let Some(&last) = pool.last()
                && !selected.contains(&last)
            {
                selected.push(last);
            }
            if selected.len() < cap && !pool.is_empty() {
                let needed = cap - selected.len();
                let span = pool.len();
                for slot in 1..=needed {
                    let idx = (slot * span) / (needed + 1);
                    let candidate = pool[idx.min(span - 1)];
                    if !selected.contains(&candidate) {
                        selected.push(candidate);
                    }
                }
            }
            if selected.len() < cap {
                for &line in pool.iter().chain(non_imports.iter()).chain(file_lines.iter()) {
                    if !selected.contains(&line) {
                        selected.push(line);
                        if selected.len() == cap {
                            break;
                        }
                    }
                }
            }
        }
        selected.sort_unstable();
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
        || memchr::memchr2(b'\n', b'\0', query.pattern.as_bytes()).is_some()
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

pub(crate) fn strip_path_line_or_anchor_suffix(raw: &str) -> &str {
    let mut s = raw.trim();
    while let Some(rest) = s.strip_prefix("./") {
        if rest.is_empty() {
            break;
        }
        s = rest;
    }
    if s.len() > 1 {
        s = s.trim_end_matches('/');
    }
    if let Some((before_hash, after_hash)) = s.rsplit_once('#')
        && !before_hash.is_empty()
        && !after_hash.is_empty()
        && before_hash.contains('.')
    {
        s = before_hash;
    }
    if let Some((before_colon, after_colon)) = s.split_once(':')
        && before_colon.len() > 1
        && !after_colon.is_empty()
        && after_colon.as_bytes()[0].is_ascii_digit()
        && after_colon
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b':' | b'-'))
    {
        s = before_colon;
    }
    s
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
            let cleaned = strip_path_line_or_anchor_suffix(path);
            let target = if cleaned.is_empty() {
                path.as_str()
            } else {
                cleaned
            };
            safe_relative(Path::new(target))?;
            Ok(normalize(Path::new(target)))
        })
        .collect()
}

enum GlobFilter<'a> {
    None,
    Extension(&'a str),
    BraceExtensions(Vec<String>),
    ScopedExtensions {
        dir_prefix: &'a str,
        suffixes: Vec<String>,
    },
    Compiled(GlobSet),
    IncludeExclude {
        include: Option<GlobSet>,
        exclude: GlobSet,
    },
}

pub(crate) fn trim_leading_dot_slash(mut glob: &str) -> &str {
    while let Some(rest) = glob.strip_prefix("./") {
        if rest.is_empty() {
            break;
        }
        glob = rest;
    }
    glob
}

pub(crate) fn is_valid_compound_ext(ext: &str) -> bool {
    !ext.is_empty()
        && !ext.starts_with('.')
        && !ext.ends_with('.')
        && !ext.contains("..")
        && ext
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

pub(crate) fn simple_extension_suffix(glob: &str) -> Option<&str> {
    let glob = trim_leading_dot_slash(glob);
    if let Some(ext) = glob.strip_prefix("**/*.") {
        let suffix = &glob[4..];
        if is_valid_compound_ext(ext) {
            return Some(suffix);
        }
    }
    if let Some(ext) = glob.strip_prefix("**.") {
        let suffix = &glob[2..];
        if is_valid_compound_ext(ext) {
            return Some(suffix);
        }
    }
    if let Some(ext) = glob.strip_prefix("*.") {
        let suffix = &glob[1..];
        if is_valid_compound_ext(ext) {
            return Some(suffix);
        }
    }
    if let Some(ext) = glob.strip_prefix('.')
        && !ext.contains(['/', '\\', '*', '?', '[', ']', '{', '}'])
        && is_valid_compound_ext(ext)
    {
        return Some(glob);
    }
    None
}

pub(crate) fn brace_extension_suffixes(glob: &str) -> Option<Vec<String>> {
    let glob = trim_leading_dot_slash(glob.trim());
    if !glob.contains('{') && glob.contains(',') {
        let mut suffixes = Vec::with_capacity(4);
        for raw_part in glob.split(',') {
            let part = trim_leading_dot_slash(raw_part.trim());
            let ext = part
                .strip_prefix("**/*.")
                .or_else(|| part.strip_prefix("*."))?;
            if !is_valid_compound_ext(ext) {
                return None;
            }
            suffixes.push(format!(".{ext}"));
        }
        return (2..=16).contains(&suffixes.len()).then_some(suffixes);
    }
    let inner = glob
        .strip_prefix("**/*.{")
        .or_else(|| glob.strip_prefix("*.{"))?
        .strip_suffix('}')?;
    if inner.is_empty() {
        return None;
    }
    let mut suffixes = Vec::with_capacity(4);
    for raw_part in inner.split(',') {
        let part = raw_part.trim();
        if !is_valid_compound_ext(part) {
            return None;
        }
        suffixes.push(format!(".{part}"));
    }
    (2..=16).contains(&suffixes.len()).then_some(suffixes)
}

pub(crate) fn scoped_extension_glob(glob: &str) -> Option<(&str, Vec<String>)> {
    let glob = trim_leading_dot_slash(glob);
    let (dir_prefix, tail) = glob.split_once("/**/")?;
    if dir_prefix.is_empty()
        || dir_prefix.starts_with('/')
        || dir_prefix.ends_with('/')
        || dir_prefix.contains("//")
        || dir_prefix.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..")
        || !dir_prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/'))
    {
        return None;
    }
    if let Some(ext) = tail.strip_prefix("*.")
        && is_valid_compound_ext(ext)
    {
        return Some((dir_prefix, vec![format!(".{ext}")]));
    }
    if let Some(inner) = tail.strip_prefix("*.{").and_then(|s| s.strip_suffix('}')) {
        let mut suffixes = Vec::with_capacity(4);
        for raw_part in inner.split(',') {
            let part = raw_part.trim();
            if !is_valid_compound_ext(part) {
                return None;
            }
            suffixes.push(format!(".{part}"));
        }
        if (2..=16).contains(&suffixes.len()) {
            return Some((dir_prefix, suffixes));
        }
    }
    None
}

pub(crate) fn escape_dynamic_route_brackets(glob: &str) -> Option<String> {
    if !glob.contains('[') || !glob.contains(']') {
        return None;
    }
    let mut out = String::with_capacity(glob.len() + 8);
    let mut rest = glob;
    let mut escaped_any = false;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        if let Some(close) = after_open.find(']') {
            let inner = &after_open[..close];
            let is_route_param = inner.starts_with("...")
                || inner.starts_with('[')
                || (inner.len() >= 2
                    && inner
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
            if is_route_param {
                out.push_str("\\[");
                out.push_str(inner);
                out.push_str("\\]");
                escaped_any = true;
            } else {
                out.push('[');
                out.push_str(inner);
                out.push(']');
            }
            rest = &after_open[close + 1..];
        } else {
            out.push_str(&rest[open..]);
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    escaped_any.then_some(out)
}

fn glob(query: &Query) -> Result<GlobFilter<'_>> {
    let Some(raw_glob) = query.glob.as_deref() else {
        return Ok(GlobFilter::None);
    };
    if raw_glob.len() > 1024 {
        return Err(invalid("glob exceeds 1024 bytes").into());
    }
    let glob = trim_leading_dot_slash(raw_glob);
    if glob.starts_with('!') || (!glob.contains('{') && glob.contains(",!") || glob.contains(", !")) {
        let mut inc_builder = GlobSetBuilder::new();
        let mut exc_builder = GlobSetBuilder::new();
        let mut has_inc = false;
        for part in glob.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(neg) = part.strip_prefix('!') {
                let neg = trim_leading_dot_slash(neg.trim());
                if !neg.is_empty() {
                    exc_builder.add(Glob::new(neg)?);
                }
            } else {
                let pos = trim_leading_dot_slash(part);
                inc_builder.add(Glob::new(pos)?);
                has_inc = true;
            }
        }
        return Ok(GlobFilter::IncludeExclude {
            include: if has_inc {
                Some(inc_builder.build()?)
            } else {
                None
            },
            exclude: exc_builder.build()?,
        });
    }
    if let Some(suffix) = simple_extension_suffix(glob) {
        return Ok(GlobFilter::Extension(suffix));
    }
    if let Some(suffixes) = brace_extension_suffixes(glob) {
        return Ok(GlobFilter::BraceExtensions(suffixes));
    }
    if let Some((dir_prefix, suffixes)) = scoped_extension_glob(glob) {
        return Ok(GlobFilter::ScopedExtensions {
            dir_prefix,
            suffixes,
        });
    }
    let mut builder = GlobSetBuilder::new();
    builder.add(Glob::new(glob)?);
    if let Some(escaped) = escape_dynamic_route_brackets(glob)
        && let Ok(route_glob) = Glob::new(&escaped)
    {
        builder.add(route_glob);
    }
    Ok(GlobFilter::Compiled(builder.build()?))
}

#[inline]
pub(crate) fn ends_with_ignore_ascii_case(haystack: &str, suffix: &str) -> bool {
    haystack.ends_with(suffix)
        || (haystack.len() >= suffix.len()
            && haystack.as_bytes()[haystack.len() - suffix.len()..]
                .eq_ignore_ascii_case(suffix.as_bytes()))
}

fn path_allowed(
    resident: &Resident,
    id: usize,
    paths: &[std::path::PathBuf],
    glob: &GlobFilter<'_>,
) -> bool {
    let raw_path = resident.documents[id].path.as_ref();
    if !path_matches_str(raw_path, paths) {
        return false;
    }
    match glob {
        GlobFilter::None => true,
        GlobFilter::Extension(suffix) => ends_with_ignore_ascii_case(raw_path, suffix),
        GlobFilter::BraceExtensions(suffixes) => suffixes
            .iter()
            .any(|suffix| ends_with_ignore_ascii_case(raw_path, suffix.as_str())),
        GlobFilter::ScopedExtensions {
            dir_prefix,
            suffixes,
        } => {
            raw_path.starts_with(*dir_prefix)
                && raw_path.as_bytes().get(dir_prefix.len()) == Some(&b'/')
                && suffixes.iter().any(|suffix| {
                    ends_with_ignore_ascii_case(&raw_path[dir_prefix.len() + 1..], suffix.as_str())
                })
        }
        GlobFilter::Compiled(set) => {
            let path = Path::new(raw_path);
            set.is_match(path) || path.file_name().is_some_and(|name| set.is_match(name))
        }
        GlobFilter::IncludeExclude { include, exclude } => {
            let path = Path::new(raw_path);
            let file_name = path.file_name();
            if exclude.is_match(path) || file_name.is_some_and(|n| exclude.is_match(n)) {
                return false;
            }
            match include {
                Some(inc) => inc.is_match(path) || file_name.is_some_and(|n| inc.is_match(n)),
                None => true,
            }
        }
    }
}

pub(crate) fn path_matches_str(raw_path: &str, paths: &[std::path::PathBuf]) -> bool {
    if raw_path.is_empty() {
        return false;
    }
    if paths.is_empty() {
        return true;
    }
    paths.iter().any(|prefix| {
        if let Some(p) = prefix.to_str() {
            if p == "." || p.is_empty() {
                return true;
            }
            raw_path.starts_with(p)
                && (raw_path.len() == p.len()
                    || raw_path.as_bytes().get(p.len()) == Some(&b'/'))
        } else {
            Path::new(raw_path).starts_with(prefix)
        }
    })
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
