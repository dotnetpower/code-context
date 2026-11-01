use crate::context::{ReadRange, Snippet, check_search_size, check_snippets_size, validate_ranges};
use crate::policy::MemoryPolicy;
use crate::search::{Hit, Search, SearchResult};
use crate::workspace::{FileInventory, Workspace, normalize, safe_relative};
use crate::{Result, invalid};
use memchr::memmem::Finder;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fmt::{Display, Formatter};
use std::hash::{BuildHasher, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy, Default)]
pub(crate) struct TrigramBuildHasher;

#[derive(Default)]
pub(crate) struct TrigramHasher(u64);

impl Hasher for TrigramHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        for &b in bytes {
            h = (h ^ u64::from(b)).wrapping_mul(0x517cc1b727220a95);
        }
        self.0 = h;
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        let mut x = u64::from(i).wrapping_mul(0x9e3779b97f4a7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        self.0 = x ^ (x >> 27);
    }
}

impl BuildHasher for TrigramBuildHasher {
    type Hasher = TrigramHasher;

    #[inline]
    fn build_hasher(&self) -> Self::Hasher {
        TrigramHasher(0)
    }
}

pub(crate) type TrigramMap<V> = HashMap<u32, V, TrigramBuildHasher>;

#[derive(Clone, Debug)]
pub(crate) enum PostingList {
    Inline { len: u8, ids: [u32; 4] },
    Heap(Vec<u32>),
}

impl Default for PostingList {
    #[inline]
    fn default() -> Self {
        Self::Inline {
            len: 0,
            ids: [0; 4],
        }
    }
}

impl PostingList {
    #[inline]
    pub(crate) fn single(id: u32) -> Self {
        Self::Inline {
            len: 1,
            ids: [id, 0, 0, 0],
        }
    }

    #[inline]
    pub(crate) fn as_slice(&self) -> &[u32] {
        match self {
            Self::Inline { len, ids } => &ids[..usize::from(*len)],
            Self::Heap(v) => v.as_slice(),
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Inline { len, .. } => usize::from(*len),
            Self::Heap(v) => v.len(),
        }
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub(crate) fn heap_bytes(&self) -> usize {
        match self {
            Self::Inline { .. } => 0,
            Self::Heap(v) => v.capacity() * std::mem::size_of::<u32>(),
        }
    }

    #[inline]
    pub(crate) fn push(&mut self, id: u32) {
        match self {
            Self::Inline { len, ids } => {
                let l = usize::from(*len);
                if l < 4 {
                    ids[l] = id;
                    *len += 1;
                } else {
                    let mut v = Vec::with_capacity(8);
                    v.extend_from_slice(ids);
                    v.push(id);
                    *self = Self::Heap(v);
                }
            }
            Self::Heap(v) => v.push(id),
        }
    }

    #[inline]
    pub(crate) fn insert_sorted(&mut self, id: u32) {
        match self {
            Self::Inline { len, ids } => {
                let l = usize::from(*len);
                let pos = ids[..l].binary_search(&id).unwrap_or_else(|i| i);
                if l < 4 {
                    ids.copy_within(pos..l, pos + 1);
                    ids[pos] = id;
                    *len += 1;
                } else {
                    let mut v = Vec::with_capacity(8);
                    v.extend_from_slice(&ids[..pos]);
                    v.push(id);
                    v.extend_from_slice(&ids[pos..4]);
                    *self = Self::Heap(v);
                }
            }
            Self::Heap(v) => {
                let pos = v.binary_search(&id).unwrap_or_else(|i| i);
                v.insert(pos, id);
            }
        }
    }

    #[inline]
    pub(crate) fn remove_sorted(&mut self, id: u32) {
        match self {
            Self::Inline { len, ids } => {
                let l = usize::from(*len);
                if let Ok(pos) = ids[..l].binary_search(&id) {
                    ids.copy_within((pos + 1)..l, pos);
                    *len -= 1;
                }
            }
            Self::Heap(v) => {
                if let Ok(pos) = v.binary_search(&id) {
                    v.remove(pos);
                }
            }
        }
    }
}

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
        let tail_start = self.lines.saturating_sub(79).max(1);
        write!(
            formatter,
            "READ_RANGE_OUT_OF_BOUNDS path={} requested={}..{} indexed_lines={} valid_tail={}..{}: start exceeds snapshot EOF; locate the actual lines with code_context, then request a range within this file. Do not repeat the same invalid range.",
            self.path, self.start, self.end, self.lines, tail_start, self.lines.max(1)
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
    pub sha256_hex: Arc<str>,
}

pub(crate) fn build_line_starts(text: &str) -> Result<Vec<u32>> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = text.as_bytes();
    let mut starts = Vec::with_capacity((bytes.len() / 36).max(1));
    starts.push(0);
    for i in memchr::memchr_iter(b'\n', bytes) {
        if i + 1 < bytes.len() {
            starts.push(u32::try_from(i + 1)?);
        }
    }
    if starts.capacity() > starts.len().saturating_add(64) {
        starts.shrink_to_fit();
    }
    Ok(starts)
}

impl Document {
    #[inline]
    pub(crate) fn line(&self, index: usize) -> &str {
        let mut end = self
            .starts
            .get(index + 1)
            .copied()
            .map(|offset| offset as usize)
            .unwrap_or(self.text.len());
        let start = self.starts[index] as usize;
        if end > start && self.text.as_bytes()[end - 1] == b'\n' {
            end -= 1;
        }
        &self.text[start..end]
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
    pub(crate) postings: Option<TrigramMap<PostingList>>,
    pub(crate) free_ids: Vec<usize>,
    pub(crate) policy: MemoryPolicy,
    pub(crate) max_owned_bytes: usize,
    pub(crate) ids_are_path_sorted: bool,
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
            let sha256_hex: Arc<str> = Arc::from(digest_hex(&sha256));
            let text = String::from_utf8(bytes)?;
            let starts = build_line_starts(&text)?;
            line_count = line_count.saturating_add(starts.len());
            if line_count > policy.max_total_lines {
                return Err(invalid("resident line count exceeds configured limit").into());
            }
            documents.push(Document {
                path: Arc::from(name),
                text,
                starts,
                sha256,
                sha256_hex,
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
            free_ids: Vec::new(),
            policy,
            max_owned_bytes,
            ids_are_path_sorted: true,
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
        let base_owned = self.estimated_bytes();
        let estimated_trigrams = (self.stats.source_bytes / 2)
            .max(64)
            .min(self.policy.max_unique_trigrams)
            .min(131_072);
        let prealloc_map_bytes = estimated_trigrams.saturating_mul(40);
        let mut index: TrigramMap<PostingList> =
            if base_owned.saturating_add(prealloc_map_bytes) <= self.max_owned_bytes {
                TrigramMap::with_capacity_and_hasher(estimated_trigrams, TrigramBuildHasher)
            } else {
                TrigramMap::default()
            };
        let mut count = 0usize;
        let dense_bytes = DENSE_TRIGRAM_SLOTS * std::mem::size_of::<u16>();
        let mut dense_seen = (self.stats.source_bytes >= DENSE_TRIGRAM_MIN_SOURCE
            && base_owned.saturating_add(dense_bytes) <= self.max_owned_bytes)
            .then(|| vec![0_u16; DENSE_TRIGRAM_SLOTS]);
        let mut dense_epoch = 1_u16;
        if dense_seen.is_none()
            && self.documents.len() >= 16
            && self.stats.source_bytes >= 2 * 1024 * 1024
            && base_owned.saturating_add(self.stats.source_bytes.saturating_mul(4))
                <= self.max_owned_bytes
        {
            let max_per_file = self.policy.max_unique_trigrams_per_file;
            let docs = &self.documents;
            let workers = docs.len().min(4);
            let chunk_size = docs.len().div_ceil(workers);
            let per_doc_keys = std::thread::scope(|scope| {
                let handles: Vec<_> = docs
                    .chunks(chunk_size)
                    .map(|chunk| {
                        std::thread::Builder::new()
                            .name("cctx-idx".into())
                            .stack_size(crate::policy::WORKER_STACK_BYTES)
                            .spawn_scoped(scope, move || {
                                let mut out = Vec::with_capacity(chunk.len());
                                let mut recent = [u32::MAX; 4096];
                                for doc in chunk {
                                    recent.fill(u32::MAX);
                                    let bytes = doc.text.as_bytes();
                                    let mut keys = Vec::with_capacity(
                                        (bytes.len() / 2)
                                            .min(max_per_file.saturating_add(1))
                                            .min(16_384),
                                    );
                                    let mut compact_threshold = 16_384usize;
                                    for key in bytes.windows(3).map(trigram) {
                                        let slot = &mut recent
                                            [((key.wrapping_mul(0x9e3779b9)) >> 20) as usize];
                                        if *slot == key {
                                            continue;
                                        }
                                        *slot = key;
                                        keys.push(key);
                                        if keys.len() >= compact_threshold {
                                            keys.sort_unstable();
                                            keys.dedup();
                                            if keys.len() > max_per_file {
                                                return Err(invalid(format!(
                                                    "file exceeds max_unique_trigrams_per_file: {}",
                                                    doc.path
                                                ))
                                                .into());
                                            }
                                            compact_threshold =
                                                (keys.len() + 16_384).max(compact_threshold);
                                        }
                                    }
                                    keys.sort_unstable();
                                    keys.dedup();
                                    if keys.len() > max_per_file {
                                        return Err(invalid(format!(
                                            "file exceeds max_unique_trigrams_per_file: {}",
                                            doc.path
                                        ))
                                        .into());
                                    }
                                    out.push(keys);
                                }
                                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(out)
                            })
                    })
                    .collect::<std::io::Result<Vec<_>>>()?;
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .map_err(|_| invalid("index worker panicked").into())
                            .and_then(|r| r)
                    })
                    .collect::<Result<Vec<_>>>()
            })?;
            for (id, keys) in per_doc_keys.into_iter().flatten().enumerate() {
                count += keys.len();
                if count > MAX_POSTINGS {
                    return Err(invalid("resident index budget exceeds 64M postings").into());
                }
                let doc_id = u32::try_from(id)?;
                for key in keys {
                    if index.len() >= self.policy.max_unique_trigrams && !index.contains_key(&key) {
                        return Err(invalid("root exceeds max_unique_trigrams").into());
                    }
                    index.entry(key).or_default().push(doc_id);
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
            return Ok(());
        }
        let mut recent = [u32::MAX; 4096];
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
                recent.fill(u32::MAX);
                let bytes = document.text.as_bytes();
                let window_count = bytes.len().saturating_sub(2);
                let mut keys = Vec::with_capacity(
                    (window_count / 2)
                        .min(self.policy.max_unique_trigrams_per_file.saturating_add(1))
                        .min(16_384),
                );
                let mut compact_threshold = 16_384usize;
                for key in bytes.windows(3).map(trigram) {
                    let slot = &mut recent[((key.wrapping_mul(0x9e3779b9)) >> 20) as usize];
                    if *slot == key {
                        continue;
                    }
                    *slot = key;
                    keys.push(key);
                    if keys.len() >= compact_threshold {
                        let transient = base_owned
                            .saturating_add(keys.capacity().saturating_mul(std::mem::size_of::<u32>()))
                            .saturating_add(index.capacity().saturating_mul(40))
                            .saturating_add(count.saturating_mul(std::mem::size_of::<u32>()));
                        if transient > self.max_owned_bytes {
                            return Err(ResidentAdmissionError::new(
                                "resident admission budget exceeded by index build peak",
                                transient,
                            )
                            .into());
                        }
                        keys.sort_unstable();
                        keys.dedup();
                        if keys.len() > self.policy.max_unique_trigrams_per_file {
                            return Err(invalid(format!(
                                "file exceeds max_unique_trigrams_per_file: {}",
                                document.path
                            ))
                            .into());
                        }
                        compact_threshold = (keys.len() + 16_384).max(compact_threshold);
                    }
                }
                keys.sort_unstable();
                keys.dedup();
                if keys.len() > self.policy.max_unique_trigrams_per_file {
                    return Err(invalid(format!(
                        "file exceeds max_unique_trigrams_per_file: {}",
                        document.path
                    ))
                    .into());
                }
                keys
            };
            count += keys.len();
            if count > MAX_POSTINGS {
                return Err(invalid("resident index budget exceeds 64M postings").into());
            }
            let doc_id = u32::try_from(id)?;
            let max_unique_trigrams = self.policy.max_unique_trigrams;
            let mut current_len = index.len();
            for key in keys {
                match index.entry(key) {
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        entry.get_mut().push(doc_id);
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        if current_len >= max_unique_trigrams {
                            return Err(invalid("root exceeds max_unique_trigrams").into());
                        }
                        current_len += 1;
                        entry.insert(PostingList::single(doc_id));
                    }
                }
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
                    + d.sha256_hex.len()
                    + d.starts.capacity() * std::mem::size_of::<u32>()
            })
            .sum();
        let index = self.postings.as_ref().map_or(0, |p| {
            p.capacity() * (std::mem::size_of::<(u32, PostingList)>() + 1)
                + p.values().map(PostingList::heap_bytes).sum::<usize>()
        });
        source
            + index
            + self
                .stats
                .path_bytes
                .saturating_add(self.paths.len().saturating_mul(64))
    }

    pub(crate) fn posting_bytes_estimate(&self) -> usize {
        self.postings.as_ref().map_or(0, |postings| {
            postings.capacity() * (std::mem::size_of::<(u32, PostingList)>() + 1)
                + postings
                    .values()
                    .map(PostingList::heap_bytes)
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
        let window_count = pattern.len() - 2;
        if window_count <= 8 {
            let mut stack_keys = [0_u32; 8];
            let mut key_len = 0usize;
            for window in pattern.windows(3) {
                let k = trigram(window);
                if !stack_keys[..key_len].contains(&k) {
                    stack_keys[key_len] = k;
                    key_len += 1;
                }
            }
            let mut keyed_lists: [(u32, &[u32]); 8] = [(0, &[]); 8];
            for (i, &k) in stack_keys[..key_len].iter().enumerate() {
                let Some(list) = index.get(&k) else {
                    return Vec::new();
                };
                let slice = list.as_slice();
                if slice.is_empty() {
                    return Vec::new();
                }
                keyed_lists[i] = (k, slice);
            }
            let active = &mut keyed_lists[..key_len];
            active.sort_unstable_by_key(|&(k, list)| {
                let b0 = (k & 0xff) as u8;
                let b1 = ((k >> 8) & 0xff) as u8;
                let b2 = ((k >> 16) & 0xff) as u8;
                let non_ascii =
                    usize::from(b0 >= 0x80) + usize::from(b1 >= 0x80) + usize::from(b2 >= 0x80);
                (list.len(), non_ascii)
            });
            if key_len == 1 {
                return active[0].1.iter().map(|&id| id as usize).collect();
            }
            let first = active[0].1;
            let mut cursors: [&[u32]; 7] = [&[]; 7];
            let cursor_len = key_len - 1;
            for (i, &(_, list)) in active[1..].iter().enumerate() {
                cursors[i] = list;
            }
            let active_cursors = &mut cursors[..cursor_len];
            let mut out = Vec::with_capacity(first.len());
            'outer_small: for &id in first {
                for cursor in active_cursors.iter_mut() {
                    match cursor.binary_search(&id) {
                        Ok(pos) => {
                            *cursor = &cursor[pos + 1..];
                        }
                        Err(pos) => {
                            if pos == cursor.len() {
                                break 'outer_small;
                            }
                            *cursor = &cursor[pos..];
                            continue 'outer_small;
                        }
                    }
                }
                out.push(id as usize);
            }
            return out;
        }
        let mut keys: Vec<_> = pattern.windows(3).map(trigram).collect();
        keys.sort_unstable();
        keys.dedup();
        let Some(mut keyed_lists) = keys
            .iter()
            .map(|&k| index.get(&k).map(|v| (k, v.as_slice())))
            .collect::<Option<Vec<_>>>()
        else {
            return Vec::new();
        };
        if keyed_lists.iter().any(|(_, list)| list.is_empty()) {
            return Vec::new();
        }
        // Prefer shortest posting lists first; break ties by preferring ASCII trigrams
        // over UTF-8 continuation-heavy trigrams for higher selectivity.
        keyed_lists.sort_unstable_by_key(|&(k, list)| {
            let b0 = (k & 0xff) as u8;
            let b1 = ((k >> 8) & 0xff) as u8;
            let b2 = ((k >> 16) & 0xff) as u8;
            let non_ascii = usize::from(b0 >= 0x80) + usize::from(b1 >= 0x80) + usize::from(b2 >= 0x80);
            (list.len(), non_ascii)
        });
        if keyed_lists.len() > MAX_QUERY_POSTING_INTERSECTIONS {
            keyed_lists.truncate(MAX_QUERY_POSTING_INTERSECTIONS);
        }
        if keyed_lists.len() == 1 {
            return keyed_lists[0].1.iter().map(|&id| id as usize).collect();
        }
        let first = keyed_lists[0].1;
        let mut cursors: [&[u32]; MAX_QUERY_POSTING_INTERSECTIONS] = [&[]; MAX_QUERY_POSTING_INTERSECTIONS];
        let cursor_len = keyed_lists.len() - 1;
        for (i, &(_, list)) in keyed_lists[1..].iter().enumerate() {
            cursors[i] = list;
        }
        let active_cursors = &mut cursors[..cursor_len];
        let mut out = Vec::with_capacity(first.len());
        'outer: for &id in first {
            for cursor in active_cursors.iter_mut() {
                match cursor.binary_search(&id) {
                    Ok(pos) => {
                        *cursor = &cursor[pos + 1..];
                    }
                    Err(pos) => {
                        if pos == cursor.len() {
                            break 'outer;
                        }
                        *cursor = &cursor[pos..];
                        continue 'outer;
                    }
                }
            }
            out.push(id as usize);
        }
        out
    }

    pub(crate) fn candidates_byte_classes(&self, positions: &[[u8; 2]]) -> Vec<usize> {
        let Some(index) = &self.postings else {
            return (0..self.documents.len()).collect();
        };
        if positions.len() < 3 {
            return (0..self.documents.len()).collect();
        }
        let mut window_lists: Vec<Vec<u32>> = Vec::new();
        let mut seen_windows: Vec<&[[u8; 2]]> = Vec::new();
        for window in positions.windows(3) {
            if seen_windows.contains(&window) {
                continue;
            }
            seen_windows.push(window);
            let mut trigram_keys = [0_u32; 8];
            let mut trigram_len = 0usize;
            for b0 in [window[0][0], window[0][1]] {
                for b1 in [window[1][0], window[1][1]] {
                    for b2 in [window[2][0], window[2][1]] {
                        let key = trigram(&[b0, b1, b2]);
                        if !trigram_keys[..trigram_len].contains(&key) {
                            trigram_keys[trigram_len] = key;
                            trigram_len += 1;
                        }
                    }
                }
            }
            let mut merged = Vec::new();
            for &key in &trigram_keys[..trigram_len] {
                if let Some(list) = index.get(&key) {
                    let slice = list.as_slice();
                    if merged.is_empty() {
                        merged.extend_from_slice(slice);
                    } else {
                        merge_u32_in_place(&mut merged, slice);
                    }
                }
            }
            if merged.is_empty() {
                return Vec::new();
            }
            window_lists.push(merged);
        }
        window_lists.sort_unstable_by_key(|list| list.len());
        window_lists.truncate(MAX_QUERY_POSTING_INTERSECTIONS);
        if window_lists.len() == 1 {
            return window_lists[0].iter().map(|&id| id as usize).collect();
        }
        let first = window_lists[0].as_slice();
        let mut cursors: Vec<&[u32]> = window_lists[1..].iter().map(Vec::as_slice).collect();
        let mut out = Vec::with_capacity(first.len());
        'outer: for &id in first {
            for cursor in &mut cursors {
                match cursor.binary_search(&id) {
                    Ok(pos) => {
                        *cursor = &cursor[pos + 1..];
                    }
                    Err(pos) => {
                        if pos == cursor.len() {
                            break 'outer;
                        }
                        *cursor = &cursor[pos..];
                        continue 'outer;
                    }
                }
            }
            out.push(id as usize);
        }
        out
    }

    pub(crate) fn candidate_bytes(&self, ids: &[usize]) -> usize {
        if ids.len() == self.stats.files && self.free_ids.is_empty() {
            return self.stats.source_bytes;
        }
        ids.iter().map(|id| self.documents[*id].text.len()).sum()
    }

    pub fn search(&self, request: &Search) -> Result<SearchResult> {
        if request.pattern.is_empty()
            || memchr::memchr2(b'\n', b'\0', request.pattern.as_bytes()).is_some()
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
        if !self.ids_are_path_sorted {
            candidates.sort_unstable_by(|a, b| self.documents[*a].path.cmp(&self.documents[*b].path));
        }
        for id in candidates {
            let document = &self.documents[id];
            if document.starts.is_empty() {
                continue;
            }
            let bytes = document.text.as_bytes();
            let mut search_pos = 0usize;
            let mut line_cursor = 0usize;
            while search_pos < bytes.len() {
                let Some(rel) = finder.find(&bytes[search_pos..]) else {
                    break;
                };
                let offset = search_pos + rel;
                total_matches += 1;
                if matches.len() < request.limit {
                    let line = find_line_gallop(&document.starts, line_cursor, offset);
                    matches.push(Hit {
                        path: document.path.to_string(),
                        line: line + 1,
                        text: document.line(line).into(),
                    });
                    line_cursor = line + 1;
                    search_pos = document
                        .starts
                        .get(line_cursor)
                        .copied()
                        .map(|start| start as usize)
                        .unwrap_or(bytes.len());
                } else if let Some(nl) = memchr::memchr(b'\n', &bytes[offset..]) {
                    search_pos = offset + nl + 1;
                } else {
                    break;
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
        let mut snippets = Vec::with_capacity(ranges.len());
        for range in ranges {
            let (name_cow, id) = if let Some(&id) = self.paths.get(range.path.as_str()) {
                (std::borrow::Cow::Borrowed(range.path.as_str()), id)
            } else {
                safe_relative(Path::new(&range.path))?;
                let path = normalize(Path::new(&range.path));
                let name = path
                    .to_str()
                    .ok_or_else(|| invalid("non-UTF8 path"))?
                    .to_owned();
                let id = *self
                    .paths
                    .get(name.as_str())
                    .ok_or_else(|| SnapshotMissError { path: name.clone() })?;
                (std::borrow::Cow::Owned(name), id)
            };
            let document = &self.documents[id];
            if range.start > document.starts.len() {
                return Err(SnapshotRangeError {
                    path: name_cow.into_owned(),
                    start: range.start,
                    end: range.end,
                    lines: document.starts.len(),
                }
                .into());
            }
            let end = range.end.min(document.starts.len());
            let mut lines = Vec::with_capacity(end - range.start + 1);
            for line in (range.start - 1)..end {
                lines.push(document.line(line).into());
            }
            snippets.push(Snippet {
                path: name_cow.into_owned(),
                start: range.start,
                end,
                sha256: document.sha256_hex.to_string(),
                lines,
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

#[inline]
pub(crate) fn find_line_gallop(starts: &[u32], cursor: usize, offset: usize) -> usize {
    let rem = &starts[cursor..];
    if rem.len() <= 1 || (rem[1] as usize) > offset {
        return cursor;
    }
    if rem.len() == 2 || (rem[2] as usize) > offset {
        return cursor + 1;
    }
    if rem.len() == 3 || (rem[3] as usize) > offset {
        return cursor + 2;
    }
    cursor + 3 + rem[3..].partition_point(|&start| (start as usize) <= offset) - 1
}

fn merge_u32_in_place(target: &mut Vec<u32>, right: &[u32]) {
    if right.is_empty() {
        return;
    }
    let mut out = Vec::with_capacity(target.len() + right.len());
    let left = target.as_slice();
    let (mut i, mut j) = (0, 0);
    while i < left.len() && j < right.len() {
        match left[i].cmp(&right[j]) {
            std::cmp::Ordering::Less => {
                out.push(left[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(right[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(left[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&left[i..]);
    out.extend_from_slice(&right[j..]);
    *target = out;
}

pub(crate) fn trigram(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
}

pub(crate) fn digest_hex(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 64];
    for (i, &byte) in digest.iter().enumerate() {
        out[i * 2] = HEX[(byte >> 4) as usize];
        out[i * 2 + 1] = HEX[(byte & 0x0f) as usize];
    }
    String::from_utf8(out.to_vec()).expect("ASCII hex digits")
}
