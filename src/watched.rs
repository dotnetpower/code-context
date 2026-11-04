use crate::catalog::Spec;
use crate::context::{ReadRequest, Snippet, assemble_trusted};
use crate::exclusion_policy;
use crate::incremental::{Stamp, stamp, stamps, validate_stamps};
use crate::policy::WORKER_STACK_BYTES;
use crate::query::{Query, QueryCache, search_fast_arc, search_profiled, search_profiled_mode};
use crate::resident::Resident;
use crate::{Result, invalid};
use globset::{Glob, GlobSetBuilder};
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, TrySendError, sync_channel},
};
use std::time::{Duration, Instant};

#[cfg(test)]
mod io_tests {
    #[test]
    fn unchanged_metadata_result_preserves_stats_without_running_an_update() {
        let (_root,mut watched)=fixture();
        let before=serde_json::to_value(&watched.resident.stats).unwrap();
        watched.maintenance_tx.try_send((watched.mutation_epoch,Ok(watched.stamps.clone()))).unwrap();
        watched.apply_maintenance().unwrap();
        assert_eq!(watched.io_profile.maintenance_noop_applies,1);
        assert_eq!(serde_json::to_value(&watched.resident.stats).unwrap(),before);
        assert_eq!(watched.last_update["changed_files"],0);
        assert_eq!(watched.last_update["source_files_read"],0);
        assert_eq!(watched.last_update["verified_all_source"],false);
    }

    use super::*;

    #[test]
    fn busy_roots_earn_a_longer_idle_lease_and_idle_roots_do_not() {
        assert_eq!(lease_multiplier(0), 1);
        assert_eq!(lease_multiplier(49), 1);
        assert_eq!(lease_multiplier(50), 2);
        assert_eq!(lease_multiplier(199), 2);
        assert_eq!(lease_multiplier(200), 4);
        let (_root, mut watched) = fixture();
        assert!(!watched.idle_for(Duration::from_secs(60)));
        watched.last_used = Instant::now() - Duration::from_secs(90);
        assert!(watched.idle_for(Duration::from_secs(60)));
        watched.source_lookups = 50;
        assert!(!watched.idle_for(Duration::from_secs(60)));
    }

    fn fixture() -> (tempfile::TempDir, Watched) {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "before\n").unwrap();
        let spec = Spec {
            root: root.path().into(),
            scopes: vec![".".into()],
            globs: vec!["*.rs".into()],
            excludes: vec![],
            default_excludes: true,
            prewarm: false,
            policy: Default::default(),
            allow_worktrees: false,
            managed_parent: None,
        };
        let mut watched = Watched::new(spec).unwrap();
        for directory in watched.watched_dirs.clone() {
            watched
                .watcher
                .unwatch(&root.path().join(directory))
                .unwrap();
        }
        (root, watched)
    }

    #[test]
    fn missed_membership_is_recovered_by_bounded_metadata_safety_scan() {
        let (root, mut watched) = fixture();
        let before = watched.io_profile.full_inventory_scans;
        watched.schedule_maintenance();
        assert_eq!(watched.io_profile.full_inventory_scans, before);
        std::fs::write(root.path().join("missed.rs"), "missed_marker\n").unwrap();
        watched.last_reconcile = Instant::now() - RECONCILE_INTERVAL - Duration::from_secs(1);
        watched.schedule_maintenance();
        let started = Instant::now();
        loop {
            let result = watched
                .call("code_grep", json!({"pattern":"missed_marker"}))
                .unwrap();
            if result["total_matches"] == 1 {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(watched.io_profile.full_inventory_scans, before + 1);
        assert_eq!(watched.io_profile.full_source_verifications, 0);
        assert_eq!(watched.io_profile.source_files_read, 1);
    }

    #[test]
    fn live_hash_miss_refreshes_requested_file_without_full_source_verification() {
        let (root, mut watched) = fixture();
        std::fs::write(root.path().join("a.rs"), "after\n").unwrap();
        let before = watched.io_profile.full_inventory_scans;
        let result = watched
            .call(
                "code_read",
                json!({"ranges":[{"path":"a.rs","start":1,"end":1}]}),
            )
            .unwrap();
        assert_eq!(result[0]["lines"], json!(["after"]));
        assert_eq!(watched.io_profile.full_inventory_scans, before);
        assert_eq!(watched.io_profile.full_source_verifications, 0);
        assert_eq!(watched.io_profile.source_files_read, 1);
        assert_eq!(watched.transparent_read_recoveries, 1);
    }

    #[test]
    fn stale_cached_search_and_context_fail_with_live_disk_guidance() {
        for operation in ["code_grep", "code_context"] {
            let (root, mut watched) = fixture();
            let query = json!({"pattern":"before"});
            let first = watched.call(operation, query.clone()).unwrap();
            assert_eq!(first["freshness"]["basis"], "watcher_snapshot");
            // Watchers are disabled in this fixture to reproduce delayed delivery.
            std::fs::write(root.path().join("a.rs"), "changed after cache\n").unwrap();
            let before = watched.io_profile.full_source_verifications;
            let error = watched.call(operation, query).unwrap_err().to_string();
            assert!(error.contains("STALE_INDEX"), "{operation}: {error}");
            assert!(error.contains("path=a.rs"), "{error}");
            assert!(error.contains(root.path().to_str().unwrap()), "{error}");
            assert!(error.contains("live disk read"), "{error}");
            assert_eq!(watched.io_profile.full_source_verifications, before);
            let read = watched
                .call(
                    "code_read",
                    json!({"ranges":[{"path":"a.rs","start":1,"end":1}]}),
                )
                .unwrap();
            assert_eq!(read[0]["lines"], json!(["changed after cache"]));
            let recovered = watched
                .call(operation, json!({"pattern":"changed after cache"}))
                .unwrap();
            assert_eq!(recovered["freshness"]["full_disk_verified"], false);
        }
    }

    #[test]
    fn zero_match_on_a_known_changed_file_is_not_a_silent_absence_claim() {
        for operation in ["code_grep", "code_context"] {
            let (root, mut watched) = fixture();
            std::fs::write(root.path().join("a.rs"), "new_marker\n").unwrap();
            let error = watched
                .call(
                    operation,
                    json!({
                        "pattern":"new_marker","paths":["a.rs"],
                    }),
                )
                .unwrap_err()
                .to_string();
            assert!(error.contains("STALE_INDEX"), "{operation}: {error}");
            let error = watched
                .call(
                    operation,
                    json!({
                        "pattern":"new_marker","glob":"a.rs",
                    }),
                )
                .unwrap_err()
                .to_string();
            assert!(error.contains("STALE_INDEX"), "{operation}: {error}");
            assert_eq!(watched.io_profile.full_source_verifications, 0);
        }
    }

    #[test]
    fn unscoped_zero_result_admits_that_new_content_is_not_completely_verified() {
        let (root, mut watched) = fixture();
        std::fs::write(root.path().join("new.rs"), "not_yet_indexed\n").unwrap();
        for operation in ["code_grep", "code_context"] {
            let result = watched
                .call(operation, json!({"pattern":"not_yet_indexed"}))
                .unwrap();
            assert_eq!(result["freshness"]["full_disk_verified"], false);
            assert!(
                result["freshness"]["notice"]
                    .as_str()
                    .unwrap()
                    .contains("code_sync")
            );
            let error = watched
                .call(
                    operation,
                    json!({
                        "pattern":"not_yet_indexed","paths":["new.rs"],
                    }),
                )
                .unwrap_err()
                .to_string();
            assert!(error.contains("TARGET_NOT_INDEXED"), "{error}");
        }
        let files = watched
            .call("code_files", json!({"glob":"new.rs"}))
            .unwrap();
        assert_eq!(files["total_matches"], 0);
        assert_eq!(files["freshness"]["full_disk_verified"], false);
    }

    #[test]
    fn deleted_hit_cannot_be_returned_from_a_cached_snapshot() {
        let (root, mut watched) = fixture();
        watched
            .call("code_context", json!({"pattern":"before"}))
            .unwrap();
        std::fs::remove_file(root.path().join("a.rs")).unwrap();
        for operation in ["code_grep", "code_context", "code_files"] {
            let args = if operation == "code_files" {
                json!({"glob":"a.rs"})
            } else {
                json!({"pattern":"before"})
            };
            let error = watched.call(operation, args).unwrap_err().to_string();
            assert!(error.contains("STALE_INDEX"), "{operation}: {error}");
        }
    }

    #[test]
    fn cached_context_checks_bytes_even_if_length_and_mtime_are_preserved() {
        let (root, mut watched) = fixture();
        let query = json!({"pattern":"before"});
        watched.call("code_context", query.clone()).unwrap();
        let path = root.path().join("a.rs");
        let modified = path.metadata().unwrap().modified().unwrap();
        std::fs::write(&path, "after!\n").unwrap();
        std::fs::File::open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let error = watched.call("code_context", query).unwrap_err().to_string();
        assert!(
            error.contains("STALE_INDEX") && error.contains("path=a.rs"),
            "{error}"
        );
    }

    #[test]
    fn overflow_still_performs_full_verification() {
        let (root, mut watched) = fixture();
        std::fs::write(root.path().join("a.rs"), "overflow_marker\n").unwrap();
        watched.overflow.store(true, Ordering::Release);
        let result = watched
            .call("code_grep", json!({"pattern":"overflow_marker"}))
            .unwrap();
        assert_eq!(result["total_matches"], 1);
        assert_eq!(watched.io_profile.full_source_verifications, 1);
        assert_eq!(watched.io_profile.source_files_read, 1);
    }

    #[test]
    fn grown_file_is_refreshed_before_rejecting_a_snapshot_eof_range() {
        let (root, mut watched) = fixture();
        std::fs::write(root.path().join("a.rs"), "before\nnew second line\n").unwrap();
        let result = watched
            .call(
                "code_read",
                json!({
                    "ranges":[{"path":"a.rs","start":2,"end":2}]
                }),
            )
            .unwrap();
        assert_eq!(result[0]["lines"], json!(["new second line"]));
        assert_eq!(watched.io_profile.full_source_verifications, 0);
        assert_eq!(watched.io_profile.source_files_read, 1);
    }

    #[test]
    fn invalid_read_range_names_root_path_requested_bounds_and_actual_line_count() {
        let (root, mut watched) = fixture();
        let error = watched
            .call(
                "code_read",
                json!({
                    "ranges":[{"path":"a.rs","start":720,"end":765}]
                }),
            )
            .unwrap_err()
            .to_string();
        for expected in [
            "READ_RANGE_OUT_OF_BOUNDS",
            "path=a.rs",
            "requested=720..765",
            "indexed_lines=1",
            "Do not repeat",
        ] {
            assert!(error.contains(expected), "{error}");
        }
        assert!(error.contains(root.path().to_str().unwrap()), "{error}");
        assert_eq!(watched.io_profile.full_source_verifications, 0);
    }

    #[test]
    fn missing_batch_member_is_reported_without_mislabeling_valid_members() {
        let (root, mut watched) = fixture();
        let error = watched
            .call(
                "code_read",
                json!({"ranges":[
                    {"path":"a.rs","start":1,"end":1},
                    {"path":"missing.rs","start":1,"end":1}
                ]}),
            )
            .unwrap_err()
            .to_string();
        for expected in [
            "READ_UNAVAILABLE",
            "path=missing.rs",
            "reason=missing_on_disk",
            "Batch failed atomically",
            "no partial success",
        ] {
            assert!(error.contains(expected), "{error}");
        }
        assert!(error.contains(root.path().to_str().unwrap()), "{error}");
        assert!(!error.contains("path=a.rs reason=unstable"), "{error}");
        assert_eq!(watched.io_profile.full_source_verifications, 0);
    }

    #[test]
    fn deleted_directories_and_deleted_scopes_are_unwatched_without_blocking() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/nested/deep")).unwrap();
        std::fs::create_dir_all(root.path().join("tests")).unwrap();
        std::fs::write(root.path().join("src/a.rs"), "keep\n").unwrap();
        std::fs::write(root.path().join("src/nested/deep/b.rs"), "remove_me\n").unwrap();
        let mut watched = Watched::new(Spec {
            root: root.path().into(),
            scopes: vec!["src".into(), "tests".into()],
            globs: vec!["*.rs".into()],
            excludes: vec![],
            default_excludes: true,
            prewarm: false,
            policy: Default::default(),
            allow_worktrees: false,
            managed_parent: None,
        })
        .unwrap();
        assert!(watched.watched_dirs.contains(Path::new("src/nested/deep")));
        assert!(watched.watched_dirs.contains(Path::new("tests")));

        // Delete the nested directory tree and recreate an empty directory at the same path.
        std::fs::remove_dir_all(root.path().join("src/nested")).unwrap();
        std::fs::create_dir_all(root.path().join("src/nested")).unwrap();

        let started = Instant::now();
        loop {
            watched.maintenance_tick().unwrap();
            if !watched.watched_dirs.contains(Path::new("src/nested/deep"))
                && !watched.watched_dirs.contains(Path::new("src/nested"))
            {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(watched.blocked.is_none());
        let grep = watched
            .call("code_grep", json!({"pattern":"remove_me"}))
            .unwrap();
        assert_eq!(grep["total_matches"], 0);

        // Deleting a configured scope directory un-watches it immediately even though sync errors
        // until the scope directory is recreated, then re-watches the recreated directory.
        std::fs::remove_dir_all(root.path().join("tests")).unwrap();
        assert!(watched.call("code_sync", json!({})).is_err());
        assert!(!watched.watched_dirs.contains(Path::new("tests")));
        std::fs::create_dir_all(root.path().join("tests")).unwrap();
        watched.call("code_sync", json!({})).unwrap();
        assert!(watched.watched_dirs.contains(Path::new("tests")));
    }
}

const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);
const READ_RECOVERY_DELAY: Duration = Duration::from_millis(8);
pub(crate) const IDLE_ROOT_TIMEOUT: Duration = Duration::from_secs(900);

/// Roots with sustained source lookups keep their cache longer, avoiding avoidable CACHE_MISS reloads.
pub(crate) fn lease_multiplier(lookups: u64) -> u32 {
    match lookups {
        0..50 => 1,
        50..200 => 2,
        _ => 4,
    }
}
const ROOT_RUNTIME_HEADROOM: usize = 160 * 1024 * 1024;
type MaintenanceResult = (u64, Result<BTreeMap<String, Stamp>>);

#[derive(Default, serde::Serialize)]
struct IoProfile {
    full_inventory_scans: u64,
    directory_inventory_scans: u64,
    full_source_verifications: u64,
    source_files_read: u64,
    source_bytes_read: u64,
    read_policy_rejections: u64,
    read_policy_compilations: u64,
    read_policy_cache_hits: u64,
    maintenance_noop_applies: u64,
}

pub struct Watched {
    pub spec: Spec,
    resident: Resident,
    watcher: RecommendedWatcher,
    watched_dirs: BTreeSet<std::path::PathBuf>,
    watched_dir_bytes: usize,
    events: Receiver<notify::Result<Event>>,
    overflow: Arc<AtomicBool>,
    stamps: BTreeMap<String, Stamp>,
    last_reconcile: Instant,
    last_used: Instant,
    source_lookups: u64,
    blocked: Option<String>,
    last_update: Value,
    query_cache: QueryCache,
    last_profile: Value,
    maintenance_tx: std::sync::mpsc::SyncSender<MaintenanceResult>,
    maintenance_rx: Receiver<MaintenanceResult>,
    maintenance_active: Arc<AtomicBool>,
    mutation_epoch: u64,
    transparent_read_recoveries: u64,
    transparent_read_recovery_failures: u64,
    io_profile: IoProfile,
    cached_workspace: Option<(Spec, crate::workspace::Workspace)>,
    watcher_policy: Option<(Spec, crate::workspace::CandidatePolicy)>,
    read_policy: Option<(
        Spec,
        crate::workspace::Workspace,
        crate::workspace::CandidatePolicy,
    )>,
}

impl Watched {
    pub(crate) fn budget_for_resident(required_bytes: usize) -> usize {
        required_bytes.saturating_add(ROOT_RUNTIME_HEADROOM)
    }

    pub fn new(spec: Spec) -> Result<Self> {
        Self::new_with_budget(spec, usize::MAX)
    }

    pub fn new_with_budget(spec: Spec, max_owned_bytes: usize) -> Result<Self> {
        let workspace = spec.workspace()?;
        let mut inventory = workspace.files_limited(&spec.policy)?;
        let stamps = std::mem::take(&mut inventory.stamps);
        let watched_dirs = workspace.watch_directories_for(
            inventory.files.iter().map(std::path::PathBuf::as_path),
            &spec.policy,
        )?;
        let (tx, events) = sync_channel(4096);
        let (maintenance_tx, maintenance_rx) = sync_channel(1);
        let overflow = Arc::new(AtomicBool::new(false));
        let flag = overflow.clone();
        let mut filter = GlobSetBuilder::new();
        for pattern in &spec.globs {
            filter.add(Glob::new(pattern)?);
        }
        let filter = filter.build()?;
        let mut exclude_filter = GlobSetBuilder::new();
        for pattern in workspace.effective_excludes() {
            exclude_filter.add(Glob::new(&pattern)?);
        }
        let exclude_filter = exclude_filter.build()?;
        let root = workspace.root.clone();
        let scopes_for_watcher = spec.scopes.clone();
        let mut watcher = RecommendedWatcher::new(
            move |event: notify::Result<Event>| {
                // Do not let our own source reads fill the invalidation queue.
                if event.as_ref().is_ok_and(|event| {
                    matches!(
                        event.kind,
                        notify::EventKind::Access(_)
                            | notify::EventKind::Modify(notify::event::ModifyKind::Metadata(
                                notify::event::MetadataKind::AccessTime
                            ))
                    )
                }) {
                    return;
                }
                if let Ok(event) = &event
                    && !event.need_rescan()
                    && !event.paths.is_empty()
                    && !matches!(
                        event.kind,
                        notify::EventKind::Remove(
                            notify::event::RemoveKind::Folder
                                | notify::event::RemoveKind::Any
                                | notify::event::RemoveKind::Other
                        ) | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                    )
                    && !event.paths.iter().any(|path| {
                        path.is_dir()
                            || path
                                .file_name()
                                .is_some_and(|name| name == ".ignore" || name == ".gitignore")
                            || path.strip_prefix(&root).is_ok_and(|relative| {
                                if exclusion_policy::is_profile_marker_for_scopes(
                                    relative,
                                    &scopes_for_watcher,
                                ) {
                                    return true;
                                }
                                let is_excluded = relative
                                    .components()
                                    .any(|c| c.as_os_str().as_encoded_bytes().starts_with(b"."))
                                    || relative
                                        .ancestors()
                                        .filter(|a| !a.as_os_str().is_empty())
                                        .any(|a| {
                                            exclude_filter.is_match(a)
                                                || a.file_name()
                                                    .is_some_and(|n| exclude_filter.is_match(n))
                                        });
                                if is_excluded {
                                    return false;
                                }
                                filter.is_match(relative)
                                    || relative
                                        .file_name()
                                        .is_some_and(|name| filter.is_match(name))
                            })
                    })
                {
                    return;
                }
                if let Err(error) = tx.try_send(event) {
                    match error {
                        TrySendError::Full(_) => {
                            flag.store(true, Ordering::Release);
                        }
                        TrySendError::Disconnected(_) => {}
                    }
                }
            },
            Config::default(),
        )?;
        let mut active_watched_dirs = BTreeSet::new();
        for directory in watched_dirs {
            let absolute = workspace.root.join(&directory);
            match watcher.watch(&absolute, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    active_watched_dirs.insert(directory);
                }
                Err(error) if Self::is_missing_watch_target_error(&error, &absolute) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let watched_dirs = active_watched_dirs;
        // Install the inventory-derived watch plan before loading source bytes.
        let resident = Resident::load_with_inventory(
            &workspace,
            true,
            1,
            spec.policy.clone(),
            max_owned_bytes.saturating_sub(ROOT_RUNTIME_HEADROOM),
            inventory,
        )?;
        let watcher_policy = crate::workspace::CandidatePolicy::compile(workspace.candidate_policy_key())
            .ok()
            .map(|policy| (spec.clone(), policy));
        let cached_workspace = Some((spec.clone(), workspace.clone()));
        let watched_dir_bytes = watched_dirs
            .iter()
            .map(|path| path.as_os_str().len() + 64)
            .sum::<usize>();
        let mut watched = Self {
            spec,
            resident,
            watcher,
            watched_dirs,
            watched_dir_bytes,
            events,
            overflow,
            stamps,
            last_reconcile: Instant::now(),
            last_used: Instant::now(),
            source_lookups: 0,
            blocked: None,
            last_update: json!({}),
            query_cache: QueryCache::default(),
            last_profile: json!({}),
            maintenance_tx,
            maintenance_rx,
            maintenance_active: Arc::new(AtomicBool::new(false)),
            mutation_epoch: 0,
            transparent_read_recoveries: 0,
            transparent_read_recovery_failures: 0,
            io_profile: IoProfile {
                full_inventory_scans: 1,
                ..IoProfile::default()
            },
            cached_workspace,
            watcher_policy,
            read_policy: None,
        };
        let overflowed = watched.overflow.load(Ordering::Acquire);
        watched.synchronize(overflowed)?;
        Ok(watched)
    }

    pub(crate) fn idle_for(&self, max_age: Duration) -> bool {
        self.last_used.elapsed() > max_age.saturating_mul(lease_multiplier(self.source_lookups))
    }

    pub fn idle_age(&self) -> Duration {
        self.last_used.elapsed()
    }

    pub(crate) fn restrict_admission(&mut self, total_budget: usize) -> usize {
        let previous = self.resident.max_owned_bytes;
        self.resident.max_owned_bytes =
            previous.min(total_budget.saturating_sub(ROOT_RUNTIME_HEADROOM));
        previous
    }

    pub(crate) fn restore_admission(&mut self, previous: usize) {
        self.resident.max_owned_bytes = previous;
    }

    pub fn estimated_bytes(&self) -> usize {
        let stamp_bytes = self
            .resident
            .stats
            .path_bytes
            .saturating_add(self.stamps.len().saturating_mul(std::mem::size_of::<Stamp>() + 64));
        self.resident.stats.estimated_owned_bytes.saturating_add(
            ROOT_RUNTIME_HEADROOM.max(
                stamp_bytes
                    .saturating_add(self.watched_dir_bytes)
                    .saturating_add(self.query_cache.estimated_bytes()),
            ),
        )
    }

    pub fn release_caches(&mut self) {
        self.query_cache.clear_memory();
        self.cached_workspace = None;
        self.watcher_policy = None;
        self.read_policy = None;
    }

    fn is_watcher_candidate(&mut self, name: &str) -> Result<bool> {
        if self
            .watcher_policy
            .as_ref()
            .is_none_or(|(saved_spec, _)| saved_spec != &self.spec)
        {
            let ws = self.workspace()?.into_owned();
            let policy = crate::workspace::CandidatePolicy::compile(ws.candidate_policy_key())?;
            self.cached_workspace = Some((self.spec.clone(), ws));
            self.watcher_policy = Some((self.spec.clone(), policy));
        }
        let (_, policy) = self.watcher_policy.as_ref().expect("watcher policy");
        Ok(policy.candidate_path(name).unwrap_or(false))
    }

    fn profile_exclusions_changed(&mut self) -> Result<bool> {
        let ws = self.workspace()?.into_owned();
        let current_excludes = ws.effective_excludes();
        let changed = self
            .watcher_policy
            .as_ref()
            .is_none_or(|(saved_spec, policy)| {
                saved_spec != &self.spec || policy.key.excludes != current_excludes
            });
        if changed {
            let policy = crate::workspace::CandidatePolicy::compile(ws.candidate_policy_key())?;
            self.cached_workspace = Some((self.spec.clone(), ws));
            self.watcher_policy = Some((self.spec.clone(), policy));
            self.read_policy = None;
        }
        Ok(changed)
    }

    fn workspace(&self) -> Result<std::borrow::Cow<'_, crate::workspace::Workspace>> {
        if let Some((saved_spec, ws)) = &self.cached_workspace
            && saved_spec == &self.spec
        {
            return Ok(std::borrow::Cow::Borrowed(ws));
        }
        if let Some((saved_spec, ws, _)) = &self.read_policy
            && saved_spec == &self.spec
        {
            return Ok(std::borrow::Cow::Borrowed(ws));
        }
        Ok(std::borrow::Cow::Owned(self.spec.workspace()?))
    }

    fn is_missing_watch_target_error(error: &notify::Error, absolute: &Path) -> bool {
        !absolute.is_dir()
            || matches!(
                error.kind,
                notify::ErrorKind::PathNotFound | notify::ErrorKind::WatchNotFound
            )
            || matches!(
                &error.kind,
                notify::ErrorKind::Io(io_err) if io_err.kind() == std::io::ErrorKind::NotFound
            )
    }

    fn record_affected_directory(
        root: &Path,
        directories: &mut BTreeMap<PathBuf, bool>,
        parent: &Path,
    ) {
        let mut current = parent;
        let mut missing_ancestor: Option<&Path> = None;
        while !current.as_os_str().is_empty() {
            if !root.join(current).is_dir() {
                missing_ancestor = Some(current);
                current = current.parent().unwrap_or(Path::new(""));
            } else {
                break;
            }
        }
        if let Some(missing) = missing_ancestor {
            directories.insert(missing.to_path_buf(), true);
        } else {
            directories.entry(parent.to_path_buf()).or_insert(false);
        }
    }

    fn unwatch_prefix(&mut self, prefix: &Path) {
        if prefix.as_os_str().is_empty() {
            return;
        }
        let matching: Vec<PathBuf> = self
            .watched_dirs
            .iter()
            .filter(|dir| !dir.as_os_str().is_empty() && dir.starts_with(prefix))
            .cloned()
            .collect();
        if matching.is_empty() {
            return;
        }
        for directory in matching {
            let _ = self.watcher.unwatch(&self.spec.root.join(&directory));
            self.watched_dirs.remove(&directory);
        }
        self.watched_dir_bytes = self
            .watched_dirs
            .iter()
            .map(|path| path.as_os_str().len() + 64)
            .sum::<usize>();
    }

    fn prune_deleted_watches(&mut self) {
        let missing: Vec<PathBuf> = self
            .watched_dirs
            .iter()
            .filter(|dir| !self.spec.root.join(dir).is_dir())
            .cloned()
            .collect();
        if missing.is_empty() {
            return;
        }
        for directory in missing {
            let _ = self.watcher.unwatch(&self.spec.root.join(&directory));
            self.watched_dirs.remove(&directory);
        }
        self.watched_dir_bytes = self
            .watched_dirs
            .iter()
            .map(|path| path.as_os_str().len() + 64)
            .sum::<usize>();
    }

    fn needs_watch_refresh(&self, workspace: &crate::workspace::Workspace) -> bool {
        self.watched_dirs
            .iter()
            .any(|dir| !workspace.root.join(dir).is_dir())
            || workspace.scopes.iter().any(|scope| {
                let scope_path = if scope == "." {
                    Path::new("")
                } else {
                    Path::new(scope)
                };
                !self.watched_dirs.contains(scope_path) && workspace.root.join(scope_path).is_dir()
            })
    }

    fn refresh_watch_directories<'a>(
        &mut self,
        workspace: &crate::workspace::Workspace,
        paths: impl IntoIterator<Item = &'a std::path::Path>,
    ) -> Result<()> {
        let mut desired = workspace.watch_directories_for(paths, &self.spec.policy)?;
        desired.retain(|directory| workspace.root.join(directory).is_dir());
        let mut added = Vec::new();
        let mut vanished = Vec::new();
        for directory in desired.difference(&self.watched_dirs) {
            let absolute = workspace.root.join(directory);
            match self.watcher.watch(&absolute, RecursiveMode::NonRecursive) {
                Ok(()) => added.push(directory.clone()),
                Err(error) if Self::is_missing_watch_target_error(&error, &absolute) => {
                    vanished.push(directory.clone());
                }
                Err(error) => {
                    for added in added {
                        let _ = self.watcher.unwatch(&workspace.root.join(added));
                    }
                    return Err(error.into());
                }
            }
        }
        for directory in vanished {
            desired.remove(&directory);
        }
        let mut removed = Vec::new();
        for directory in self.watched_dirs.difference(&desired) {
            let absolute = workspace.root.join(directory);
            if let Err(error) = self.watcher.unwatch(&absolute)
                && !Self::is_missing_watch_target_error(&error, &absolute)
            {
                for removed in removed {
                    let _ = self
                        .watcher
                        .watch(&workspace.root.join(removed), RecursiveMode::NonRecursive);
                }
                for added in added {
                    let _ = self.watcher.unwatch(&workspace.root.join(added));
                }
                return Err(error.into());
            }
            removed.push(directory.clone());
        }
        self.watched_dir_bytes = desired
            .iter()
            .map(|path| path.as_os_str().len() + 64)
            .sum::<usize>();
        self.watched_dirs = desired;
        Ok(())
    }

    fn synchronize(&mut self, strict: bool) -> Result<()> {
        self.apply_maintenance()?;
        let verify_all =
            strict || self.overflow.swap(false, Ordering::AcqRel) || self.blocked.is_some();
        let mut iter = self.events.try_iter().take(4096).peekable();
        if !verify_all && iter.peek().is_none() {
            return Ok(());
        }
        let drained: Vec<_> = iter.collect();
        let mut changed: BTreeSet<String> = BTreeSet::new();
        let mut directories = BTreeMap::new();
        let mut removed_watch_prefixes = Vec::new();
        let mut reconcile = verify_all || self.blocked.is_some();
        let mut watcher_rescan = false;
        for event in drained {
            let event = event.map_err(|e| invalid(format!("filesystem watcher failed: {e}")))?;
            if matches!(
                event.kind,
                notify::EventKind::Access(_)
                    | notify::EventKind::Modify(notify::event::ModifyKind::Metadata(
                        notify::event::MetadataKind::AccessTime
                    ))
            ) {
                continue;
            }

            if event.need_rescan() {
                reconcile = true;
                watcher_rescan = true;
            }
            if matches!(
                event.kind,
                notify::EventKind::Modify(notify::event::ModifyKind::Name(
                    notify::event::RenameMode::From | notify::event::RenameMode::Any
                ))
            ) && event.paths.iter().any(|absolute| {
                absolute.strip_prefix(&self.spec.root).is_ok_and(|path| {
                    let normalized = crate::workspace::normalize(path);
                    let path = normalized.as_path();
                    self.watched_dirs.contains(path)
                })
            }) {
                reconcile = true;
            }
            for path in event.paths {
                if let Ok(path) = path.strip_prefix(&self.spec.root) {
                    let normalized = crate::workspace::normalize(path);
                    let path = normalized.as_path();
                    let name = path
                        .to_str()
                        .ok_or_else(|| invalid("non-UTF8 event path"))?;
                    if path
                        .file_name()
                        .is_some_and(|p| p == ".gitignore" || p == ".ignore")
                    {
                        reconcile = true;
                        continue;
                    }
                    if exclusion_policy::is_profile_marker_for_scopes(path, &self.spec.scopes)
                        && (!self.stamps.contains_key(name) || !self.spec.root.join(path).is_file())
                        && self.profile_exclusions_changed()?
                    {
                        reconcile = true;
                    }
                    if self.spec.root.join(path).is_dir() || self.watched_dirs.contains(path)
                    {
                        if matches!(
                            event.kind,
                            notify::EventKind::Remove(_)
                                | notify::EventKind::Modify(notify::event::ModifyKind::Name(
                                    notify::event::RenameMode::From
                                        | notify::event::RenameMode::Any
                                ))
                        ) {
                            removed_watch_prefixes.push(path.to_path_buf());
                        }
                        directories.insert(path.to_path_buf(), true);
                    } else if !self.stamps.contains_key(name) {
                        if !self.is_watcher_candidate(name)? {
                            continue;
                        }
                        Self::record_affected_directory(
                            &self.spec.root,
                            &mut directories,
                            path.parent().unwrap_or(Path::new("")),
                        );
                        changed.insert(name.into());
                    } else {
                        if !matches!(
                            event.kind,
                            notify::EventKind::Modify(
                                notify::event::ModifyKind::Data(_)
                                    | notify::event::ModifyKind::Metadata(_)
                            )
                        ) && !self.spec.root.join(path).is_file()
                        {
                            Self::record_affected_directory(
                                &self.spec.root,
                                &mut directories,
                                path.parent().unwrap_or(Path::new("")),
                            );
                        }
                        changed.insert(name.into());
                    }
                } else {
                    reconcile = true;
                }
            }
        }
        if changed.is_empty() && directories.is_empty() && !reconcile {
            return Ok(());
        }
        for prefix in removed_watch_prefixes {
            self.unwatch_prefix(&prefix);
        }
        self.prune_deleted_watches();
        let workspace = self.workspace()?.into_owned();
        if !reconcile && !directories.is_empty() {
            return self.reconcile_directories(&directories, &changed);
        }
        if !reconcile && changed.iter().all(|name| self.stamps.contains_key(name)) {
            let mut previous = Vec::with_capacity(changed.len());
            let mut actual_changed = BTreeSet::new();
            for name in &changed {
                let target_path = workspace.root.join(name);
                let mut current_stamp = stamp(&target_path);
                if current_stamp.as_ref().is_ok_and(|s| s.length == 0)
                    && self.stamps.get(name).is_some_and(|old| old.length > 0)
                {
                    std::thread::sleep(Duration::from_millis(2));
                    current_stamp = stamp(&target_path);
                }
                match current_stamp {
                    Ok(stamp) => {
                        let old = self
                            .stamps
                            .insert(name.clone(), stamp.clone())
                            .expect("known stamp exists");
                        if verify_all || old != stamp {
                            actual_changed.insert(name.clone());
                        }
                        previous.push((name.clone(), old));
                    }
                    Err(error) => {
                        for (name, old) in previous.drain(..) {
                            self.stamps.insert(name, old);
                        }
                        if workspace.root.join(name).exists() {
                            return Err(error);
                        }
                        for candidate in &changed {
                            if !workspace.root.join(candidate).is_file() {
                                Self::record_affected_directory(
                                    &workspace.root,
                                    &mut directories,
                                    Path::new(candidate).parent().unwrap_or(Path::new("")),
                                );
                            }
                        }
                        return self.reconcile_directories(&directories, &changed);
                    }
                }
            }
            if !reconcile {
                if actual_changed.is_empty() {
                    self.blocked = None;
                    return Ok(());
                }
                match self
                    .resident
                    .update(&workspace, &actual_changed, &self.stamps, verify_all)
                {
                    Ok(update) => {
                        self.record_update(update);
                        self.query_cache
                            .retain_generation(self.resident.stats.generation);
                        self.mutation_epoch = self.mutation_epoch.wrapping_add(1);
                        self.blocked = None;
                        return Ok(());
                    }
                    Err(error) => {
                        for (name, old) in previous {
                            self.stamps.insert(name, old);
                        }
                        return Err(error);
                    }
                }
            }
        }
        self.io_profile.full_inventory_scans += 1;
        let mut current = stamps(&workspace, &self.spec.policy)?;
        if let Some((_, cached_policy)) = &self.watcher_policy
            && workspace.candidate_policy_key() != cached_policy.key
        {
            self.watcher_policy = None;
        }
        if let Some((_, _, cached_policy)) = &self.read_policy
            && workspace.candidate_policy_key() != cached_policy.key
        {
            self.read_policy = None;
        }
        let mut need_watch_refresh = self.needs_watch_refresh(&workspace);
        for (path, stamp) in &current {
            match self.stamps.get(path) {
                Some(old) if old == stamp => {}
                Some(_) => {
                    changed.insert(path.clone());
                }
                None => {
                    if let Some(parent) = Path::new(path).parent()
                        && !self.watched_dirs.contains(parent)
                    {
                        need_watch_refresh = true;
                    }
                    changed.insert(path.clone());
                }
            }
        }
        for path in self.stamps.keys() {
            if !current.contains_key(path) {
                need_watch_refresh = true;
                changed.insert(path.clone());
            }
        }
        if need_watch_refresh {
            self.refresh_watch_directories(&workspace, current.keys().map(std::path::Path::new))?;
        }
        // Unchanged files are not reopened except for an explicit strict barrier.
        let update =
            self.resident
                .update(&workspace, &changed, &current, verify_all || watcher_rescan)?;
        current.retain(|path, _| self.resident.paths.contains_key(path.as_str()));
        self.record_update(update);
        self.query_cache
            .retain_generation(self.resident.stats.generation);
        self.stamps = current;
        if !changed.is_empty() {
            self.mutation_epoch = self.mutation_epoch.wrapping_add(1);
        }
        self.last_reconcile = Instant::now();
        self.blocked = None;
        Ok(())
    }

    pub fn maintenance_tick(&mut self) -> Result<()> {
        if self.blocked.is_none() {
            self.synchronize_or_block(false)?;
        } else {
            self.apply_maintenance()?;
        }
        self.schedule_maintenance();
        Ok(())
    }

    fn record_update(&mut self, update: Value) {
        self.io_profile.source_files_read += update["source_files_read"]
            .as_u64()
            .expect("incremental update read count");
        self.io_profile.source_bytes_read += update["source_bytes_read"]
            .as_u64()
            .expect("incremental update read bytes");
        self.io_profile.full_source_verifications +=
            u64::from(update["verified_all_source"] == true);
        self.last_update = update;
    }

    fn reconcile_directories(
        &mut self,
        directories: &BTreeMap<PathBuf, bool>,
        forced: &BTreeSet<String>,
    ) -> Result<()> {
        self.prune_deleted_watches();
        let workspace = self.workspace()?.into_owned();
        let mut current = self.stamps.clone();
        for (directory, recursive) in directories {
            if !directory.as_os_str().is_empty()
                && crate::workspace::safe_relative(directory).is_err()
            {
                continue;
            }
            let directory = crate::workspace::normalize(directory);
            current.retain(|name, _| {
                let path = Path::new(name);
                if *recursive {
                    !path.starts_with(&directory)
                } else {
                    path.parent().unwrap_or(Path::new("")) != directory
                }
            });
            if !workspace.root.join(&directory).is_dir() {
                continue;
            }
            let mut targets = BTreeSet::new();
            for scope in &workspace.scopes {
                let scope_path = if scope == "." {
                    Path::new("")
                } else {
                    Path::new(scope)
                };
                if directory.starts_with(scope_path) {
                    targets.insert(if directory.as_os_str().is_empty() {
                        ".".into()
                    } else {
                        directory.to_string_lossy().into_owned()
                    });
                } else if *recursive && scope_path.starts_with(&directory) {
                    targets.insert(scope.clone());
                }
            }
            if targets.is_empty() {
                continue;
            }
            self.io_profile.directory_inventory_scans += 1;
            let inventory = workspace.files_limited_at(
                &self.spec.policy,
                &targets.into_iter().collect::<Vec<_>>(),
                !recursive,
            )?;
            for (name, metadata) in inventory.stamps {
                current.insert(name, metadata);
            }
        }
        validate_stamps(&current, &self.spec.policy)?;
        if let Some((_, cached_policy)) = &self.watcher_policy
            && workspace.candidate_policy_key() != cached_policy.key
        {
            self.watcher_policy = None;
        }
        if let Some((_, _, cached_policy)) = &self.read_policy
            && workspace.candidate_policy_key() != cached_policy.key
        {
            self.read_policy = None;
        }
        let mut changed = forced.clone();
        let mut need_watch_refresh = self.needs_watch_refresh(&workspace);
        for (name, metadata) in &current {
            match self.stamps.get(name) {
                Some(old) if old == metadata => {}
                Some(_) => {
                    changed.insert(name.clone());
                }
                None => {
                    if let Some(parent) = Path::new(name).parent()
                        && !self.watched_dirs.contains(parent)
                    {
                        need_watch_refresh = true;
                    }
                    changed.insert(name.clone());
                }
            }
        }
        for name in self.stamps.keys() {
            if !current.contains_key(name) {
                need_watch_refresh = true;
                changed.insert(name.clone());
            }
        }
        if need_watch_refresh {
            self.refresh_watch_directories(&workspace, current.keys().map(Path::new))?;
        }
        let update = self
            .resident
            .update(&workspace, &changed, &current, false)?;
        current.retain(|path, _| self.resident.paths.contains_key(path.as_str()));
        self.record_update(update);
        self.query_cache
            .retain_generation(self.resident.stats.generation);
        self.stamps = current;
        if !changed.is_empty() {
            self.mutation_epoch = self.mutation_epoch.wrapping_add(1);
        }
        self.blocked = None;
        Ok(())
    }

    pub fn schedule_maintenance(&mut self) {
        if self.last_reconcile.elapsed() < RECONCILE_INTERVAL
            || self.maintenance_active.swap(true, Ordering::AcqRel)
        {
            return;
        }
        self.last_reconcile = Instant::now();
        self.io_profile.full_inventory_scans += 1;
        let spec = self.spec.clone();
        let cached_ws = self.workspace().ok().map(std::borrow::Cow::into_owned);
        let sender = self.maintenance_tx.clone();
        let active = self.maintenance_active.clone();
        let epoch = self.mutation_epoch;
        if let Err(error) = std::thread::Builder::new()
            .name("cctx-maintenance".into())
            .stack_size(WORKER_STACK_BYTES)
            .spawn(move || {
                let policy = spec.policy.clone();
                let result = match cached_ws {
                    Some(workspace) => Ok(workspace),
                    None => spec.workspace(),
                }
                .and_then(|workspace| stamps(&workspace, &policy));
                let _ = sender.try_send((epoch, result));
                active.store(false, Ordering::Release);
            })
        {
            self.maintenance_active.store(false, Ordering::Release);
            self.blocked = Some(format!("failed to spawn maintenance worker: {error}"));
        }
    }

    fn apply_maintenance(&mut self) -> Result<()> {
        while let Ok((epoch, result)) = self.maintenance_rx.try_recv() {
            let started=Instant::now();
            if epoch != self.mutation_epoch {
                continue;
            }
            let mut current = match result {
                Ok(current) => current,
                Err(error) => {
                    self.prune_deleted_watches();
                    self.blocked = Some(error.to_string());
                    return Err(error);
                }
            };
            self.prune_deleted_watches();
            if current==self.stamps {
                let workspace = self.workspace()?.into_owned();
                if self.needs_watch_refresh(&workspace) {
                    self.refresh_watch_directories(
                        &workspace,
                        current.keys().map(std::path::Path::new),
                    )?;
                }
                self.io_profile.maintenance_noop_applies+=1;
                self.record_update(self.resident.unchanged_update(started));
                self.blocked=None;
                continue;
            }
            let workspace = self.workspace()?.into_owned();
            let mut changed = BTreeSet::new();
            let mut need_watch_refresh = self.needs_watch_refresh(&workspace);
            for (path, stamp) in &current {
                match self.stamps.get(path) {
                    Some(old) if old == stamp => {}
                    Some(_) => {
                        changed.insert(path.clone());
                    }
                    None => {
                        if let Some(parent) = Path::new(path).parent()
                            && !self.watched_dirs.contains(parent)
                        {
                            need_watch_refresh = true;
                        }
                        changed.insert(path.clone());
                    }
                }
            }
            for path in self.stamps.keys() {
                if !current.contains_key(path) {
                    need_watch_refresh = true;
                    changed.insert(path.clone());
                }
            }
            if need_watch_refresh {
                self.refresh_watch_directories(
                    &workspace,
                    current.keys().map(std::path::Path::new),
                )?;
            }
            let update = self
                .resident
                .update(&workspace, &changed, &current, false)?;
            current.retain(|path, _| self.resident.paths.contains_key(path.as_str()));
            self.record_update(update);
            self.query_cache
                .retain_generation(self.resident.stats.generation);
            self.stamps = current;
            if !changed.is_empty() {
                self.mutation_epoch = self.mutation_epoch.wrapping_add(1);
            }
            self.blocked = None;
        }
        Ok(())
    }

    pub fn call(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.call_with_mode(name, arguments, false)
    }

    pub fn call_with_mode(&mut self, name: &str, mut arguments: Value, compact: bool) -> Result<Value> {
        self.last_used = Instant::now();
        if matches!(name, "code_files" | "code_grep" | "code_read" | "code_context") {
            self.source_lookups = self.source_lookups.saturating_add(1);
        }
        self.synchronize_or_block(name == "code_sync")?;
        self.normalize_root_prefixed_arguments(&mut arguments);
        match name {
            "code_files" => {
                let mut request: crate::files::FileQuery = serde_json::from_value(arguments)?;
                if compact {
                    request.limit = request.limit.clamp(1, 1000);
                }
                self.filter_multi_search_paths(&mut request.paths);
                self.check_explicit_search_targets(&request.paths, Some(&request.glob))?;
                let result =
                    crate::files::find(self.resident.paths.keys().map(|p| p.as_ref()), &request)?;
                self.check_search_paths(result.paths.iter().map(String::as_str))?;
                if compact {
                    #[derive(serde::Serialize)]
                    struct CompactFiles<'a> {
                        paths: &'a [String],
                        total_matches: usize,
                        truncated: bool,
                        freshness: Value,
                    }
                    return Ok(serde_json::to_value(CompactFiles {
                        paths: &result.paths,
                        total_matches: result.total_matches,
                        truncated: result.truncated,
                        freshness: self.search_freshness("metadata"),
                    })?);
                }
                let mut value = serde_json::to_value(result)?;
                value["freshness"] = self.search_freshness("metadata");
                Ok(value)
            }
            "code_grep" => {
                let mut query: Query = serde_json::from_value(arguments)?;
                if compact {
                    query.limit = query.limit.clamp(1, 1000);
                    if let Some(m) = query.max_matches_per_file {
                        query.max_matches_per_file = Some(m.clamp(1, 64));
                    }
                }
                self.filter_multi_search_paths(&mut query.paths);
                self.check_explicit_search_targets(&query.paths, query.glob.as_deref())?;
                let mut budget_narrowed = false;
                let result = match search_fast_arc(&self.resident, &query, &mut self.query_cache) {
                    Ok(res) => res,
                    Err(err) if compact && crate::context::is_response_budget(err.as_ref()) => {
                        budget_narrowed = true;
                        let mut narrowed = query.clone();
                        narrowed.limit = narrowed.limit.min(40);
                        narrowed.max_matches_per_file =
                            Some(narrowed.max_matches_per_file.unwrap_or(2).min(2));
                        search_fast_arc(&self.resident, &narrowed, &mut self.query_cache)?
                    }
                    Err(err) => return Err(err),
                };
                self.check_search_paths(result.matches.iter().map(|hit| hit.path.as_str()))?;
                let mut freshness = self.search_freshness("metadata");
                if result.total_matches == 0 && looks_like_unintended_literal_regex(&query.pattern, query.regex) {
                    append_regex_hint(&mut freshness);
                }
                if compact {
                    #[derive(serde::Serialize)]
                    struct CompactGrepHit<'a> {
                        path: &'a str,
                        line: usize,
                        text: std::borrow::Cow<'a, str>,
                    }
                    #[derive(serde::Serialize)]
                    struct CompactGrep<'a> {
                        matches: Vec<CompactGrepHit<'a>>,
                        total_matches: usize,
                        truncated: bool,
                        freshness: Value,
                    }
                    let matches = result
                        .matches
                        .iter()
                        .map(|hit| {
                            let text = if hit.text.len() > 512 {
                                let end = hit.text.floor_char_boundary(512);
                                std::borrow::Cow::Owned(format!("{}...", &hit.text[..end]))
                            } else {
                                std::borrow::Cow::Borrowed(hit.text.as_str())
                            };
                            CompactGrepHit {
                                path: hit.path.as_str(),
                                line: hit.line,
                                text,
                            }
                        })
                        .collect();
                    return Ok(serde_json::to_value(CompactGrep {
                        matches,
                        total_matches: result.total_matches,
                        truncated: result.truncated || budget_narrowed,
                        freshness,
                    })?);
                }
                let mut value = serde_json::to_value(&*result)?;
                value["freshness"] = freshness;
                Ok(value)
            }
            "code_profile" => {
                let profiled = search_profiled(
                    &self.resident,
                    &serde_json::from_value::<Query>(arguments)?,
                    &mut self.query_cache,
                )?;
                self.last_profile = serde_json::to_value(&profiled.profile)?;
                Ok(serde_json::to_value(profiled)?)
            }
            "code_profile_full" => {
                let profiled = search_profiled_mode(
                    &self.resident,
                    &serde_json::from_value::<Query>(arguments)?,
                    &mut self.query_cache,
                    false,
                )?;
                Ok(serde_json::to_value(profiled)?)
            }
            "code_context" => {
                let mut args = arguments;
                let mut radius = args
                    .as_object_mut()
                    .ok_or_else(|| invalid("arguments must be an object"))?
                    .remove("radius")
                    .map(serde_json::from_value::<usize>)
                    .transpose()?
                    .unwrap_or(3);
                if args.get("limit").is_none() {
                    args["limit"] = json!(20);
                    if args.get("max_matches_per_file").is_none() {
                        args["max_matches_per_file"] = json!(5);
                    }
                }
                let mut query: Query = serde_json::from_value(args)?;
                if compact {
                    radius = radius.min(100);
                    query.limit = query.limit.clamp(1, 64);
                    if let Some(m) = query.max_matches_per_file {
                        query.max_matches_per_file = Some(m.clamp(1, 64));
                    }
                } else if radius > 100 || query.limit > 64 {
                    return Err(invalid("radius 0..100, context limit 1..64 required; use code_read for larger ranges").into());
                }
                self.filter_multi_search_paths(&mut query.paths);
                self.check_explicit_search_targets(&query.paths, query.glob.as_deref())?;
                let regex_hint_needed = !query.regex
                    && looks_like_unintended_literal_regex(&query.pattern, query.regex);
                if let Some(result) =
                    self.query_cache
                        .get_context(self.resident.stats.generation, &query, radius)
                {
                    let mut val = self.verified_context(&result, compact)?;
                    if result.search.total_matches == 0 && regex_hint_needed {
                        append_regex_hint(&mut val["freshness"]);
                    }
                    return Ok(val);
                }
                let build_context = |this: &mut Self, q: &Query, r: usize| -> Result<Arc<crate::context::ContextResult>> {
                    let search_arc = search_fast_arc(&this.resident, q, &mut this.query_cache)?;
                    let search_owned =
                        Arc::try_unwrap(search_arc).unwrap_or_else(|shared| (*shared).clone());
                    Ok(Arc::new(assemble_trusted(search_owned, r, |ranges| {
                        this.resident.read_many(ranges)
                    })?))
                };
                let result = match build_context(self, &query, radius) {
                    Ok(res) => {
                        self.query_cache.put_context(
                            self.resident.stats.generation,
                            query,
                            radius,
                            res.clone(),
                        );
                        res
                    }
                    Err(err) if compact && crate::context::is_response_budget(err.as_ref()) => {
                        let mut narrowed = query.clone();
                        narrowed.limit = narrowed.limit.min(10);
                        narrowed.max_matches_per_file =
                            Some(narrowed.max_matches_per_file.unwrap_or(2).min(2));
                        let narrowed_radius = radius.min(4);
                        build_context(self, &narrowed, narrowed_radius)?
                    }
                    Err(err) => return Err(err),
                };
                let mut val = self.verified_context(&result, compact)?;
                if result.search.total_matches == 0 && regex_hint_needed {
                    append_regex_hint(&mut val["freshness"]);
                }
                Ok(val)
            }
            "code_read" => {
                let request: ReadRequest = serde_json::from_value(arguments)?;
                if request.page {
                    return self.read_page(&request);
                }
                let snippets = self.read_verified_with_recovery(&request)?;
                Ok(serde_json::to_value(snippets)?)
            }
            "code_sync" | "code_status" => {
                if arguments != json!({}) {
                    return Err(invalid("sync/status take empty arguments").into());
                }
                self.status_snapshot()
            }
            _ => Err(invalid(format!("unknown tool {name}")).into()),
        }
    }

    fn normalize_root_prefixed_arguments(&self, arguments: &mut Value) {
        let has_paths = arguments.get("paths").is_some_and(|v| {
            v.as_array().is_some_and(|arr| {
                !arr.is_empty()
                    && !arr
                        .iter()
                        .all(|item| item.as_str().is_some_and(|s| self.stamps.contains_key(s)))
            })
        });
        let has_ranges = arguments.get("ranges").is_some_and(|v| {
            v.as_array().is_some_and(|arr| {
                !arr.is_empty()
                    && !arr.iter().all(|item| {
                        item.get("path")
                            .and_then(Value::as_str)
                            .is_some_and(|s| self.stamps.contains_key(s))
                    })
            })
        });
        if !has_paths && !has_ranges {
            return;
        }
        let Some(root_str) = self.spec.root.to_str() else {
            return;
        };
        let root_prefix = format!("{}/", root_str.trim_end_matches('/'));
        let root_basename_prefix = self
            .spec
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .map(|n| format!("{n}/"));
        let strip_one = |raw: &mut String| {
            let trimmed = raw.trim();
            if trimmed.len() >= 2
                && ((trimmed.starts_with('`') && trimmed.ends_with('`'))
                    || (trimmed.starts_with('"') && trimmed.ends_with('"'))
                    || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
            {
                *raw = trimmed[1..trimmed.len() - 1].trim().to_owned();
            }
            if raw.starts_with('[')
                && let Some((_, rest)) = raw.split_once("](")
                && let Some(target) = rest.strip_suffix(')')
            {
                let mut target = target.trim();
                if target.starts_with('<') && target.ends_with('>') && target.len() >= 2 {
                    target = target[1..target.len() - 1].trim();
                }
                *raw = target.to_owned();
            }
            if let Some(rest) = raw.strip_prefix("-->") {
                *raw = rest.trim().to_owned();
            }
            if (raw.starts_with("https://github.com/") || raw.starts_with("https://gitlab.com/"))
                && let Some((_, after_blob)) = raw.split_once("/blob/")
                && let Some((_, repo_rel)) = after_blob.split_once('/')
                && !repo_rel.is_empty()
            {
                *raw = repo_rel.to_owned();
            }
            if let Some(rest) = raw.strip_prefix("File \"")
                && let Some((file_part, _)) = rest.split_once('"')
            {
                *raw = file_part.to_owned();
            }
            if let Some(rest) = raw.strip_prefix("file://") {
                *raw = rest.to_owned();
            } else if let Some(scheme_end) = raw.find("://")
                && matches!(
                    &raw[..scheme_end],
                    "vscode-remote"
                        | "vscode-file"
                        | "vscode-userdata"
                        | "vscode-insiders"
                        | "cursor"
                        | "windsurf"
                        | "vscodium"
                        | "git"
                )
                && let Some(slash) = raw[scheme_end + 3..].find('/')
            {
                *raw = raw[scheme_end + 3 + slash..].to_owned();
            }
            if raw.contains("%20") {
                *raw = raw.replace("%20", " ");
            }
            if !raw.starts_with('/') && !raw.starts_with("\\\\") && raw.contains('\\') {
                *raw = raw.replace('\\', "/");
            }
            if raw == root_str {
                *raw = ".".into();
                return;
            }
            if let Some(rest) = raw.strip_prefix(&root_prefix)
                && !rest.is_empty()
            {
                *raw = rest.to_owned();
            }
            while let Some(rest) = raw.strip_prefix("./") {
                if !rest.is_empty() {
                    *raw = rest.to_owned();
                } else {
                    break;
                }
            }
            let candidate_exists = |path_candidate: &str| -> bool {
                if self.stamps.contains_key(path_candidate)
                    || self.spec.root.join(path_candidate).exists()
                {
                    return true;
                }
                if let Some(rest) = path_candidate
                    .strip_prefix("@/")
                    .or_else(|| path_candidate.strip_prefix("~/"))
                    && !rest.is_empty()
                {
                    let aliased = format!("src/{rest}");
                    return self.stamps.contains_key(aliased.as_str())
                        || self.spec.root.join(&aliased).exists();
                }
                false
            };
            if !self.stamps.contains_key(raw.as_str())
                && let Some((base, query)) = raw.split_once('?')
                && !base.is_empty()
                && [
                    "raw",
                    "url",
                    "worker",
                    "sharedworker",
                    "inline",
                    "inline-css",
                    "no-inline",
                    "react",
                    "component",
                    "used",
                ]
                .iter()
                .any(|tag| query == *tag || query.starts_with(&format!("{tag}&")))
            {
                *raw = base.to_owned();
            }
            if !self.stamps.contains_key(raw.as_str()) {
                if let Some((base, suffix)) = raw.rsplit_once('#')
                    && !base.is_empty()
                    && !suffix.is_empty()
                    && candidate_exists(base)
                {
                    *raw = base.to_owned();
                } else if let Some((base, suffix)) = raw.split_once(':')
                    && !base.is_empty()
                    && !suffix.is_empty()
                    && suffix.as_bytes()[0].is_ascii_digit()
                    && candidate_exists(base)
                {
                    *raw = base.to_owned();
                } else if let Some(open) = raw.rfind('(')
                    && open > 0
                    && raw.ends_with(')')
                    && raw[open + 1..raw.len() - 1]
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b == b',' || b == b' ')
                {
                    let base = &raw[..open];
                    if candidate_exists(base) {
                        *raw = base.to_owned();
                    }
                }
            }
            if !self.stamps.contains_key(raw.as_str())
                && !self.spec.root.join(raw.as_str()).exists()
            {
                if let Some(ref b_prefix) = root_basename_prefix
                    && let Some(rest) = raw.strip_prefix(b_prefix.as_str())
                    && !rest.is_empty()
                    && (self.stamps.contains_key(rest) || self.spec.root.join(rest).exists())
                {
                    *raw = rest.to_owned();
                } else if let Some(rest) = raw
                    .strip_prefix("@/")
                    .or_else(|| raw.strip_prefix("~/"))
                    .or_else(|| raw.strip_prefix('#'))
                    && !rest.is_empty()
                {
                    let candidate = format!("src/{rest}");
                    if self.stamps.contains_key(candidate.as_str())
                        || self.spec.root.join(&candidate).exists()
                    {
                        *raw = candidate;
                    }
                } else if let Some(rest) = raw
                    .strip_prefix("a/")
                    .or_else(|| raw.strip_prefix("b/"))
                    && !rest.is_empty()
                    && self.stamps.contains_key(rest)
                {
                    *raw = rest.to_owned();
                }
            }
        };
        if let Some(paths) = arguments.get_mut("paths").and_then(Value::as_array_mut) {
            let mut expanded = Vec::with_capacity(paths.len());
            for item in paths.drain(..) {
                if let Some(text) = item.as_str() {
                    if text.contains(',') && !text.contains('{') {
                        for part in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                            let mut owned = part.to_owned();
                            strip_one(&mut owned);
                            expanded.push(Value::String(owned));
                        }
                    } else {
                        let mut owned = text.to_owned();
                        strip_one(&mut owned);
                        expanded.push(Value::String(owned));
                    }
                } else {
                    expanded.push(item);
                }
            }
            *paths = expanded;
        }
        if let Some(ranges) = arguments.get_mut("ranges").and_then(Value::as_array_mut) {
            let initial_len = ranges.len();
            let mut expanded_ranges = Vec::with_capacity(initial_len);
            for (idx, mut range) in ranges.drain(..).enumerate() {
                let mut md_anchor: Option<String> = None;
                if let Some(path_val) = range.get_mut("path")
                    && let Some(text) = path_val.as_str()
                {
                    let mut owned = text.to_owned();
                    if let Some((base, frag)) = owned.rsplit_once('#') {
                        let frag = frag.trim_end_matches(')').trim_end_matches('>');
                        if !base.is_empty()
                            && !frag.is_empty()
                            && !frag.contains('/')
                            && !frag
                                .bytes()
                                .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'L' | b'C'))
                        {
                            md_anchor = Some(frag.to_owned());
                        }
                    }
                    strip_one(&mut owned);
                    if owned != text {
                        *path_val = Value::String(owned);
                    }
                }
                if let Some(anchor) = md_anchor
                    && let Some(path_str) =
                        range.get("path").and_then(Value::as_str).map(str::to_owned)
                    && crate::document::is_document_path(&path_str)
                    && range.get("start").and_then(Value::as_u64).unwrap_or(1) == 1
                    && let Some(&doc_id) = self.resident.paths.get(path_str.as_str())
                {
                    if anchor.eq_ignore_ascii_case("toc") || anchor.eq_ignore_ascii_case("outline")
                    {
                        let remaining_after = initial_len.saturating_sub(idx + 1);
                        let budget = 64usize
                            .saturating_sub(expanded_ranges.len() + remaining_after)
                            .max(1);
                        let toc_ranges = crate::document::resolve_toc_ranges(
                            &self.resident.documents[doc_id].text,
                            budget,
                        );
                        if !toc_ranges.is_empty() {
                            for (s, e) in toc_ranges {
                                expanded_ranges.push(json!({
                                    "path": path_str,
                                    "start": s,
                                    "end": e,
                                }));
                            }
                            continue;
                        }
                    } else if let Some((sec_start, sec_end)) =
                        crate::document::resolve_markdown_anchor_range(
                            &self.resident.documents[doc_id].text,
                            &anchor,
                        )
                        && let Some(obj) = range.as_object_mut()
                    {
                        obj.insert("start".into(), json!(sec_start));
                        obj.insert("end".into(), json!(sec_end));
                    }
                }
                expanded_ranges.push(range);
            }
            *ranges = expanded_ranges;
        }
    }

    pub(crate) fn status_snapshot(&self) -> Result<Value> {
        let workspace = self.workspace()?;
        let exclusion_summary = workspace.exclusion_summary();
        Ok(json!({"root":self.spec.root,"index_state":"loaded",
                    "status_freshness":"observational snapshot; status does not load, reconcile, or hash source",
                    "stats":self.resident.stats,"last_update":self.last_update,
                    "freshness":"watcher events reconcile changed directories before query; full background metadata reconciliation runs every 60s; code_sync hashes all permitted source; no atomic filesystem transaction",
                    "blocked":self.blocked,"last_query_profile":self.last_profile,
                    "query_cache":self.query_cache.stats(),
                    "watched_directories":self.watched_dirs.len(),
                    "root_admission_estimated_bytes":self.estimated_bytes(),
                    "runtime_headroom_bytes":ROOT_RUNTIME_HEADROOM,
                    "transparent_read_recoveries":self.transparent_read_recoveries,
                    "transparent_read_recovery_failures":self.transparent_read_recovery_failures,
                    "io_profile":self.io_profile,
                    "reconcile_interval_seconds":RECONCILE_INTERVAL.as_secs(),
                    "detected_profiles":exclusion_summary.profiles,
                    "effective_exclude_count":workspace.excludes.len()+exclusion_summary.patterns.len(),
                    "idle_timeout_seconds":IDLE_ROOT_TIMEOUT.as_secs()}))
    }

    pub(crate) fn recover_stale_search(&mut self, error_message: &str) -> Result<bool> {
        let rest = if let Some(rest) = error_message.strip_prefix("STALE_INDEX ") {
            rest
        } else if let Some(rest) = error_message.strip_prefix("TARGET_NOT_INDEXED ")
            && error_message.contains("reason=not_yet_indexed")
        {
            rest
        } else {
            return Ok(false);
        };
        let Some(path_part) = rest
            .split_whitespace()
            .find_map(|token| token.strip_prefix("path="))
        else {
            return Ok(false);
        };
        let relative_str = path_part.trim_end_matches(':');
        if crate::workspace::safe_relative(Path::new(relative_str)).is_err() {
            return Ok(false);
        }
        let relative = crate::workspace::normalize(Path::new(relative_str));
        let mut directories = BTreeMap::new();
        Self::record_affected_directory(
            &self.spec.root,
            &mut directories,
            relative.parent().unwrap_or(Path::new("")),
        );
        let mut forced = BTreeSet::new();
        forced.insert(relative.to_string_lossy().into_owned());
        self.reconcile_directories(&directories, &forced)?;
        self.transparent_read_recoveries = self.transparent_read_recoveries.saturating_add(1);
        Ok(true)
    }

    fn stale_search_error(&self, path: &str) -> std::io::Error {
        invalid(format!(
            "STALE_INDEX root={} path={path}: the saved disk file differs from the indexed snapshot or was removed; stale search/context was not returned. Use code_read with this root/path to refresh known ranges, or a host policy-aware live disk read. Use code_sync before exhaustive absence claims; do not bypass exclusions.",
            self.spec.root.display()
        ))
    }

    fn check_search_paths<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Result<()> {
        let mut checked = BTreeSet::new();
        let mut checked_dirs: BTreeSet<&'a Path> = BTreeSet::new();
        let mut last_path: Option<&'a str> = None;
        for path in paths {
            if last_path == Some(path) || !checked.insert(path) {
                continue;
            }
            last_path = Some(path);
            let rel_path = Path::new(path);
            let Some(expected) = self.stamps.get(path) else {
                crate::workspace::safe_relative(rel_path)?;
                return Err(self.stale_search_error(path).into());
            };
            if let Some(parent) = rel_path.parent()
                && !parent.as_os_str().is_empty()
            {
                for ancestor in parent.ancestors() {
                    if ancestor.as_os_str().is_empty() || !checked_dirs.insert(ancestor) {
                        break;
                    }
                    let dir_meta = match std::fs::symlink_metadata(self.spec.root.join(ancestor)) {
                        Ok(meta) => meta,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            return Err(self.stale_search_error(path).into());
                        }
                        Err(error) => return Err(error.into()),
                    };
                    if dir_meta.file_type().is_symlink() {
                        return Err(
                            invalid("symlinks are unsupported in this local experiment").into(),
                        );
                    }
                }
            }
            let absolute = self.spec.root.join(rel_path);
            let meta = match std::fs::symlink_metadata(&absolute) {
                Ok(meta) => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(self.stale_search_error(path).into());
                }
                Err(error) => return Err(error.into()),
            };
            if meta.file_type().is_symlink() {
                return Err(invalid("symlinks are unsupported in this local experiment").into());
            }
            let current = crate::incremental::stamp_from_metadata(&meta)?;
            if &current != expected {
                return Err(self.stale_search_error(path).into());
            }
        }
        Ok(())
    }

    fn filter_multi_search_paths(&self, paths: &mut Vec<String>) {
        if paths.len() < 2 {
            return;
        }
        let is_admitted_search_target = |raw: &str| -> bool {
            let p = Path::new(raw);
            if crate::workspace::safe_relative(p).is_err() {
                return false;
            }
            let norm = crate::workspace::normalize(p);
            let norm_str = norm.to_string_lossy();
            if norm_str == "." || self.stamps.contains_key(norm_str.as_ref()) {
                return true;
            }
            let prefix = format!("{}/", norm_str.trim_end_matches('/'));
            if self.stamps.keys().any(|k| k.starts_with(&prefix)) {
                return true;
            }
            // If it is an existing file on disk that is not in stamps, it would fail check_explicit_search_targets
            if let Ok(meta) = std::fs::symlink_metadata(self.spec.root.join(&norm))
                && (meta.is_file() || meta.file_type().is_symlink())
            {
                return false;
            }
            true
        };
        let filtered: Vec<String> = paths
            .iter()
            .filter(|item| is_admitted_search_target(item.as_str()))
            .cloned()
            .collect();
        if !filtered.is_empty() && filtered.len() < paths.len() {
            *paths = filtered;
        }
    }

    fn check_explicit_search_targets(&self, paths: &[String], glob: Option<&str>) -> Result<()> {
        if paths.is_empty() && glob.is_none() {
            return Ok(());
        }
        for path in crate::query::paths(paths)? {
            let name = path.to_string_lossy();
            if self.stamps.contains_key(name.as_ref()) {
                self.check_search_paths([name.as_ref()])?;
            } else {
                match std::fs::symlink_metadata(self.spec.root.join(&path)) {
                    Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                        let reason = self.workspace()?.unindexed_reason(&name, &self.spec.policy)?;
                        let action = if reason == "not_yet_indexed" {
                            "Use code_read for known saved ranges or a permitted host read; do not interpret this as absence.".into()
                        } else if reason == "unsupported_glob" {
                            format!(
                                "Do not retry this target with code_context/code_grep/code_read or switch CCTX transports. Use a policy-aware host read only if allowed; do not widen access automatically. (To admit standard source/config globs on a legacy registration, run: cctx --root {} migrate --apply)",
                                self.spec.root.display()
                            )
                        } else {
                            "Do not retry this target with code_context/code_grep/code_read or switch CCTX transports. Use a policy-aware host read only if allowed; do not widen access automatically.".into()
                        };
                        return Err(invalid(format!(
                            "TARGET_NOT_INDEXED root={} path={name} reason={reason}: no indexed version. {action}",
                            self.spec.root.display()
                        )).into());
                    }
                    Ok(_) => {}
                    // Search prefixes may be valid filters without being valid filesystem names.
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound
                                | std::io::ErrorKind::NotADirectory
                                | std::io::ErrorKind::InvalidFilename
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        if let Some(glob) = glob
            && !glob.contains(['*', '?', '[', ']', '{', '}', '\\'])
            && self.stamps.contains_key(glob)
        {
            self.check_search_paths([glob])?;
        }
        Ok(())
    }

    fn search_freshness(&self, returned_file_check: &str) -> Value {
        json!({
            "basis":"watcher_snapshot", "generation":self.resident.stats.generation,
            "returned_file_check":returned_file_check, "full_disk_verified":false,
            "notice":"Verify recent edits with code_read or a host policy-aware live disk read; use code_sync before claiming absence."
        })
    }

    fn verified_context(
        &self,
        result: &crate::context::ContextResult,
        compact: bool,
    ) -> Result<Value> {
        let ws = self.workspace()?;
        if let Some(path) = self.stale_snippet_path_with_workspace(ws.as_ref(), &result.snippets)? {
            return Err(self.stale_search_error(&path).into());
        }
        if compact {
            #[derive(serde::Serialize)]
            struct CompactHit<'a> {
                path: &'a str,
                line: usize,
            }
            #[derive(serde::Serialize)]
            struct CompactSearch<'a> {
                matches: Vec<CompactHit<'a>>,
                total_matches: usize,
                truncated: bool,
            }
            #[derive(serde::Serialize)]
            struct CompactContext<'a> {
                search: CompactSearch<'a>,
                snippets: &'a [Snippet],
                freshness: Value,
            }
            let matches = result
                .search
                .matches
                .iter()
                .map(|hit| CompactHit {
                    path: hit.path.as_str(),
                    line: hit.line,
                })
                .collect();
            return Ok(serde_json::to_value(CompactContext {
                search: CompactSearch {
                    matches,
                    total_matches: result.search.total_matches,
                    truncated: result.search.truncated,
                },
                snippets: &result.snippets,
                freshness: self.search_freshness("sha256"),
            })?);
        }
        let mut value = serde_json::to_value(result)?;
        value["freshness"] = self.search_freshness("sha256");
        Ok(value)
    }

    fn read_verified_with_recovery(&mut self, request: &ReadRequest) -> Result<Vec<Snippet>> {
        if request.split_ranges {
            if request.ranges.is_empty()
                || request.ranges.len() > 64
                || request
                    .ranges
                    .iter()
                    .any(|range| range.start == 0 || range.end < range.start)
            {
                return Err(invalid(
                    "READ_RANGE_LIMIT: provide1..64 valid1-based inclusive ranges",
                )
                .into());
            }
        } else {
            crate::context::validate_ranges(&request.ranges)?;
        }
        let needs_compile = self
            .read_policy
            .as_ref()
            .is_none_or(|(saved_spec, _, _)| saved_spec != &self.spec);
        if needs_compile {
            let workspace = self.workspace()?.into_owned();
            let key = workspace.candidate_policy_key();
            if self
                .read_policy
                .as_ref()
                .is_none_or(|(_, _, policy)| policy.key != key)
            {
                let policy = if let Some((saved_spec, watcher_pol)) = &self.watcher_policy
                    && saved_spec == &self.spec
                    && watcher_pol.key == key
                {
                    watcher_pol.clone()
                } else {
                    match crate::workspace::CandidatePolicy::compile(key) {
                        Ok(policy) => policy,
                        Err(error) => {
                            self.io_profile.read_policy_rejections += 1;
                            return Err(error);
                        }
                    }
                };
                self.cached_workspace = Some((self.spec.clone(), workspace.clone()));
                self.watcher_policy = Some((self.spec.clone(), policy.clone()));
                self.read_policy = Some((self.spec.clone(), workspace, policy));
                self.io_profile.read_policy_compilations += 1;
            } else {
                let (_, _, policy) = self.read_policy.take().expect("cached policy");
                self.cached_workspace = Some((self.spec.clone(), workspace.clone()));
                self.read_policy = Some((self.spec.clone(), workspace, policy));
                self.io_profile.read_policy_cache_hits += 1;
            }
        } else {
            self.io_profile.read_policy_cache_hits += 1;
        }
        if !needs_compile
            && request
                .ranges
                .iter()
                .all(|range| self.stamps.contains_key(&range.path))
            && let Ok(ranges) = self.plan_read_ranges(request)
            && let Ok(snippets) = self.resident.read_many(&ranges)
        {
            let (_, workspace, _) = self.read_policy.as_ref().expect("compiled read policy");
            if self
                .stale_snippet_path_with_workspace(workspace, &snippets)?
                .is_none()
            {
                return Ok(snippets);
            }
        }
        let mut requested = BTreeSet::new();
        let mut directories = BTreeMap::new();
        for range in &request.ranges {
            let (_, _, policy) = self.read_policy.as_ref().expect("compiled read policy");
            match policy.candidate_path(&range.path) {
                Ok(true) => {}
                Ok(false) => {
                    let reason = policy
                        .rejection_reason(&range.path)?
                        .unwrap_or("not_admitted");
                    self.io_profile.read_policy_rejections += 1;
                    self.transparent_read_recovery_failures += 1;
                    let suggestions = self.suggest_similar_paths(&range.path);
                    let suggest_hint = if suggestions.is_empty() {
                        String::new()
                    } else {
                        format!(" did_you_mean=[{}].", suggestions.join(", "))
                    };
                    let migrate_hint = if reason == "unsupported_glob" {
                        format!(" (To admit standard source/config globs on a legacy registration, run: cctx --root {} migrate --apply)", self.spec.root.display())
                    } else {
                        String::new()
                    };
                    return Err(invalid(format!(
                        "READ_POLICY_DENIED root={} path={} reason={reason}: no source was read.{suggest_hint} Do not retry this target with context/search/read or switch CCTX transports. Use a host policy-aware read only if allowed; do not widen access automatically.{migrate_hint}",
                        self.spec.root.display(), range.path,
                    )).into());
                }
                Err(error) => {
                    self.io_profile.read_policy_rejections += 1;
                    return Err(error);
                }
            }
            let path = crate::workspace::normalize(Path::new(&range.path));
            Self::record_affected_directory(
                &self.spec.root,
                &mut directories,
                path.parent().unwrap_or(Path::new("")),
            );
            requested.insert(path.to_string_lossy().into_owned());
        }
        let workspace = self
            .read_policy
            .as_ref()
            .expect("compiled read policy")
            .1
            .clone();
        let mut recovering = false;
        let mut unstable_paths = BTreeSet::new();
        let indexed_before_recovery: BTreeSet<_> = requested
            .iter()
            .filter(|path| self.stamps.contains_key(path.as_str()))
            .cloned()
            .collect();
        for attempt in 0..3 {
            let ranges = self.plan_read_ranges(request)?;
            match self.resident.read_many(&ranges) {
                Ok(snippets) => {
                    match self.stale_snippet_path_with_workspace(&workspace, &snippets)? {
                        None => {
                            if recovering {
                                self.transparent_read_recoveries =
                                    self.transparent_read_recoveries.saturating_add(1);
                            }
                            return Ok(snippets);
                        }
                        Some(path) => {
                            unstable_paths.insert(path);
                        }
                    }
                }
                Err(error) if crate::resident::is_snapshot_miss(error.as_ref()) => {}
                Err(error)
                    if crate::resident::is_snapshot_range_error(error.as_ref()) && attempt == 0 => {
                }
                Err(error) if crate::context::is_response_budget(error.as_ref()) => {
                    return Err(error);
                }
                Err(error) => {
                    return Err(
                        invalid(format!("root={}: {error}", self.spec.root.display())).into(),
                    );
                }
            }
            if attempt == 2 {
                break;
            }
            recovering = true;
            if attempt > 0 {
                std::thread::sleep(READ_RECOVERY_DELAY);
            }
            // A file appearing during inventory is not proof of a permanent exclusion.
            let present_before_reconcile:BTreeSet<_>=requested.iter()
                .filter(|path|workspace.root.join(path).exists()).cloned().collect();
            if let Err(error) = self.reconcile_directories(&directories, &requested) {
                self.blocked = Some(error.to_string());
                return Err(error);
            }
            if requested
                .iter()
                .any(|path| !indexed_before_recovery.contains(path)
                    && present_before_reconcile.contains(path)
                    && !self.stamps.contains_key(path) && workspace.root.join(path).exists())
            {
                self.io_profile.read_policy_rejections += 1;
                break;
            }
        }
        self.transparent_read_recovery_failures =
            self.transparent_read_recovery_failures.saturating_add(1);
        let mut reasons = Vec::new();
        for path in &requested {
            let reason = if self.stamps.contains_key(path) {
                if !unstable_paths.contains(path) {
                    continue;
                }
                "unstable_saved_version"
            } else {
                match std::fs::symlink_metadata(workspace.root.join(path)) {
                    Ok(_) => workspace.unindexed_reason(path, &self.spec.policy)?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing_on_disk",
                    Err(error) => return Err(error.into()),
                }
            };
            let suggestion = if reason == "missing_on_disk" {
                let suggestions = self.suggest_similar_paths(path);
                if suggestions.is_empty() {
                    String::new()
                } else {
                    format!(" (did_you_mean=[{}])", suggestions.join(", "))
                }
            } else {
                String::new()
            };
            reasons.push(format!("path={path} reason={reason}{suggestion}"));
            if reasons.len() == 8 {
                break;
            }
        }
        let valid_indexed: Vec<&str> = requested
            .iter()
            .filter(|path| self.stamps.contains_key(path.as_str()) && !unstable_paths.contains(path.as_str()))
            .take(8)
            .map(String::as_str)
            .collect();
        let valid_hint = if valid_indexed.is_empty() {
            String::new()
        } else {
            format!(" Valid indexed members in this batch: [{}].", valid_indexed.join(", "))
        };
        Err(invalid(format!(
            "READ_UNAVAILABLE root={}: file is unavailable after automatic recovery; {}. Batch failed atomically; no partial success. Discover actual paths with code_files/code_context; use a host policy-aware read for permitted non-indexed files. Do not repeat missing/excluded paths.{}{}",
            self.spec.root.display(), reasons.join("; "), valid_hint,
            if requested.len() > 8 { " Additional requested paths omitted from this diagnostic." } else { "" }
        )).into())
    }

    fn stale_snippet_path(&self, snippets: &[Snippet]) -> Result<Option<String>> {
        let ws = self.workspace()?;
        self.stale_snippet_path_with_workspace(ws.as_ref(), snippets)
    }

    fn stale_snippet_path_with_workspace(
        &self,
        workspace: &crate::workspace::Workspace,
        snippets: &[Snippet],
    ) -> Result<Option<String>> {
        let mut unique: Vec<&Snippet> = Vec::with_capacity(snippets.len());
        let mut seen = BTreeSet::new();
        for snippet in snippets {
            if seen.insert(snippet.path.as_str()) {
                if !self.stamps.contains_key(&snippet.path) {
                    return Err(invalid(
                        "file is outside configured scope, ignored, or excluded by glob",
                    )
                    .into());
                }
                unique.push(snippet);
            }
        }
        let mut checked_dirs: BTreeSet<&Path> = BTreeSet::new();
        for snippet in &unique {
            let rel_path = Path::new(&snippet.path);
            if let Some(parent) = rel_path.parent()
                && !parent.as_os_str().is_empty()
            {
                for ancestor in parent.ancestors() {
                    if ancestor.as_os_str().is_empty() || !checked_dirs.insert(ancestor) {
                        break;
                    }
                    let dir_meta = match std::fs::symlink_metadata(workspace.root.join(ancestor)) {
                        Ok(meta) => meta,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            return Ok(Some(snippet.path.clone()));
                        }
                        Err(error) => return Err(error.into()),
                    };
                    if dir_meta.file_type().is_symlink() {
                        return Err(
                            invalid("symlinks are unsupported in this local experiment").into(),
                        );
                    }
                }
            }
        }
        let resident = &self.resident;
        let stamps = &self.stamps;
        let max_file_bytes = self.spec.policy.max_file_bytes;
        let verify_with_buf = |snippet: &Snippet, buf: &mut Vec<u8>| -> Result<bool> {
            let rel_path = Path::new(&snippet.path);
            let absolute = workspace.root.join(rel_path);
            let meta = match std::fs::symlink_metadata(&absolute) {
                Ok(meta) => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
                Err(error) => return Err(error.into()),
            };
            if meta.file_type().is_symlink() {
                return Err(invalid("symlinks are unsupported in this local experiment").into());
            }
            if meta.len() > max_file_bytes as u64 {
                return Err(invalid(format!(
                    "binary file or file larger than configured limit is unsupported: {}",
                    rel_path.display()
                ))
                .into());
            }
            let known_doc = resident
                .paths
                .get(snippet.path.as_str())
                .map(|&id| &resident.documents[id])
                .filter(|doc| doc.sha256_hex.as_ref() == snippet.sha256);
            if let Some(doc) = known_doc {
                if stamps
                    .get(&snippet.path)
                    .is_some_and(|s| s.length != meta.len())
                {
                    return Ok(true);
                }
                if !crate::document::is_container_or_rich_document(rel_path)
                    && meta.len() as usize != doc.text.len()
                    && !matches!(meta.len() % 2, 0)
                {
                    return Ok(true);
                }
                if (meta.len() > 64 || crate::document::is_binary_container_document(rel_path))
                    && let Ok(current_stamp) = crate::incremental::stamp_from_metadata(&meta)
                    && stamps.get(&snippet.path) == Some(&current_stamp)
                {
                    return Ok(false);
                }
            }
            buf.clear();
            let needed = (meta.len() as usize).min(max_file_bytes).saturating_add(1);
            if buf.capacity() < needed {
                buf.reserve(needed - buf.len());
            }
            use std::io::Read;
            match std::fs::File::open(&absolute) {
                Ok(file) => {
                    file.take(max_file_bytes as u64 + 1).read_to_end(buf)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
                Err(error) => return Err(error.into()),
            }
            if buf.len() > max_file_bytes {
                return Err(invalid(format!(
                    "binary file or file larger than configured limit is unsupported: {}",
                    rel_path.display()
                ))
                .into());
            }
            use sha2::{Digest, Sha256};
            let digest: [u8; 32] =
                if let Some(extracted) = crate::document::extract_document_bytes(rel_path, buf)? {
                    Sha256::digest(extracted.as_bytes()).into()
                } else {
                    if memchr::memchr(0, buf).is_some() {
                        return Err(invalid(format!(
                            "binary file or file larger than configured limit is unsupported: {}",
                            rel_path.display()
                        ))
                        .into());
                    }
                    Sha256::digest(&*buf).into()
                };
            if let Some(doc) = known_doc {
                return Ok(digest != doc.sha256);
            }
            Ok(crate::resident::digest_hex(&digest) != snippet.sha256)
        };
        if unique.len() >= 4 {
            let workers = unique.len().min(4);
            let chunk_size = unique.len().div_ceil(workers);
            let outcomes = std::thread::scope(|scope| {
                let handles: Vec<_> = unique
                    .chunks(chunk_size)
                    .map(|chunk| {
                        std::thread::Builder::new()
                            .name("cctx-verify".into())
                            .stack_size(WORKER_STACK_BYTES)
                            .spawn_scoped(scope, move || {
                                let mut buf = Vec::new();
                                let mut results = Vec::with_capacity(chunk.len());
                                for &snippet in chunk {
                                    results.push((snippet.path.as_str(), verify_with_buf(snippet, &mut buf)?));
                                }
                                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(results)
                            })
                    })
                    .collect::<std::io::Result<Vec<_>>>()?;
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .map_err(|_| invalid("snippet verification worker panicked").into())
                            .and_then(|r| r)
                    })
                    .collect::<Result<Vec<_>>>()
            })?;
            for group in outcomes {
                for (path, is_stale) in group {
                    if is_stale {
                        return Ok(Some(path.to_owned()));
                    }
                }
            }
            return Ok(None);
        }
        let mut buf = Vec::new();
        for snippet in unique {
            if verify_with_buf(snippet, &mut buf)? {
                return Ok(Some(snippet.path.clone()));
            }
        }
        Ok(None)
    }

    fn read_page(&mut self, request: &ReadRequest) -> Result<Value> {
        crate::context::validate_ranges(
            &request
                .ranges
                .iter()
                .map(|range| crate::context::ReadRange {
                    path: range.path.clone(),
                    start: range.start,
                    end: range.start,
                })
                .collect::<Vec<_>>(),
        )?;
        for range in &request.ranges {
            if range.end < range.start {
                return Err(invalid("read range end precedes start").into());
            }
            self.read_verified_with_recovery(&ReadRequest {
                ranges: vec![crate::context::ReadRange {
                    path: range.path.clone(),
                    start: range.start,
                    end: range.start,
                }],
                split_ranges: false,
                page: false,
            })?;
        }
        let mut pending: std::collections::VecDeque<_> = self.plan_read_ranges(request)?.into();
        let mut snippets = Vec::new();
        let mut splits = 0;
        while let Some(range) = pending.pop_front() {
            let single = ReadRequest {
                ranges: vec![range.clone()],
                split_ranges: false,
                page: false,
            };
            let mut read = match self.read_verified_with_recovery(&single) {
                Ok(read) => read,
                Err(error)
                    if crate::context::is_response_budget(error.as_ref())
                        && range.end > range.start =>
                {
                    splits += 1;
                    if splits > 64 {
                        return Err(invalid(
                            "READ_PAGE_LIMIT: too many page splits; request fewer lines",
                        )
                        .into());
                    }
                    let middle = range.start + (range.end - range.start) / 2;
                    pending.push_front(crate::context::ReadRange {
                        path: range.path.clone(),
                        start: middle + 1,
                        end: range.end,
                    });
                    pending.push_front(crate::context::ReadRange {
                        path: range.path,
                        start: range.start,
                        end: middle,
                    });
                    continue;
                }
                Err(error) => return Err(error),
            };
            snippets.append(&mut read);
            if let Err(error) = crate::context::check_snippets_size(&snippets) {
                if !crate::context::is_response_budget(error.as_ref()) {
                    return Err(error);
                }
                snippets.pop();
                pending.push_front(range);
                break;
            }
        }
        let mut hashes = BTreeMap::new();
        for snippet in &snippets {
            if let Some(previous) = hashes.insert(&snippet.path, &snippet.sha256)
                && previous != &snippet.sha256
            {
                return Err(self.stale_search_error(&snippet.path).into());
            }
        }
        if let Some(path) = self.stale_snippet_path(&snippets)? {
            return Err(self.stale_search_error(&path).into());
        }
        let value = json!({"root":self.spec.root,"snippets":snippets,"complete":pending.is_empty(),
            "next_ranges":pending,"page":true,
            "notice":"Explicit read page, not an implicit complete batch. Continue with page=true and next_ranges. Compare whole-file hashes across pages; if they differ restart the read. No atomic multi-page filesystem view."});
        if serde_json::to_vec(&value)?.len() > crate::workspace::MAX_RESPONSE {
            return Err(invalid(
                "READ_PAGE_LIMIT: page metadata exceeds256KiB; request fewer ranges",
            )
            .into());
        }
        Ok(value)
    }

    fn suggest_similar_paths(&self, missing: &str) -> Vec<&str> {
        let req_path = Path::new(missing);
        let Some(file_name) = req_path.file_name().and_then(|n| n.to_str()) else {
            return Vec::new();
        };
        let stem = req_path.file_stem().and_then(|s| s.to_str()).unwrap_or(file_name);
        let parent_str = req_path.parent().and_then(|p| p.to_str()).unwrap_or("");
        let mut same_dir_stem = Vec::new();
        let mut same_filename = Vec::new();
        for indexed in self.resident.paths.keys() {
            let p = Path::new(indexed.as_ref());
            let idx_parent = p.parent().and_then(|pr| pr.to_str()).unwrap_or("");
            let idx_name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let idx_stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if !parent_str.is_empty() && idx_parent == parent_str && idx_stem == stem {
                same_dir_stem.push(indexed.as_ref());
            } else if idx_name == file_name {
                same_filename.push(indexed.as_ref());
            }
        }
        same_dir_stem.extend(same_filename);
        same_dir_stem.truncate(3);
        same_dir_stem
    }

    fn plan_read_ranges(&self, request: &ReadRequest) -> Result<Vec<crate::context::ReadRange>> {
        if !request.split_ranges {
            return Ok(request.ranges.clone());
        }
        let mut planned: Vec<crate::context::ReadRange> = Vec::new();
        let mut reached_eof_paths: BTreeSet<String> = BTreeSet::new();
        for range in &request.ranges {
            let path = crate::workspace::normalize(std::path::Path::new(&range.path));
            let path_key = path.to_string_lossy().into_owned();
            let doc_lines = self
                .resident
                .paths
                .get(path_key.as_str())
                .map(|id| self.resident.documents[*id].starts.len());
            if let Some(lines) = doc_lines
                && range.start > lines
                && reached_eof_paths.contains(&path_key)
            {
                continue;
            }
            let end = doc_lines
                .filter(|lines| *lines >= range.start)
                .map_or(range.end.min(range.start.saturating_add(499)), |lines| {
                    if range.end >= lines {
                        reached_eof_paths.insert(path_key);
                    }
                    range.end.min(lines)
                });
            let mut start = range.start;
            while start <= end {
                if planned.len() >= 64 {
                    return Err(invalid(format!("READ_PLAN_TOO_LARGE path={}: more than64segments required; split the request into fewer ranges. No partial result returned.",range.path)).into());
                }
                let stop = end.min(start.saturating_add(499));
                planned.push(crate::context::ReadRange {
                    path: range.path.clone(),
                    start,
                    end: stop,
                });
                if stop == usize::MAX {
                    break;
                }
                start = stop + 1;
            }
        }
        Ok(planned)
    }

    fn synchronize_or_block(&mut self, strict: bool) -> Result<()> {
        if let Err(error) = self.synchronize(strict) {
            self.blocked = Some(error.to_string());
            return Err(error);
        }
        Ok(())
    }
}

fn looks_like_unintended_literal_regex(pattern: &str, regex: bool) -> bool {
    if regex || pattern.len() < 3 {
        return false;
    }
    pattern.contains("\\s")
        || pattern.contains("\\b")
        || pattern.contains("\\w")
        || pattern.contains("\\d")
        || pattern.contains(".*")
        || pattern.contains(".+")
        || pattern.contains("(?:")
        || (pattern.contains('|')
            && !pattern.contains("||")
            && !pattern.contains("| ")
            && !pattern.contains(" |"))
}

fn append_regex_hint(freshness: &mut Value) {
    if let Some(notice) = freshness.get("notice").and_then(Value::as_str) {
        let updated = format!(
            "{notice} (Pattern contains regex syntax with regex=false; set regex=true if a regular expression was intended.)"
        );
        freshness["notice"] = Value::String(updated);
    }
}
