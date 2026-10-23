use serde::{Deserialize, Serialize};

pub const GLOBAL_RESIDENT_TARGET: usize = 3 * 1024 * 1024 * 1024;
pub const MAX_RESIDENT_ROOTS: usize = 8;
pub const WORKER_STACK_BYTES: usize = 512 * 1024;
pub const IO_WORKER_STACK_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryPolicy {
    pub max_files: usize,
    pub max_source_bytes: usize,
    pub max_file_bytes: usize,
    pub max_total_lines: usize,
    pub max_total_path_bytes: usize,
    pub max_unique_trigrams_per_file: usize,
    pub max_unique_trigrams: usize,
    pub max_changed_files: usize,
    pub max_staged_bytes: usize,
    pub max_watch_dirs: usize,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            max_files: 100_000,
            max_source_bytes: 256 * 1024 * 1024,
            max_file_bytes: 1024 * 1024,
            max_total_lines: 10_000_000,
            max_total_path_bytes: 16 * 1024 * 1024,
            max_unique_trigrams_per_file: 250_000,
            max_unique_trigrams: 2_000_000,
            max_changed_files: 10_000,
            max_staged_bytes: 128 * 1024 * 1024,
            max_watch_dirs: 50_000,
        }
    }
}

impl MemoryPolicy {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.max_files == 0
            || self.max_source_bytes == 0
            || self.max_file_bytes == 0
            || self.max_total_lines == 0
            || self.max_total_path_bytes == 0
            || self.max_unique_trigrams_per_file == 0
            || self.max_unique_trigrams == 0
            || self.max_changed_files == 0
            || self.max_staged_bytes == 0
            || self.max_watch_dirs == 0
        {
            return Err("all memory policy limits must be positive");
        }
        if self.max_file_bytes > self.max_source_bytes {
            return Err("max_file_bytes cannot exceed max_source_bytes");
        }
        if self.max_staged_bytes > self.max_source_bytes {
            return Err("max_staged_bytes cannot exceed max_source_bytes");
        }
        Ok(())
    }
}
