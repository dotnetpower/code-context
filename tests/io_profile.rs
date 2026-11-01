use code_context::catalog::Spec;
use code_context::watched::Watched;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn fixture() -> (tempfile::TempDir, Watched) {
    let root = tempfile::tempdir().unwrap();
    for directory in ["src", "unrelated", "docs"] {
        std::fs::create_dir(root.path().join(directory)).unwrap();
    }
    std::fs::write(root.path().join(".ignore"), "src/ignored.rs\n").unwrap();
    std::fs::write(root.path().join("src/ignored.rs"), "excluded\n").unwrap();
    std::fs::write(root.path().join("src/a.rs"), "original\n").unwrap();
    for index in 0..32 {
        std::fs::write(
            root.path().join(format!("unrelated/{index}.rs")),
            "untouched\n",
        )
        .unwrap();
    }
    let watched = Watched::new(Spec {
        root: root.path().into(),
        scopes: vec![".".into()],
        globs: vec!["*.rs".into()],
        excludes: vec!["secrets/**".into()],
        default_excludes: true,
        prewarm: false,
        policy: Default::default(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();
    (root, watched)
}

fn profile(watched: &mut Watched) -> Value {
    watched.call("code_status", json!({})).unwrap()["io_profile"].clone()
}

#[test]
fn read_policy_compiles_once_and_rechecks_configuration_and_ignore_changes() {
    let (root,mut watched)=fixture();
    let request=json!({"ranges":[{"path":"src/a.rs","start":1,"end":1}]});
    for _ in 0..8 {
        assert_eq!(watched.call("code_read",request.clone()).unwrap()[0]["lines"],json!(["original"]));
    }
    let counters=profile(&mut watched);
    assert_eq!(counters["read_policy_compilations"],1);
    assert_eq!(counters["read_policy_cache_hits"],7);
    let original=watched.spec.excludes.clone();
    watched.spec.excludes.push("src/a.rs".into());
    let denied=watched.call("code_read",request.clone()).unwrap_err().to_string();
    assert!(denied.contains("READ_POLICY_DENIED"),"{denied}");
    watched.spec.excludes=original;
    assert!(watched.call("code_read",request.clone()).is_ok());
    assert_eq!(profile(&mut watched)["read_policy_compilations"],3);
    std::fs::write(root.path().join(".ignore"),"src/ignored.rs\nsrc/a.rs\n").unwrap();
    watched.call("code_sync",json!({})).unwrap();
    let ignored=watched.call("code_read",request.clone()).unwrap_err().to_string();
    assert!(ignored.contains("READ_UNAVAILABLE"),"{ignored}");
    assert_eq!(profile(&mut watched)["read_policy_compilations"],3);
    std::fs::write(root.path().join(".ignore"),"src/ignored.rs\n").unwrap();
    watched.call("code_sync",json!({})).unwrap();
    assert_eq!(watched.call("code_read",request).unwrap()[0]["lines"],json!(["original"]));
    assert_eq!(profile(&mut watched)["read_policy_compilations"],3);
}

fn wait(watched: &mut Watched, marker: &str, count: usize) {
    let started = Instant::now();
    loop {
        let value = match watched.call("code_grep", json!({"pattern":marker})) {
            Ok(value) => value,
            Err(error) if error.to_string().contains("STALE_INDEX") => {
                assert!(started.elapsed() < Duration::from_secs(4), "{error}");
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(error) => panic!("{error}"),
        };
        if value["total_matches"] == count {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "watcher did not converge: {value}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn single_edit_does_not_read_other_source_or_enumerate_the_whole_root() {
    let (root, mut watched) = fixture();
    let before = profile(&mut watched);
    std::fs::write(root.path().join("src/a.rs"), "new_marker\n").unwrap();
    wait(&mut watched, "new_marker", 1);
    let after = profile(&mut watched);
    assert_eq!(
        after["full_inventory_scans"],
        before["full_inventory_scans"]
    );
    assert_eq!(
        after["full_source_verifications"],
        before["full_source_verifications"]
    );
    assert_eq!(
        after["source_files_read"].as_u64().unwrap()
            - before["source_files_read"].as_u64().unwrap(),
        1
    );
    assert_eq!(
        after["source_bytes_read"].as_u64().unwrap()
            - before["source_bytes_read"].as_u64().unwrap(),
        11
    );
}

#[test]
fn create_delete_and_atomic_save_use_directory_membership_not_global_inventory() {
    let (root, mut watched) = fixture();
    let before = profile(&mut watched);
    std::fs::write(root.path().join("src/new.rs"), "created_marker\n").unwrap();
    wait(&mut watched, "created_marker", 1);
    std::fs::write(root.path().join("src/atomic.tmp"), "atomic_marker\n").unwrap();
    std::fs::rename(
        root.path().join("src/atomic.tmp"),
        root.path().join("src/a.rs"),
    )
    .unwrap();
    wait(&mut watched, "atomic_marker", 1);
    std::fs::remove_file(root.path().join("src/new.rs")).unwrap();
    wait(&mut watched, "created_marker", 0);
    let after = profile(&mut watched);
    assert_eq!(
        after["full_inventory_scans"],
        before["full_inventory_scans"]
    );
    assert_eq!(
        after["full_source_verifications"],
        before["full_source_verifications"]
    );
    assert!(
        after["directory_inventory_scans"].as_u64().unwrap()
            > before["directory_inventory_scans"].as_u64().unwrap()
    );
    assert!(
        after["source_files_read"].as_u64().unwrap()
            - before["source_files_read"].as_u64().unwrap()
            < 8
    );
}

#[test]
fn directory_moves_discover_destinations_even_in_unwatched_parents() {
    let (root, mut watched) = fixture();
    let before = profile(&mut watched);
    std::fs::create_dir_all(root.path().join("src/nested/deep")).unwrap();
    std::fs::write(root.path().join("src/nested/deep/a.rs"), "nested_marker\n").unwrap();
    wait(&mut watched, "nested_marker", 1);
    std::fs::rename(
        root.path().join("src/nested"),
        root.path().join("docs/moved"),
    )
    .unwrap();
    let started = Instant::now();
    loop {
        let result = match watched.call("code_grep", json!({"pattern":"nested_marker"})) {
            Ok(result) => result,
            Err(error) if error.to_string().contains("STALE_INDEX") => {
                assert!(started.elapsed() < Duration::from_secs(4), "{error}");
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(error) => panic!("{error}"),
        };
        if result["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hit| hit["path"] == "docs/moved/deep/a.rs")
        {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "rename destination was not discovered: {result}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::remove_file(root.path().join("docs/moved/deep/a.rs")).unwrap();
    std::fs::remove_dir(root.path().join("docs/moved/deep")).unwrap();
    std::fs::remove_dir(root.path().join("docs/moved")).unwrap();
    wait(&mut watched, "nested_marker", 0);
    let after = profile(&mut watched);
    assert!(
        after["full_inventory_scans"].as_u64().unwrap()
            <= before["full_inventory_scans"].as_u64().unwrap() + 1
    );
    assert_eq!(
        after["full_source_verifications"],
        before["full_source_verifications"]
    );
}

#[test]
fn excluded_or_unsupported_reads_do_not_hash_or_reload_the_corpus() {
    let (root, mut watched) = fixture();
    std::fs::write(root.path().join("docs/not-indexed.md"), "unsupported\n").unwrap();
    let before = profile(&mut watched);
    for path in [
        "src/ignored.rs",
        "docs/not-indexed.md",
        ".github/private.rs",
        "secrets/key.rs",
    ] {
        assert!(
            watched
                .call(
                    "code_read",
                    json!({"ranges":[{"path":path,"start":1,"end":1}]})
                )
                .is_err()
        );
    }
    let after = profile(&mut watched);
    assert_eq!(
        after["full_inventory_scans"],
        before["full_inventory_scans"]
    );
    assert_eq!(
        after["full_source_verifications"],
        before["full_source_verifications"]
    );
    assert_eq!(after["source_files_read"], before["source_files_read"]);
    assert!(after["read_policy_rejections"].as_u64().unwrap() >= 4);
}

#[test]
fn immediate_read_recovery_only_loads_requested_new_file_and_sync_still_verifies_all() {
    let (root, mut watched) = fixture();
    let before = profile(&mut watched);
    std::fs::write(root.path().join("src/immediate.rs"), "immediate_marker\n").unwrap();
    let result = watched
        .call(
            "code_read",
            json!({"ranges":[{"path":"src/immediate.rs","start":1,"end":1}]}),
        )
        .unwrap();
    assert_eq!(result[0]["lines"], json!(["immediate_marker"]));
    let recovered = profile(&mut watched);
    assert_eq!(
        recovered["full_source_verifications"],
        before["full_source_verifications"]
    );
    assert_eq!(
        recovered["full_inventory_scans"],
        before["full_inventory_scans"]
    );
    assert!(
        recovered["source_files_read"].as_u64().unwrap()
            - before["source_files_read"].as_u64().unwrap()
            <= 3
    );
    let status = watched.call("code_sync", json!({})).unwrap();
    assert_eq!(
        status["io_profile"]["full_source_verifications"]
            .as_u64()
            .unwrap(),
        recovered["full_source_verifications"].as_u64().unwrap() + 1
    );
    assert_eq!(
        status["last_update"]["source_files_read"],
        status["stats"]["files"]
    );
    assert_eq!(status["reconcile_interval_seconds"], 60);
}

#[test]
fn ignore_policy_changes_revoke_membership_without_reopening_unchanged_source() {
    let (root, mut watched) = fixture();
    let before = profile(&mut watched);
    std::fs::write(root.path().join(".ignore"), "src/*.rs\n").unwrap();
    wait(&mut watched, "original", 0);
    let after = profile(&mut watched);
    assert!(
        after["full_inventory_scans"].as_u64().unwrap()
            > before["full_inventory_scans"].as_u64().unwrap()
    );
    assert_eq!(
        after["full_source_verifications"],
        before["full_source_verifications"]
    );
    assert_eq!(after["source_files_read"], before["source_files_read"]);
}

#[test]
fn broad_read_is_explicitly_split_and_clamped_without_silent_partial_success() {
    let (root, mut watched) = fixture();
    let text = (0..1100).map(|i| format!("line{i}\n")).collect::<String>();
    std::fs::write(root.path().join("src/broad.rs"), text).unwrap();
    let value = watched
        .call(
            "code_read",
            json!({"ranges":[{"path":"src/broad.rs","start":1,"end":5000}]}),
        )
        .unwrap();
    assert_eq!(value.as_array().unwrap().len(), 3);
    assert_eq!(value[0]["start"], 1);
    assert_eq!(value[0]["end"], 500);
    assert_eq!(value[1]["start"], 501);
    assert_eq!(value[1]["end"], 1000);
    assert_eq!(value[2]["start"], 1001);
    assert_eq!(value[2]["end"], 1100);
    assert!(watched.call("code_read",json!({"split_ranges":false,"ranges":[{"path":"src/broad.rs","start":1,"end":5000}]})).is_err());
    assert_eq!(
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|snippet| snippet["lines"].as_array().unwrap().len())
            .sum::<usize>(),
        1100
    );
}

#[test]
fn explicit_read_pages_cover_large_content_once_and_stay_under_the_response_cap() {
    let (root, mut watched) = fixture();
    let text = (0..1200)
        .map(|i| format!("{i:04} {}\n", "x".repeat(235)))
        .collect::<String>();
    std::fs::write(root.path().join("src/paged.rs"), text).unwrap();
    let original = json!([{"path":"src/paged.rs","start":1,"end":1200}]);
    let error = watched
        .call("code_read", json!({"ranges":original.clone()}))
        .unwrap_err();
    assert!(error.to_string().contains("RESPONSE_BUDGET_EXCEEDED"));
    let mut ranges = original;
    let mut all = Vec::new();
    let mut pages = 0;
    loop {
        let result = watched
            .call("code_read", json!({"page":true,"ranges":ranges}))
            .unwrap();
        assert!(serde_json::to_vec(&result).unwrap().len() <= 256 * 1024);
        pages += 1;
        for snippet in result["snippets"].as_array().unwrap() {
            all.extend(
                snippet["lines"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|line| line.as_str().unwrap().to_owned()),
            );
        }
        if result["complete"] == true {
            break;
        }
        ranges = result["next_ranges"].clone();
        assert!(pages < 8);
    }
    assert!(pages > 1);
    assert_eq!(all.len(), 1200);
    for (index, line) in all.iter().enumerate() {
        assert!(line.starts_with(&format!("{index:04} ")));
    }
    let long_text = (0..300)
        .map(|i| format!("{i:04} {}\n", "x".repeat(1590)))
        .collect::<String>();
    std::fs::write(root.path().join("src/long-lines.rs"), long_text).unwrap();
    let first = watched
        .call(
            "code_read",
            json!({"page":true,"ranges":[{"path":"src/long-lines.rs","start":1,"end":300}]}),
        )
        .unwrap();
    assert_eq!(first["complete"], false);
    assert!(serde_json::to_vec(&first).unwrap().len() <= 256 * 1024);
    let second = watched
        .call(
            "code_read",
            json!({"page":true,"ranges":first["next_ranges"]}),
        )
        .unwrap();
    assert_eq!(second["complete"], true);
}
