use code_context::catalog::Spec;
use code_context::query::Query;
use code_context::watched::Watched;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn fixture() -> (tempfile::TempDir, Watched) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("src")).unwrap();
    std::fs::write(
        directory.path().join("src/a.rs"),
        "alpha\nneedle\nHELLO\n한글\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("src/b.rs"), "needle two\nhello\n").unwrap();
    let watched = Watched::new(Spec {
        root: directory.path().canonicalize().unwrap(),
        scopes: vec!["src".into()],
        globs: vec!["*.rs".into()],
        excludes: Vec::new(),
        default_excludes: true,
        prewarm: false,
        policy: Default::default(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();
    (directory, watched)
}

fn query(watched: &mut Watched, pattern: &str) -> Value {
    watched
        .call("code_grep", json!({"pattern":pattern}))
        .unwrap()
}

fn wait_for(watched: &mut Watched, pattern: &str, count: u64) {
    let start = Instant::now();
    loop {
        let found = match watched.call("code_grep", json!({"pattern":pattern})) {
            Ok(found) => found,
            Err(error) if error.to_string().contains("STALE_INDEX") => {
                assert!(start.elapsed() < Duration::from_secs(4), "{error}");
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(error) => panic!("{error}"),
        };
        if found["total_matches"] == count {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "watcher did not converge: wanted {count}, got {found}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn watcher_applies_modify_create_delete_and_rename() {
    let (dir, mut watched) = fixture();
    std::fs::write(dir.path().join("src/a.rs"), "fresh\n").unwrap();
    wait_for(&mut watched, "fresh", 1);
    std::fs::write(dir.path().join("src/new.rs"), "fresh\n").unwrap();
    wait_for(&mut watched, "fresh", 2);
    std::fs::rename(
        dir.path().join("src/new.rs"),
        dir.path().join("src/renamed.rs"),
    )
    .unwrap();
    watched.call("code_sync", json!({})).unwrap();
    let result = query(&mut watched, "fresh");
    assert!(
        result["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["path"] == "src/renamed.rs")
    );
    std::fs::remove_file(dir.path().join("src/a.rs")).unwrap();
    wait_for(&mut watched, "fresh", 1);
}

#[test]
fn watcher_discovers_new_nested_directories_without_recursive_dependency_watches() {
    let (dir, mut watched) = fixture();
    std::fs::create_dir_all(dir.path().join("src/new/deep")).unwrap();
    std::fs::write(dir.path().join("src/new/deep/new.rs"), "nested_marker\n").unwrap();
    wait_for(&mut watched, "nested_marker", 1);
    let status = watched.call("code_status", json!({})).unwrap();
    assert!(status["watched_directories"].as_u64().unwrap() >= 4);
}

#[test]
fn code_read_recovers_new_file_without_user_sync_or_retry() {
    let (dir, mut watched) = fixture();
    std::fs::write(dir.path().join("src/immediate.rs"), "first\nsecond\n").unwrap();
    let result = watched
        .call(
            "code_read",
            json!({"ranges":[{"path":"src/immediate.rs","start":1,"end":2}]}),
        )
        .unwrap();
    assert_eq!(result[0]["lines"], json!(["first", "second"]));
    let status = watched.call("code_status", json!({})).unwrap();
    assert!(status["transparent_read_recoveries"].as_u64().unwrap() <= 1);
    assert_eq!(status["transparent_read_recovery_failures"], 0);
}

#[test]
fn code_read_waits_through_a_short_atomic_save_gap() {
    let (dir, mut watched) = fixture();
    let path = dir.path().join("src/delayed.rs");
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        std::fs::write(path, "arrived\n").unwrap();
    });
    let result = watched
        .call(
            "code_read",
            json!({"ranges":[{"path":"src/delayed.rs","start":1,"end":1}]}),
        )
        .unwrap();
    writer.join().unwrap();
    assert_eq!(result[0]["lines"], json!(["arrived"]));
    let status=watched.call("code_status",json!({})).unwrap();
    assert!(status["transparent_read_recoveries"].as_u64().unwrap()<=1);
    assert_eq!(status["transparent_read_recovery_failures"],0);
}

#[test]
fn code_read_automatic_recovery_does_not_bypass_exclusions() {
    let (dir, mut watched) = fixture();
    std::fs::write(dir.path().join("src/ignored.rs"), "secret\n").unwrap();
    std::fs::write(dir.path().join(".ignore"), "src/ignored.rs\n").unwrap();
    let error = watched
        .call(
            "code_read",
            json!({"ranges":[{"path":"src/ignored.rs","start":1,"end":1}]}),
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unavailable after automatic recovery")
    );
    assert_eq!(
        watched.call("code_status", json!({})).unwrap()["transparent_read_recoveries"],
        0
    );
    assert_eq!(
        watched.call("code_status", json!({})).unwrap()["transparent_read_recovery_failures"],
        1
    );
}

#[test]
fn insertion_and_id_reuse_preserve_path_order() {
    let (dir, mut watched) = fixture();
    std::fs::remove_file(dir.path().join("src/a.rs")).unwrap();
    watched.call("code_sync", json!({})).unwrap();
    std::fs::write(dir.path().join("src/zero.rs"), "needle\n").unwrap();
    std::fs::write(dir.path().join("src/aaa.rs"), "needle\n").unwrap();
    watched.call("code_sync", json!({})).unwrap();
    let result = query(&mut watched, "needle");
    let paths: Vec<_> = result["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["src/aaa.rs", "src/b.rs", "src/zero.rs"]);
}

#[test]
fn ignore_policy_change_removes_and_restores_indexed_content() {
    let (dir, mut watched) = fixture();
    std::fs::write(dir.path().join(".ignore"), "src/a.rs\n").unwrap();
    wait_for(&mut watched, "needle", 1);
    assert!(
        watched
            .call(
                "code_read",
                json!({"ranges":[{"path":"src/a.rs","start":1,"end":1}]})
            )
            .is_err()
    );
    std::fs::write(dir.path().join(".ignore"), "").unwrap();
    wait_for(&mut watched, "needle", 2);
}

#[test]
fn failed_update_blocks_source_results_until_repaired() {
    let (dir, mut watched) = fixture();
    std::fs::write(dir.path().join("src/a.rs"), [255, 254]).unwrap();
    assert!(watched.call("code_sync", json!({})).is_err());
    assert!(
        watched
            .call("code_grep", json!({"pattern":"needle"}))
            .is_err()
    );
    std::fs::write(dir.path().join("src/a.rs"), "repaired\n").unwrap();
    watched.call("code_sync", json!({})).unwrap();
    assert_eq!(query(&mut watched, "repaired")["total_matches"], 1);
}

#[test]
fn regex_case_prefix_glob_and_context_have_defined_semantics() {
    let (_dir, mut watched) = fixture();
    let result = watched
        .call(
            "code_grep",
            json!({"pattern":"^hello$","regex":true,"ignore_case":true}),
        )
        .unwrap();
    assert_eq!(result["total_matches"], 2);
    let result = watched
        .call(
            "code_grep",
            json!({"pattern":"needle","paths":["src/a.rs"]}),
        )
        .unwrap();
    assert_eq!(result["total_matches"], 1);
    let result = watched
        .call("code_grep", json!({"pattern":"needle","glob":"a.rs"}))
        .unwrap();
    assert_eq!(result["total_matches"], 1);
    let result = watched
        .call(
            "code_context",
            json!({"pattern":"NEEDLE","ignore_case":true,"radius":1,"paths":["src/b.rs"]}),
        )
        .unwrap();
    assert_eq!(
        result["snippets"][0]["lines"],
        json!(["needle two", "hello"])
    );
    for input in [
        json!({"pattern":"x","paths":["../"]}),
        json!({"pattern":"x","glob":"["}),
        json!({"pattern":"[","regex":true}),
        json!({"pattern":"x","unknown":true}),
    ] {
        assert!(watched.call("code_grep", input).is_err());
    }
}

#[test]
fn strict_barrier_noop_does_not_advance_generation() {
    let (_dir, mut watched) = fixture();
    let before = watched.call("code_status", json!({})).unwrap()["stats"]["generation"].clone();
    let after = watched.call("code_sync", json!({})).unwrap();
    assert_eq!(after["stats"]["generation"], before);
    assert_eq!(after["last_update"]["changed_files"], 0);
}

#[test]
fn changed_bytes_are_reported_and_corpus_hash_is_not_stale() {
    let (dir, mut watched) = fixture();
    std::fs::write(dir.path().join("src/a.rs"), "updated\n").unwrap();
    let after = watched.call("code_sync", json!({})).unwrap();
    assert_eq!(after["last_update"]["changed_files"], 1);
    assert_eq!(after["stats"]["source_sha256"], "");
    assert_eq!(after["stats"]["files"], 2);
}

#[test]
fn query_contract_rejects_invalid_and_oversized_input() {
    let (_dir, mut watched) = fixture();
    for input in [
        json!({"pattern":""}),
        json!({"pattern":"a\nb"}),
        json!({"pattern":"a","limit":0}),
        json!({"pattern":"a","limit":1001}),
        json!({"pattern":"a","paths":[".hidden"]}),
        json!({"pattern":"a","paths":["x".repeat(4097)]}),
    ] {
        assert!(watched.call("code_grep", input).is_err());
    }

    assert!(serde_json::from_value::<Query>(json!({"pattern":"x","unexpected":1})).is_err());
}

#[test]
fn loading_many_files_does_not_overflow_watcher_with_own_reads() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("src")).unwrap();
    for index in 0..1600 {
        std::fs::write(
            directory.path().join(format!("src/file{index}.rs")),
            "shared needle\n",
        )
        .unwrap();
    }
    let mut watched = Watched::new(Spec {
        root: directory.path().canonicalize().unwrap(),
        scopes: vec!["src".into()],
        globs: vec!["*.rs".into()],
        excludes: Vec::new(),
        default_excludes: true,
        prewarm: false,
        policy: Default::default(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(30));
    let status = watched.call("code_status", json!({})).unwrap();
    assert_eq!(
        status["last_update"],
        json!({}),
        "filtered loader reads must not force a second full verification"
    );
    assert_eq!(query(&mut watched, "needle")["total_matches"], 1600);
}

#[test]
fn incremental_index_agrees_with_live_rg_after_repeated_updates() {
    let (dir, mut watched) = fixture();
    for index in 0..24 {
        let path = dir.path().join(format!("src/changed{}.rs", index % 6));
        if index % 5 == 0 {
            if path.exists() {
                std::fs::remove_file(&path).unwrap();
            }
        } else {
            std::fs::write(path, format!("repeat needle {index}\n")).unwrap();
        }
        watched.call("code_sync", json!({})).unwrap();
        let workspace = watched.spec.workspace().unwrap();
        let expected = code_context::search::search(
            &workspace,
            &code_context::search::Search {
                pattern: "needle".into(),
                regex: false,
                ignore_case: false,
                limit: 1000,
            },
        )
        .unwrap();
        let actual = watched
            .call("code_grep", json!({"pattern":"needle","limit":1000}))
            .unwrap();
        assert_eq!(
            actual["matches"],
            serde_json::to_value(expected.matches).unwrap()
        );
        assert_eq!(actual["total_matches"], expected.total_matches);
    }
}

#[test]
fn stale_background_reconciliation_cannot_overwrite_newer_generation() {
    let (dir, mut watched) = fixture();
    watched.schedule_maintenance();
    std::fs::write(dir.path().join("src/a.rs"), "new_epoch_marker\n").unwrap();
    watched.call("code_sync", json!({})).unwrap();
    std::thread::sleep(Duration::from_millis(10));
    assert_eq!(query(&mut watched, "new_epoch_marker")["total_matches"], 1);
}

#[test]
fn context_cache_is_bounded_and_generation_aware() {
    let (dir, mut watched) = fixture();
    let request = json!({"pattern":"needle","radius":1,"limit":8});
    let first = watched.call("code_context", request.clone()).unwrap();
    let second = watched.call("code_context", request.clone()).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        watched.call("code_status", json!({})).unwrap()["query_cache"]["context_entries"],
        1
    );
    std::fs::write(dir.path().join("src/a.rs"), "changed needle\n").unwrap();
    watched.call("code_sync", json!({})).unwrap();
    let next = watched.call("code_context", request).unwrap();
    assert_ne!(first, next);
    assert_eq!(
        watched.call("code_status", json!({})).unwrap()["query_cache"]["context_entries"],
        1
    );
}
