use code_context::catalog::Spec;
use code_context::policy::MemoryPolicy;
use code_context::resident::Resident;
use code_context::search::Search;
use code_context::watched::Watched;
use code_context::workspace::Workspace;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn policy() -> MemoryPolicy {
    MemoryPolicy {
        max_files: 100,
        max_source_bytes: 1024,
        max_file_bytes: 1024,
        max_total_lines: 100,
        max_total_path_bytes: 1024,
        max_unique_trigrams_per_file: 100,
        max_unique_trigrams: 100,
        max_changed_files: 10,
        max_staged_bytes: 1024,
        max_watch_dirs: 100,
    }
}

#[test]
fn cold_load_checks_whole_inventory_before_reading_source() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "a".repeat(128)).unwrap();
    std::fs::write(root.path().join("z.rs"), "z".repeat(512)).unwrap();
    let workspace = Workspace::new(root.path(), vec![".".into()], vec!["*.rs".into()]).unwrap();
    let inventory = workspace.files_limited(&MemoryPolicy::default()).unwrap();
    let expected = 640 + inventory.total_path_bytes * 2 + 2 * 128;
    let error = Resident::load_with_policy(
        &workspace, true, 1, MemoryPolicy::default(), 64,
    ).err().unwrap().to_string();
    assert!(error.contains("by source inventory"));
    assert!(error.contains(&format!("required_bytes={expected}")), "{error}");
}

#[test]
fn inventory_rejects_tiny_file_flood_and_path_amplification() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..32 {
        std::fs::write(root.path().join(format!("tiny-{index:04}.rs")), "").unwrap();
    }
    let workspace = Workspace::new(root.path(), vec![".".into()], vec!["*.rs".into()]).unwrap();
    let mut limited = policy();
    limited.max_files = 31;
    assert!(
        workspace
            .files_limited(&limited)
            .err()
            .unwrap()
            .to_string()
            .contains("max_files")
    );
    limited.max_files = 100;
    limited.max_total_path_bytes = 64;
    assert!(
        workspace
            .files_limited(&limited)
            .err()
            .unwrap()
            .to_string()
            .contains("max_total_path_bytes")
    );
}

#[test]
fn inventory_rejects_per_file_and_aggregate_source_limits() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "123456").unwrap();
    std::fs::write(root.path().join("b.rs"), "123456").unwrap();
    let workspace = Workspace::new(root.path(), vec![".".into()], vec!["*.rs".into()]).unwrap();
    let mut limited = policy();
    limited.max_file_bytes = 5;
    assert!(
        workspace
            .files_limited(&limited)
            .err()
            .unwrap()
            .to_string()
            .contains("max_file_bytes")
    );
    limited.max_file_bytes = 10;
    limited.max_source_bytes = 10;
    limited.max_staged_bytes = 10;
    assert!(
        workspace
            .files_limited(&limited)
            .err()
            .unwrap()
            .to_string()
            .contains("max_source_bytes")
    );
}

#[test]
fn resident_rejects_newline_and_trigram_amplification() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("lines.rs"), "\n\n\n\n\n").unwrap();
    let workspace = Workspace::new(root.path(), vec![".".into()], vec!["*.rs".into()]).unwrap();
    let mut limited = policy();
    limited.max_total_lines = 4;
    assert!(
        Resident::load_with_policy(&workspace, true, 1, limited, 1024 * 1024)
            .err()
            .unwrap()
            .to_string()
            .contains("line count")
    );

    std::fs::write(root.path().join("lines.rs"), "abcdefghijk").unwrap();
    let mut limited = policy();
    limited.max_unique_trigrams_per_file = 3;
    assert!(
        Resident::load_with_policy(&workspace, true, 1, limited, 1024 * 1024)
            .err()
            .unwrap()
            .to_string()
            .contains("max_unique_trigrams_per_file")
    );
}

#[test]
fn resident_admission_budget_is_enforced_during_load() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("large.rs"), "x".repeat(512)).unwrap();
    let workspace = Workspace::new(root.path(), vec![".".into()], vec!["*.rs".into()]).unwrap();
    assert!(
        Resident::load_with_policy(&workspace, true, 1, policy(), 128)
            .err()
            .unwrap()
            .to_string()
            .contains("admission budget")
    );
}

#[test]
fn scopes_are_canonicalized_and_excludes_remove_generated_trees() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src/nested")).unwrap();
    std::fs::create_dir(root.path().join("vendor")).unwrap();
    std::fs::write(root.path().join("src/a.rs"), "source").unwrap();
    std::fs::write(root.path().join("vendor/generated.rs"), "generated").unwrap();
    let workspace = Workspace::new_with_excludes(
        root.path(),
        vec!["src/nested".into(), "src".into(), "src".into()],
        vec!["*.rs".into(), "*.rs".into()],
        vec!["vendor/**".into()],
        true,
    )
    .unwrap();
    assert_eq!(workspace.scopes, ["src"]);
    assert_eq!(workspace.globs, ["*.rs"]);

    let broad = Workspace::new_with_excludes(
        root.path(),
        vec![".".into()],
        vec!["*.rs".into()],
        vec!["vendor/**".into()],
        true,
    )
    .unwrap();
    let files = broad.files_limited(&policy()).unwrap().files;
    assert!(files.contains(std::path::Path::new("src/a.rs")));
    assert!(!files.contains(std::path::Path::new("vendor/generated.rs")));
}

#[test]
fn workspace_configuration_has_count_and_byte_caps() {
    let root = tempfile::tempdir().unwrap();
    let too_many = (0..129).map(|i| format!("*.{i}")).collect();
    assert!(Workspace::new(root.path(), vec![".".into()], too_many).is_err());
    assert!(
        Workspace::new_with_excludes(
            root.path(),
            vec![".".into()],
            vec!["*.rs".into()],
            vec!["x".repeat(1025)],
            true,
        )
        .is_err()
    );
}

#[test]
fn legacy_registry_specs_enable_new_safety_defaults() {
    let root = tempfile::tempdir().unwrap();
    let spec: Spec = serde_json::from_value(json!({
        "root": root.path(),
        "scopes": ["."],
        "globs": ["*.rs"],
        "prewarm": false,
        "policy": {}
    }))
    .unwrap();
    assert!(spec.default_excludes);
    assert_eq!(spec.policy.max_watch_dirs, 50_000);
}

#[test]
fn default_exclude_opt_out_is_persisted_explicitly() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let root = directory.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("source.rs"), "source").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &state)
        .args([
            "--root",
            root.to_str().unwrap(),
            "--scope",
            ".",
            "--glob",
            "*.rs",
            "--no-default-excludes",
            "register",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let registry: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state.join("workspaces.json")).unwrap()).unwrap();
    assert_eq!(registry[0]["default_excludes"], false);
}

#[test]
fn broad_scope_cannot_be_registered_for_prewarm() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let root = directory.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &state)
        .args([
            "--root",
            root.to_str().unwrap(),
            "--scope",
            ".",
            "--glob",
            "*.rs",
            "--prewarm",
            "register",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires --allow-root-scope"));
}

#[test]
fn plan_does_not_persist_a_registry_entry() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let root = directory.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.rs"), "source").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &state)
        .args([
            "--root",
            root.to_str().unwrap(),
            "--scope",
            ".",
            "--glob",
            "*.rs",
            "--prewarm",
            "--max-source-mib",
            "1",
            "plan",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["files"], 1);
    assert_eq!(value["prewarm"], true);
    assert_eq!(value["policy"]["max_staged_bytes"], 1024 * 1024);
    assert!(!state.join("workspaces.json").exists());
}

#[test]
fn oversized_registry_is_rejected_without_unbounded_read() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(state.join("workspaces.json"), vec![b' '; 1024 * 1024 + 1]).unwrap();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &state)
        .arg("daemon")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !state.join("daemon.sock").exists() {
        assert!(started.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &state)
        .arg("workspaces")
        .output()
        .unwrap();
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("registry exceeds 1 MiB"));
}

#[test]
fn changed_file_transaction_limit_retains_previous_generation() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/a.rs"), "old-a").unwrap();
    std::fs::write(root.path().join("src/b.rs"), "old-b").unwrap();
    let mut limited = policy();
    limited.max_changed_files = 1;
    let mut watched = Watched::new(Spec {
        root: root.path().to_path_buf(),
        scopes: vec!["src".into()],
        globs: vec!["*.rs".into()],
        excludes: Vec::new(),
        default_excludes: true,
        prewarm: false,
        policy: limited,
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();
    std::fs::write(root.path().join("src/a.rs"), "new-a").unwrap();
    std::fs::write(root.path().join("src/b.rs"), "new-b").unwrap();
    assert!(watched.call("code_sync", json!({})).is_err());
    std::fs::write(root.path().join("src/a.rs"), "old-a").unwrap();
    std::fs::write(root.path().join("src/b.rs"), "old-b").unwrap();
    watched.call("code_sync", json!({})).unwrap();
    let status = watched.call("code_status", json!({})).unwrap();
    assert_eq!(status["stats"]["generation"], 1);
}

#[test]
fn default_excluded_tree_event_flood_does_not_force_reconciliation() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::create_dir_all(root.path().join("node_modules/pkg")).unwrap();
    std::fs::write(root.path().join("src/a.rs"), "source").unwrap();
    let mut watched = Watched::new(Spec {
        root: root.path().to_path_buf(),
        scopes: vec![".".into()],
        globs: vec!["*.rs".into()],
        excludes: Vec::new(),
        default_excludes: true,
        prewarm: false,
        policy: policy(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();
    for index in 0..5000 {
        std::fs::write(
            root.path()
                .join(format!("node_modules/pkg/generated-{index}.rs")),
            "generated",
        )
        .unwrap();
    }
    let status = watched.call("code_status", json!({})).unwrap();
    assert_eq!(status["last_update"], json!({}));
    assert_eq!(
        watched
            .call("code_grep", json!({"pattern":"generated"}))
            .unwrap()["total_matches"],
        0
    );
}

#[test]
fn compiled_matcher_cache_is_strictly_bounded() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/a.rs"), "needle xxxxx").unwrap();
    let mut watched = Watched::new(Spec {
        root: root.path().to_path_buf(),
        scopes: vec!["src".into()],
        globs: vec!["*.rs".into()],
        excludes: Vec::new(),
        default_excludes: true,
        prewarm: false,
        policy: policy(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();
    for width in 1..=32 {
        watched
            .call(
                "code_grep",
                json!({"pattern":format!("needle x{{{width}}}"),"regex":true}),
            )
            .unwrap();
    }

    let status = watched.call("code_status", json!({})).unwrap();
    assert_eq!(status["query_cache"]["entries"], 8);
    assert_eq!(status["query_cache"]["entry_limit"], 8);
    assert!(
        status["query_cache"]["pattern_bytes"].as_u64().unwrap()
            <= status["query_cache"]["pattern_byte_limit"]
                .as_u64()
                .unwrap()
    );
}

#[test]
fn result_cache_accounts_for_query_key_memory() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/a.rs"), "needle").unwrap();
    let mut watched = Watched::new(Spec {
        root: root.path().to_path_buf(),
        scopes: vec!["src".into()],
        globs: vec!["*.rs".into()],
        excludes: Vec::new(),
        default_excludes: true,
        prewarm: false,
        policy: policy(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();
    for index in 0..32 {
        watched
            .call(
                "code_grep",
                json!({"pattern":"needle","paths":[format!("path-{index}-{}", "x".repeat(3000))]}),
            )
            .unwrap();
    }
    let status = watched.call("code_status", json!({})).unwrap();
    assert!(status["query_cache"]["result_bytes"].as_u64().unwrap() > 96_000);
    assert!(
        status["query_cache"]["result_bytes"].as_u64().unwrap()
            <= status["query_cache"]["result_byte_limit"].as_u64().unwrap()
    );
}

#[test]
fn dense_trigram_builder_preserves_large_corpus_results() {
    let root = tempfile::tempdir().unwrap();
    let line_count = (16 * 1024 * 1024) / 4 + 1;
    let bytes = "abc\n".repeat(line_count);
    std::fs::write(root.path().join("large.rs"), bytes).unwrap();
    let workspace = Workspace::new(root.path(), vec![".".into()], vec!["*.rs".into()]).unwrap();
    let mut limited = policy();
    limited.max_source_bytes = 18 * 1024 * 1024;
    limited.max_file_bytes = 18 * 1024 * 1024;
    limited.max_staged_bytes = 18 * 1024 * 1024;
    limited.max_total_lines = 5_000_000;
    let resident =
        Resident::load_with_policy(&workspace, true, 1, limited, 128 * 1024 * 1024).unwrap();
    let found = resident
        .search(&Search {
            pattern: "abc".into(),
            regex: false,
            ignore_case: false,
            limit: 8,
        })
        .unwrap();
    assert_eq!(found.total_matches, line_count);
    assert_eq!(resident.candidate_stats("abc").0, 1);
}

#[test]
fn default_dependency_and_build_trees_are_excluded_from_files_and_watches() {
    let root = tempfile::tempdir().unwrap();
    for directory in [
        "src",
        "node_modules/pkg",
        "target/debug",
        "dist/assets",
        "build/generated",
        "coverage/data",
        "__pycache__",
        "venv/lib",
        "Pods/Package",
    ] {
        std::fs::create_dir_all(root.path().join(directory)).unwrap();
        std::fs::write(root.path().join(directory).join("source.rs"), "content").unwrap();
    }
    let workspace = Workspace::new(root.path(), vec![".".into()], vec!["*.rs".into()]).unwrap();
    let inventory = workspace.files_limited(&policy()).unwrap();
    assert_eq!(
        inventory.files,
        ["src/source.rs".into()].into_iter().collect()
    );
    let watches = workspace.watch_directories(&policy()).unwrap();
    assert_eq!(watches.len(), 2);
    assert!(
        workspace
            .effective_excludes()
            .iter()
            .any(|item| item.contains("node_modules"))
    );

    let unfiltered = Workspace::new_with_excludes(
        root.path(),
        vec![".".into()],
        vec!["*.rs".into()],
        Vec::new(),
        false,
    )
    .unwrap();
    let files = unfiltered
        .files_limited(&MemoryPolicy::default())
        .unwrap()
        .files;
    assert!(files.contains(std::path::Path::new("node_modules/pkg/source.rs")));
    assert!(files.contains(std::path::Path::new("target/debug/source.rs")));
}

#[test]
fn watch_directory_budget_rejects_directory_amplification() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..8 {
        let directory = root.path().join(format!("dir-{index}"));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("source.rs"), "").unwrap();
    }

    let workspace = Workspace::new(root.path(), vec![".".into()], vec!["*.rs".into()]).unwrap();
    let mut limited = policy();
    limited.max_watch_dirs = 4;
    assert!(
        workspace
            .watch_directories(&limited)
            .err()
            .unwrap()
            .to_string()
            .contains("max_watch_dirs")
    );
}

#[test]
fn gitignored_directories_do_not_consume_watches() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/source.rs"), "source").unwrap();
    std::fs::write(root.path().join(".gitignore"), "vendor/\n").unwrap();
    for index in 0..100 {
        let directory = root.path().join(format!("vendor/pkg-{index}"));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("ignored.rs"), "ignored").unwrap();
    }
    let workspace = Workspace::new(root.path(), vec![".".into()], vec!["*.rs".into()]).unwrap();
    assert_eq!(workspace.files_limited(&policy()).unwrap().files.len(), 1);
    assert_eq!(workspace.watch_directories(&policy()).unwrap().len(), 2);
}

#[test]
fn external_inventory_command_has_a_hard_timeout() {
    let started = Instant::now();
    let error = code_context::workspace::capture_with_timeout(
        Command::new("sh").args(["-c", "exec sleep 5"]),
        Duration::from_millis(50),
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(1));
}
