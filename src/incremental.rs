use crate::policy::MemoryPolicy;
use crate::resident::{Document, MAX_POSTINGS, Resident, trigram};
use crate::workspace::Workspace;
use crate::{Result, invalid};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Stamp {
    length: u64,
    modified: std::time::SystemTime,
}

pub(crate) fn stamp(path: &Path) -> Result<Stamp> {
    let metadata = std::fs::metadata(path)?;
    Ok(Stamp {
        length: metadata.len(),
        modified: metadata.modified()?,
    })
}

pub(crate) fn stamps(
    workspace: &Workspace,
    policy: &MemoryPolicy,
) -> Result<BTreeMap<String, Stamp>> {
    workspace
        .files_limited(policy)?
        .files
        .iter()
        .map(|path| {
            Ok((
                path.to_str()
                    .ok_or_else(|| invalid("non-UTF8 path"))?
                    .into(),
                stamp(&workspace.root.join(path))?,
            ))
        })
        .collect()
}

pub(crate) fn validate_stamps(
    current: &BTreeMap<String, Stamp>,
    policy: &MemoryPolicy,
) -> Result<()> {
    if current.len() > policy.max_files
        || current
            .values()
            .map(|stamp| stamp.length as u128)
            .sum::<u128>()
            > policy.max_source_bytes as u128
        || current.keys().map(String::len).sum::<usize>() > policy.max_total_path_bytes
        || current
            .values()
            .any(|stamp| stamp.length > policy.max_file_bytes as u64)
    {
        return Err(
            invalid("incremental inventory exceeds configured file/source/path budget").into(),
        );
    }
    Ok(())
}

fn keys(text: &str, limit: usize) -> Result<Vec<u32>> {
    let mut unique = HashSet::new();
    for key in text.as_bytes().windows(3).map(trigram) {
        if unique.insert(key) && unique.len() > limit {
            return Err(invalid("changed file exceeds max_unique_trigrams_per_file").into());
        }
    }
    let mut keys: Vec<_> = unique.into_iter().collect();
    keys.sort_unstable();
    Ok(keys)
}

fn adjust_delta(
    deltas: &mut HashMap<u32, i64>,
    key: u32,
    amount: i64,
    max_entries: usize,
) -> Result<()> {
    if !deltas.contains_key(&key) && deltas.len() >= max_entries {
        return Err(invalid("trigram delta exceeds resident admission peak").into());
    }
    let value = deltas.entry(key).or_default();
    *value += amount;
    if *value == 0 {
        deltas.remove(&key);
    }
    Ok(())
}

fn accumulate_differences(
    deltas: &mut HashMap<u32, i64>,
    old: &[u32],
    new: &[u32],
    max_entries: usize,
) -> Result<()> {
    let (mut left, mut right) = (0, 0);
    while left < old.len() && right < new.len() {
        match old[left].cmp(&new[right]) {
            std::cmp::Ordering::Less => {
                adjust_delta(deltas, old[left], -1, max_entries)?;
                left += 1;
            }
            std::cmp::Ordering::Greater => {
                adjust_delta(deltas, new[right], 1, max_entries)?;
                right += 1;
            }
            std::cmp::Ordering::Equal => {
                left += 1;
                right += 1;
            }
        }
    }
    for key in &old[left..] {
        adjust_delta(deltas, *key, -1, max_entries)?;
    }
    for key in &new[right..] {
        adjust_delta(deltas, *key, 1, max_entries)?;
    }
    Ok(())
}

fn document(path: String, bytes: Vec<u8>) -> Result<Document> {
    let sha256: [u8; 32] = Sha256::digest(&bytes).into();
    let text = String::from_utf8(bytes)?;
    let mut starts = Vec::new();
    if !text.is_empty() {
        starts.push(0);
        for (index, byte) in text.bytes().enumerate() {
            if byte == b'\n' && index + 1 < text.len() {
                starts.push(u32::try_from(index + 1)?);
            }
        }
    }
    Ok(Document {
        path: Arc::from(path),
        text,
        starts,
        sha256,
    })
}

impl Resident {
    pub(crate) fn unchanged_update(&self, started:Instant) -> serde_json::Value {
        serde_json::json!({"changed_files":0,"update_ms":started.elapsed().as_secs_f64()*1000.0,
            "generation":self.stats.generation,"verified_all_source":false,
            "source_files_read":0,"source_bytes_read":0})
    }

    pub(crate) fn update(
        &mut self,
        workspace: &Workspace,
        changed: &BTreeSet<String>,
        current: &BTreeMap<String, Stamp>,
        verify_all: bool,
    ) -> Result<serde_json::Value> {
        let started = Instant::now();
        if changed.is_empty() && !verify_all {
            return Ok(self.unchanged_update(started));
        }
        let allowed: HashSet<PathBuf> = current.keys().map(PathBuf::from).collect();
        let mut replacements = Vec::new();
        let mut source_bytes = self.stats.source_bytes;
        let mut line_count = self.stats.line_count;
        let mut posting_count = self.stats.postings;
        let mut staged_bytes = 0usize;
        let mut staged_postings = 0usize;
        let current_owned = self.estimated_bytes();
        let mut staged_work_bytes = 0usize;
        let mut key_delta: HashMap<u32, i64> = HashMap::new();
        let mut source_files_read = 0usize;
        let mut source_bytes_read = 0usize;
        let targets: BTreeSet<_> = if verify_all {
            self.paths
                .keys()
                .map(|path| path.to_string())
                .chain(current.keys().cloned())
                .collect()
        } else {
            changed.clone()
        };
        for path in targets {
            let old = self.paths.get(path.as_str()).map(|id| &self.documents[*id]);
            let new = if allowed.contains(Path::new(&path)) {
                let bytes = workspace.read_allowed_with_limit(
                    &path,
                    &allowed,
                    self.policy.max_file_bytes,
                )?;
                source_files_read += 1;
                source_bytes_read += bytes.len();
                Some(document(path.clone(), bytes)?)
            } else {
                None
            };
            if old.map(|d| &d.sha256) == new.as_ref().map(|d| &d.sha256) {
                continue;
            }
            let old_keys = match old {
                Some(document) => keys(&document.text, self.policy.max_unique_trigrams_per_file)?,
                None => Vec::new(),
            };
            let new_keys = match &new {
                Some(document) => keys(&document.text, self.policy.max_unique_trigrams_per_file)?,
                None => Vec::new(),
            };
            staged_bytes += new.as_ref().map_or(0, |d| d.text.len());
            staged_postings += new_keys.len();
            staged_work_bytes = staged_work_bytes
                .saturating_add(new.as_ref().map_or(0, |d| {
                    d.text.capacity()
                        + d.starts.capacity() * std::mem::size_of::<u32>()
                        + d.path.len()
                        + std::mem::size_of::<Document>()
                }))
                .saturating_add(
                    (old_keys.capacity() + new_keys.capacity()) * std::mem::size_of::<u32>(),
                );
            if staged_bytes > self.policy.max_staged_bytes || staged_postings > MAX_POSTINGS {
                return Err(invalid(
                    "staged update exceeds memory budget; previous generation retained",
                )
                .into());
            }
            let fixed_peak = current_owned.saturating_add(staged_work_bytes);
            if fixed_peak > self.max_owned_bytes {
                return Err(invalid(
                    "staged update exceeds resident admission peak; previous generation retained",
                )
                .into());
            }
            let max_delta_entries = self
                .max_owned_bytes
                .saturating_sub(fixed_peak)
                .checked_div(64)
                .unwrap_or(0)
                .min(self.policy.max_unique_trigrams.saturating_mul(2));
            accumulate_differences(&mut key_delta, &old_keys, &new_keys, max_delta_entries)?;
            let transaction_peak = current_owned
                .saturating_add(staged_work_bytes)
                .saturating_add(key_delta.len().saturating_mul(64));
            if transaction_peak > self.max_owned_bytes {
                return Err(invalid(
                    "staged update exceeds resident admission peak; previous generation retained",
                )
                .into());
            }
            source_bytes = source_bytes - old.map_or(0, |d| d.text.len())
                + new.as_ref().map_or(0, |d| d.text.len());
            line_count = line_count - old.map_or(0, |d| d.starts.len())
                + new.as_ref().map_or(0, |d| d.starts.len());
            if line_count > self.policy.max_total_lines {
                return Err(invalid("incremental line count exceeds configured limit").into());
            }
            posting_count = posting_count - old_keys.len() + new_keys.len();
            replacements.push((path, new, old_keys, new_keys));
            if replacements.len() > self.policy.max_changed_files {
                return Err(
                    invalid("changed file count exceeds configured transaction limit").into(),
                );
            }
        }
        if source_bytes > self.policy.max_source_bytes || posting_count > MAX_POSTINGS {
            return Err(invalid(
                "incremental update exceeds source/posting budget; previous generation retained",
            )
            .into());
        }
        let mut unique_trigrams = self.postings.as_ref().map_or(0, HashMap::len);
        if let Some(postings) = &self.postings {
            for (key, delta) in &key_delta {
                let before = postings.get(key).map_or(0_i64, |ids| ids.len() as i64);
                let after = before + delta;
                if before == 0 && after > 0 {
                    unique_trigrams += 1;
                } else if before > 0 && after <= 0 {
                    unique_trigrams = unique_trigrams.saturating_sub(1);
                }
            }
        }
        if unique_trigrams > self.policy.max_unique_trigrams {
            return Err(invalid("incremental update exceeds max_unique_trigrams").into());
        }
        let projected_path_bytes = current.keys().map(String::len).sum::<usize>();
        let projected_owned = source_bytes
            .saturating_add(projected_path_bytes.saturating_mul(2))
            .saturating_add(line_count.saturating_mul(std::mem::size_of::<u32>()))
            .saturating_add(posting_count.saturating_mul(std::mem::size_of::<u32>()))
            .saturating_add(unique_trigrams.saturating_mul(64))
            .saturating_add(current.len().saturating_mul(128));
        if projected_owned > self.max_owned_bytes {
            return Err(invalid("incremental update exceeds resident admission budget").into());
        }
        let generation = self
            .stats
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("generation overflow"))?;
        let changed_count = replacements.len();
        if changed_count == 0 {
            return Ok(
                serde_json::json!({"changed_files":0,"update_ms":started.elapsed().as_secs_f64()*1000.0,
                "generation":self.stats.generation,"verified_all_source":verify_all,
                "source_files_read":source_files_read,"source_bytes_read":source_bytes_read}),
            );
        }
        // All fallible reads, parsing and budget checks precede the index mutation.
        for (path, new, old_keys, new_keys) in replacements {
            let old_id = self.paths.remove(path.as_str());
            if let Some(id) = old_id {
                if let Some(index) = &mut self.postings {
                    for key in old_keys {
                        if let Some(list) = index.get_mut(&key) {
                            if let Ok(position) = list.binary_search(&(id as u32)) {
                                list.remove(position);
                            }
                            if list.is_empty() {
                                index.remove(&key);
                            }
                        }
                    }
                }
                self.documents[id] = Document {
                    path: Arc::from(""),
                    text: String::new(),
                    starts: Vec::new(),
                    sha256: [0; 32],
                };
            }
            if let Some(new) = new {
                let id = old_id
                    .or_else(|| self.documents.iter().position(|d| d.path.is_empty()))
                    .unwrap_or(self.documents.len());
                if let Some(index) = &mut self.postings {
                    for key in new_keys {
                        let list = index.entry(key).or_default();
                        let position = list.binary_search(&(id as u32)).unwrap_or_else(|i| i);
                        list.insert(position, id as u32);
                    }
                }
                self.paths.insert(Arc::from(path), id);
                if id == self.documents.len() {
                    self.documents.push(new);
                } else {
                    self.documents[id] = new;
                }
            }
        }
        self.stats.source_bytes = source_bytes;
        self.stats.postings = posting_count;
        self.stats.files = self.paths.len();
        self.stats.line_count = line_count;
        self.stats.line_offset_bytes = line_count * std::mem::size_of::<u32>();
        self.stats.path_bytes = self.paths.keys().map(|path| path.len()).sum();
        self.stats.document_bytes = source_bytes;
        self.stats.unique_trigrams = self.postings.as_ref().map_or(0, |p| p.len());
        self.stats.posting_bytes_estimate = self.posting_bytes_estimate();
        if changed_count > 0 {
            self.stats.generation = generation;
            // A load-time corpus hash is not a hash of this updated generation.
            self.stats.source_sha256.clear();
        }
        self.stats.estimated_owned_bytes = self.estimated_bytes();
        Ok(
            serde_json::json!({"changed_files":changed_count,"update_ms":started.elapsed().as_secs_f64()*1000.0,
            "generation":self.stats.generation,"verified_all_source":verify_all,
            "source_files_read":source_files_read,"source_bytes_read":source_bytes_read}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigram_delta_tracks_only_set_differences() {
        let mut deltas = HashMap::new();
        accumulate_differences(&mut deltas, &[1, 2, 3, 4], &[1, 2, 4, 5], 2).unwrap();
        assert_eq!(deltas, HashMap::from([(3, -1), (5, 1)]));
    }

    #[test]
    fn trigram_delta_checks_budget_before_growth() {
        let mut deltas = HashMap::new();
        let error = accumulate_differences(&mut deltas, &[], &[1, 2], 1)
            .err()
            .unwrap();
        assert!(error.to_string().contains("admission peak"));
        assert_eq!(deltas.len(), 1);
    }
}
