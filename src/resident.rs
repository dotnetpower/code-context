use crate::context::{ReadRange, Snippet, check_search_size, check_snippets_size, validate_ranges};
use crate::policy::MemoryPolicy;
use crate::search::{Hit, Search, SearchResult};
use crate::workspace::{FileInventory, Workspace, normalize, safe_relative};
use crate::{Result, invalid};
use memchr::memmem::Finder;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

pub(crate) const MAX_POSTINGS: usize = 64 * 1024 * 1024;
const DENSE_TRIGRAM_SLOTS: usize = 1 << 24;
const DENSE_TRIGRAM_MIN_SOURCE: usize = 16 * 1024 * 1024;
const MAX_QUERY_POSTING_INTERSECTIONS: usize = 8;

#[derive(Debug)]
pub(crate) struct ResidentAdmissionError {
    message: &'static str,
    required_bytes: usize,
}

impl ResidentAdmissionError {
    pub(crate) fn new(message: &'static str, required_bytes: usize) -> Self {
        Self {
            message,
            required_bytes,
        }
    }
}

#[derive(Debug)]
struct SnapshotMissError {
    path: String,
}

#[derive(Debug)]
struct SnapshotRangeError {
    path: String,
    start: usize,
    end: usize,
    lines: usize,
}

impl Display for SnapshotRangeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "READ_RANGE_OUT_OF_BOUNDS path={} requested={}..{} indexed_lines={}: start exceeds snapshot EOF; locate the actual lines with code_context, then request a range within this file. Do not repeat the same invalid range.",
            self.path, self.start, self.end, self.lines
        )
    }
}

impl std::error::Error for SnapshotRangeError {}

impl Display for ResidentAdmissionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} (required_bytes={})",
            self.message, self.required_bytes
        )
    }
}

impl std::error::Error for ResidentAdmissionError {}

impl Display for SnapshotMissError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "file is absent from the permitted snapshot: {}",
            self.path
        )
    }
}

impl std::error::Error for SnapshotMissError {}

pub(crate) fn is_admission_error(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<ResidentAdmissionError>().is_some()
}

pub(crate) fn admission_required_bytes(error: &(dyn std::error::Error + 'static)) -> Option<usize> {
    error
        .downcast_ref::<ResidentAdmissionError>()
        .map(|error| error.required_bytes)
}

pub(crate) fn is_snapshot_miss(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<SnapshotMissError>().is_some()
}

pub(crate) fn is_snapshot_range_error(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<SnapshotRangeError>().is_some()
}

#[cfg(test)]
mod admission_tests {
    use super::{ResidentAdmissionError, admission_required_bytes, is_admission_error};
    use crate::invalid;

    #[test]
    fn admission_errors_are_typed_instead_of_string_matched() {
        let admission = ResidentAdmissionError::new("resident admission budget exceeded", 1234);
        let invalid = invalid("resident admission budget exceeded");
        assert!(is_admission_error(&admission));
        assert!(!is_admission_error(&invalid));
        assert_eq!(admission_required_bytes(&admission), Some(1234));
        assert_eq!(admission_required_bytes(&invalid), None);
    }
}

pub(crate) struct Document {
    pub path: Arc<str>,
    pub text: String,
    pub starts: Vec<u32>,
    pub sha256: [u8; 32],
}

impl Document {
    pub(crate) fn line(&self, index: usize) -> &str {
        let end = self
            .starts
            .get(index + 1)
            .copied()
            .map(|offset| offset as usize)
            .unwrap_or(self.text.len());
        let start = self.starts[index] as usize;
        self.text[start..end]
            .strip_suffix('\n')
            .unwrap_or(&self.text[start..end])
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Stats {
    pub files: usize,
    pub source_bytes: usize,
    pub source_sha256: String,
    pub source_load_ms: f64,
    pub index_build_ms: f64,
    pub unique_trigrams: usize,
    pub postings: usize,
    pub estimated_owned_bytes: usize,
    pub generation: u64,
    pub path_bytes: usize,
    pub line_count: usize,
    pub line_offset_bytes: usize,
    pub document_bytes: usize,
    pub posting_bytes_estimate: usize,
}

pub struct Resident {
    pub(crate) documents: Vec<Document>,
    pub(crate) paths: BTreeMap<Arc<str>, usize>,
    pub(crate) postings: Option<HashMap<u32, Vec<u32>>>,
    pub(crate) policy: MemoryPolicy,
    pub(crate) max_owned_bytes: usize,
    pub stats: Stats,
}

impl Resident {
    pub fn load(workspace: &Workspace, indexed: bool, generation: u64) -> Result<Self> {
        Self::load_with_policy(
            workspace,
            indexed,
            generation,
            MemoryPolicy::default(),
            usize::MAX,
        )
    }

    pub fn load_with_policy(
        workspace: &Workspace,
        indexed: bool,
        generation: u64,
        policy: MemoryPolicy,
        max_owned_bytes: usize,
    ) -> Result<Self> {
        policy.validate().map_err(invalid)?;
        let inventory = workspace.files_limited(&policy)?;
        Self::load_with_inventory(
            workspace,
            indexed,
            generation,
            policy,
            max_owned_bytes,
            inventory,
        )
    }

    pub(crate) fn load_with_inventory(
        workspace: &Workspace,
        indexed: bool,
        generation: u64,
        policy: MemoryPolicy,
        max_owned_bytes: usize,
        inventory: FileInventory,
    ) -> Result<Self> {
        policy.validate().map_err(invalid)?;
        let source_floor = usize::try_from(inventory.metadata_source_bytes)?
            .saturating_add(inventory.total_path_bytes.saturating_mul(2))
            .saturating_add(inventory.files.len().saturating_mul(128));
        if source_floor > max_owned_bytes {
            return Err(ResidentAdmissionError::new(
                "resident admission budget exceeded by source inventory",
                source_floor,
            ).into());
        }
        let start = Instant::now();
        let allowed = inventory.files;
        let mut paths: Vec<_> = allowed.iter().collect();
        paths.sort();
        let mut documents = Vec::new();
        let mut source_bytes = 0;
        let mut line_count = 0usize;
        let mut digest = Sha256::new();
        for path in paths {
            let name = path.to_str().ok_or_else(|| invalid("non-UTF8 path"))?;
            let bytes = workspace.read_allowed_with_limit(name, &allowed, policy.max_file_bytes)?;
            source_bytes += bytes.len();
            if source_bytes > policy.max_source_bytes {
                return Err(invalid("resident source budget exceeds configured limit").into());
            }
            digest.update((name.len() as u64).to_le_bytes());
            digest.update(name.as_bytes());
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(&bytes);
            let sha256: [u8; 32] = Sha256::digest(&bytes).into();
            let text = String::from_utf8(bytes)?;
            let mut starts = Vec::new();
            if !text.is_empty() {
                starts.push(0);
            }
            for (i, byte) in text.bytes().enumerate() {
                if byte == b'\n' && i + 1 < text.len() {
                    starts.push(u32::try_from(i + 1)?);
                }
            }
            line_count = line_count.saturating_add(starts.len());
            if line_count > policy.max_total_lines {
                return Err(invalid("resident line count exceeds configured limit").into());
            }
            documents.push(Document {
                path: Arc::from(name),
                text,
                starts,
                sha256,
            });
            let floor = source_bytes
                .saturating_add(inventory.total_path_bytes.saturating_mul(2))
                .saturating_add(line_count.saturating_mul(std::mem::size_of::<u32>()))
                .saturating_add(documents.len().saturating_mul(128));
            if floor > max_owned_bytes {
                return Err(ResidentAdmissionError::new(
                    "resident admission budget exceeded while loading files",
                    floor,
                )
                .into());
            }
        }
        drop(allowed);
        let paths = documents
            .iter()
            .enumerate()
            .map(|(id, d)| (d.path.clone(), id))
            .collect();
        let stats = Stats {
            files: documents.len(),
            source_bytes,
            source_sha256: format!("{:x}", digest.finalize()),
            source_load_ms: start.elapsed().as_secs_f64() * 1000.0,
            index_build_ms: 0.0,
            unique_trigrams: 0,
            postings: 0,
            estimated_owned_bytes: 0,
            generation,
            path_bytes: inventory.total_path_bytes,
            line_count,
            line_offset_bytes: line_count * std::mem::size_of::<u32>(),
            document_bytes: source_bytes,
            posting_bytes_estimate: 0,
        };
        let mut resident = Self {
            documents,
            paths,
            postings: None,
            policy,
            max_owned_bytes,
            stats,
        };
        if indexed {
            resident.build_index()?;
        }
        resident.stats.estimated_owned_bytes = resident.estimated_bytes();
        resident.stats.posting_bytes_estimate = resident.posting_bytes_estimate();
        if resident.stats.estimated_owned_bytes > max_owned_bytes {
            return Err(ResidentAdmissionError::new(
                "resident admission budget exceeded after indexing",
                resident.stats.estimated_owned_bytes,
            )
            .into());
        }
        Ok(resident)
    }

    fn build_index(&mut self) -> Result<()> {
        let start = Instant::now();
        let mut index: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut count = 0usize;
        let base_owned = self.estimated_bytes();
        let dense_bytes = DENSE_TRIGRAM_SLOTS * std::mem::size_of::<u16>();
        let mut dense_seen = (self.stats.source_bytes >= DENSE_TRIGRAM_MIN_SOURCE
            && base_owned.saturating_add(dense_bytes) <= self.max_owned_bytes)
            .then(|| vec![0_u16; DENSE_TRIGRAM_SLOTS]);
        let mut dense_epoch = 1_u16;
        for (id, document) in self.documents.iter().enumerate() {
            let keys = if let Some(seen) = dense_seen.as_mut() {
                let key_capacity = document
                    .text
                    .len()
                    .saturating_sub(2)
                    .min(self.policy.max_unique_trigrams_per_file.saturating_add(1));
                let transient = base_owned
                    .saturating_add(dense_bytes)
                    .saturating_add(key_capacity.saturating_mul(std::mem::size_of::<u32>()))
                    .saturating_add(index.capacity().saturating_mul(40))
                    .saturating_add(count.saturating_mul(std::mem::size_of::<u32>()));
                if transient > self.max_owned_bytes {
                    return Err(ResidentAdmissionError::new(
                        "resident admission budget exceeded by dense index build peak",
                        transient,
                    )
                    .into());
                }
                let mut keys = Vec::with_capacity(key_capacity);
                for key in document.text.as_bytes().windows(3).map(trigram) {
                    let slot = &mut seen[key as usize];
                    if *slot != dense_epoch {
                        *slot = dense_epoch;
                        keys.push(key);
                        if keys.len() > self.policy.max_unique_trigrams_per_file {
                            return Err(invalid(format!(
                                "file exceeds max_unique_trigrams_per_file: {}",
                                document.path
                            ))
                            .into());
                        }
                    }
                }
                if dense_epoch == u16::MAX {
                    seen.fill(0);
                    dense_epoch = 1;
                } else {
                    dense_epoch += 1;
                }
                keys
            } else {
                let mut unique = HashSet::new();
                for key in document.text.as_bytes().windows(3).map(trigram) {
                    let inserted = unique.insert(key);
                    if inserted && unique.len() > self.policy.max_unique_trigrams_per_file {
                        return Err(invalid(format!(
                            "file exceeds max_unique_trigrams_per_file: {}",
                            document.path
                        ))
                        .into());
                    }
                    if inserted && unique.len() % 16_384 == 0 {
                        let transient = base_owned
                            .saturating_add(unique.capacity().saturating_mul(36))
                            .saturating_add(index.capacity().saturating_mul(40))
                            .saturating_add(count.saturating_mul(std::mem::size_of::<u32>()));
                        if transient > self.max_owned_bytes {
                            return Err(ResidentAdmissionError::new(
                                "resident admission budget exceeded by index build peak",
                                transient,
                            )
                            .into());
                        }
                    }
                }
                let mut keys: Vec<u32> = unique.into_iter().collect();
                keys.sort_unstable();
                keys
            };
            count += keys.len();
            if count > MAX_POSTINGS {
                return Err(invalid("resident index budget exceeds 64M postings").into());
            }
            for key in keys {
                if index.len() >= self.policy.max_unique_trigrams && !index.contains_key(&key) {
                    return Err(invalid("root exceeds max_unique_trigrams").into());
                }
                index.entry(key).or_default().push(u32::try_from(id)?);
            }
            let projected = self
                .stats
                .document_bytes
                .saturating_add(self.stats.path_bytes.saturating_mul(2))
                .saturating_add(self.stats.line_offset_bytes)
                .saturating_add(count.saturating_mul(std::mem::size_of::<u32>()))
                .saturating_add(index.len().saturating_mul(64));
            if projected > self.max_owned_bytes {
                return Err(ResidentAdmissionError::new(
                    "resident admission budget exceeded while building index",
                    projected,
                )
                .into());
            }
        }
        self.stats.index_build_ms = start.elapsed().as_secs_f64() * 1000.0;
        self.stats.unique_trigrams = index.len();
        self.stats.postings = count;
        self.postings = Some(index);
        Ok(())
    }

    pub(crate) fn estimated_bytes(&self) -> usize {
        let source: usize = self
            .documents
            .iter()
            .map(|d| {
                std::mem::size_of::<Document>()
                    + d.path.len()
                    + d.text.capacity()
                    + d.sha256.len()
                    + d.starts.capacity() * std::mem::size_of::<u32>()
            })
            .sum();
        let index = self.postings.as_ref().map_or(0, |p| {
            p.capacity() * (std::mem::size_of::<(u32, Vec<u32>)>() + 1)
                + p.values().map(|v| v.capacity() * 4).sum::<usize>()
        });
        source + index + self.paths.keys().map(|p| p.len() + 64).sum::<usize>()
    }

    pub(crate) fn posting_bytes_estimate(&self) -> usize {
        self.postings.as_ref().map_or(0, |postings| {
            postings.capacity() * (std::mem::size_of::<(u32, Vec<u32>)>() + 1)
                + postings
                    .values()
                    .map(|ids| ids.capacity() * std::mem::size_of::<u32>())
                    .sum::<usize>()
        })
    }

    pub fn candidates(&self, pattern: &str) -> Vec<usize> {
        self.candidates_bytes(pattern.as_bytes())
    }

    pub(crate) fn candidates_bytes(&self, pattern: &[u8]) -> Vec<usize> {
        let Some(index) = &self.postings else {
            return (0..self.documents.len()).collect();
        };
        if pattern.len() < 3 {
            return (0..self.documents.len()).collect();
        }
        let mut keys: Vec<_> = pattern.windows(3).map(trigram).collect();
        keys.sort_unstable();
        keys.dedup();
        let Some(mut lists) = keys
            .iter()
            .map(|k| index.get(k))
            .collect::<Option<Vec<_>>>()
        else {
            return Vec::new();
        };
        lists.sort_by_key(|list| list.len());
        lists.truncate(MAX_QUERY_POSTING_INTERSECTIONS);
        lists[0]
            .iter()
            .filter(|id| lists[1..].iter().all(|list| list.binary_search(id).is_ok()))
            .map(|id| *id as usize)
            .collect()
    }

    pub(crate) fn candidate_bytes(&self, ids: &[usize]) -> usize {
        ids.iter().map(|id| self.documents[*id].text.len()).sum()
    }

    pub fn search(&self, request: &Search) -> Result<SearchResult> {
        if request.pattern.is_empty()
            || request.pattern.contains('\n')
            || request.pattern.contains('\0')
            || request.regex
            || request.ignore_case
            || request.limit == 0
            || request.limit > 1000
        {
            return Err(invalid("resident experiment supports nonempty, case-sensitive, single-line literals only; limit 1..1000; use live backend for regex/case folding").into());
        }
        let finder = Finder::new(request.pattern.as_bytes());
        let mut matches = Vec::new();
        let mut total_matches = 0;
        let mut candidates = self.candidates(&request.pattern);
        candidates.sort_unstable_by(|a, b| self.documents[*a].path.cmp(&self.documents[*b].path));
        for id in candidates {
            let document = &self.documents[id];
            let mut previous_line = None;
            for offset in finder.find_iter(document.text.as_bytes()) {
                let line = document
                    .starts
                    .partition_point(|start| (*start as usize) <= offset)
                    - 1;
                if previous_line == Some(line) {
                    continue;
                }
                previous_line = Some(line);
                total_matches += 1;
                if matches.len() < request.limit {
                    matches.push(Hit {
                        path: document.path.to_string(),
                        line: line + 1,
                        text: document.line(line).into(),
                    });
                }
            }
        }
        let result = SearchResult {
            matches,
            total_matches,
            truncated: total_matches > request.limit,
            consistency: format!(
                "caller-managed RAM snapshot generation {}; filesystem updates require the enclosing watcher or an explicit refresh; not an atomic capture of live files",
                self.stats.generation
            ),
        };
        check_search_size(&result)?;
        Ok(result)
    }

    pub fn read_many(&self, ranges: &[ReadRange]) -> Result<Vec<Snippet>> {
        validate_ranges(ranges)?;
        let mut snippets = Vec::new();
        for range in ranges {
            safe_relative(Path::new(&range.path))?;
            let path = normalize(Path::new(&range.path));
            let name = path.to_str().ok_or_else(|| invalid("non-UTF8 path"))?;
            let id = self
                .paths
                .get(name)
                .ok_or_else(|| SnapshotMissError { path: name.into() })?;
            let document = &self.documents[*id];
            if range.start > document.starts.len() {
                return Err(SnapshotRangeError {
                    path: name.into(),
                    start: range.start,
                    end: range.end,
                    lines: document.starts.len(),
                }
                .into());
            }
            let end = range.end.min(document.starts.len());
            snippets.push(Snippet {
                path: name.into(),
                start: range.start,
                end,
                sha256: digest_hex(&document.sha256),
                lines: (range.start - 1..end)
                    .map(|line| document.line(line).into())
                    .collect(),
            });
        }
        check_snippets_size(&snippets)?;
        Ok(snippets)
    }

    pub fn candidate_stats(&self, pattern: &str) -> (usize, usize) {
        let ids = self.candidates(pattern);
        let bytes = ids.iter().map(|id| self.documents[*id].text.len()).sum();
        (ids.len(), bytes)
    }
}

pub(crate) fn trigram(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
}

pub(crate) fn digest_hex(digest: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut output = String::with_capacity(64);
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}
