use serde_json::{Value, json};
use code_context::catalog::MAX_REGISTERED_ROOTS;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
    linked: PathBuf,
    daemon: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("state");
        let repo = dir.path().join("repo");
        let linked = dir.path().join("linked worktree");
        let git = |args: &[&str]| {
            let output = Command::new("git").args(args).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q", "--template=", repo.to_str().unwrap()]);
        std::fs::create_dir(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/a.rs"), "main branch\n").unwrap();
        std::fs::write(repo.join("README.md"), "documentation\n").unwrap();
        std::fs::write(repo.join("secrets.json"), "{}\n").unwrap();
        std::fs::write(repo.join(".gitignore"), "ignored.json\n").unwrap();
        git(&["-C", repo.to_str().unwrap(), "add", "."]);
        git(&[
            "-C",
            repo.to_str().unwrap(),
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ]);
        git(&[
            "-C",
            repo.to_str().unwrap(),
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ]);
        std::fs::write(linked.join("src/a.rs"), "linked branch\n").unwrap();
        let f = Self {
            _dir: dir,
            home,
            repo,
            linked,
            daemon: None,
        };
        f.cli(
            &f.repo,
            &[
                "--scope",
                "src",
                "--glob",
                "*.rs",
                "--exclude",
                "generated/**",
                "register",
            ],
        );
        f
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cctx"));
        command.env("CCTX_HOME", &self.home);
        command
    }

    fn cli(&self, root: &Path, args: &[&str]) -> Value {
        let output = self
            .command()
            .arg("--root")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn start(&mut self) {
        self.daemon = Some(
            self.command()
                .arg("daemon")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let start = Instant::now();
        while std::os::unix::net::UnixStream::connect(self.home.join("daemon.sock")).is_err() {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
        while !self
            .command()
            .arg("ping")
            .output()
            .unwrap()
            .status
            .success()
        {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn mcp(&self, root: &Path, name: &str, arguments: Value) -> Value {
        // Readiness is explicit here so these tests isolate policy/shape behavior.
        if matches!(
            name,
            "code_files" | "code_grep" | "code_context" | "code_read"
        ) {
            let ready = self.mcp(root, "code_sync", json!({}));
            if ready["isError"] == true {
                return ready;
            }
        }
        self.mcp_once(root, name, arguments)
    }

    fn mcp_once(&self, root: &Path, name: &str, arguments: Value) -> Value {
        let mut child = self
            .command()
            .current_dir(root)
            .arg("shared-mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(
            child.stdin.as_mut().unwrap(),
            "{}",
            json!({
                "jsonrpc":"2.0", "id":1, "method":"tools/call",
                "params":{"name":name,"arguments":arguments}
            })
        )
        .unwrap();
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["result"].clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.daemon {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }
}

#[test]
fn mcp_cold_miss_is_explicit_and_not_retried_or_reported_as_zero_matches() {
    let mut f = Fixture::new();
    f.start();
    let started = Instant::now();
    let miss = f.mcp_once(&f.repo, "code_files", json!({"glob":"*.rs"}));
    assert!(started.elapsed() < Duration::from_secs(2), "{miss}");
    assert_eq!(miss["isError"], true);
    let text = miss["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("CACHE_MISS "), "{text}");
    assert!(text.contains("permitted host"), "{text}");
    assert!(text.contains("not a zero-match"), "{text}");
    let status = f.mcp_once(&f.repo, "code_status", json!({}));
    let data: Value = serde_json::from_str(status["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(data["protocol_calls"]["bincode:code_files"], 1);
    assert!(data["protocol_failures"]["bincode:code_files"].is_null());
    assert_eq!(data["cache"]["misses"], 1);
    assert_eq!(data["cache"]["automatic_prewarm"], false);
    let deadline = Instant::now();
    loop {
        let status = f.cli(&f.repo, &["status"]);
        if status["data"]["index_state"] == "loaded" {
            break;
        }
        assert!(deadline.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(3));
    }
    let hit = f.mcp_once(&f.repo, "code_files", json!({"glob":"*.rs"}));
    assert_eq!(hit["isError"], false);
    let data: Value = serde_json::from_str(hit["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(data["total_matches"], 1);
}

#[test]
fn requested_worktree_loads_without_listing_or_preparing_siblings() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    f.cli(&f.repo, &["authorize-worktrees"]);
    let directory = f.home.parent().unwrap();
    let sibling = directory.join("unrequested-sibling");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&f.repo)
            .args(["worktree", "add", "-q", "-b", "unrequested"])
            .arg(&sibling)
            .status()
            .unwrap()
            .success()
    );
    let bin = directory.join("git-wrapper");
    std::fs::create_dir(&bin).unwrap();
    let git = bin.join("git");
    std::fs::write(&git,"#!/bin/sh\ncase \" $* \" in *\" worktree list \"*) echo 'sibling enumeration forbidden' >&2; exit 71;; esac\nexec /usr/bin/git \"$@\"\n").unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    f.daemon = Some(
        f.command()
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .arg("daemon")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    while !f.home.join("daemon.sock").exists() {
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(2));
    }
    let miss = f.mcp_once(&f.linked, "code_files", json!({"glob":"*.rs"}));
    assert!(
        miss["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("CACHE_MISS "),
        "{miss}"
    );
    let started = Instant::now();
    loop {
        let status = f.cli(&f.repo, &["status"]);
        if status["data"]["daemon_admission"]["root_loads"][f.linked.to_str().unwrap()] == 1 {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "{status}");
        std::thread::sleep(Duration::from_millis(3));
    }
    let hit = f.mcp_once(
        &f.linked,
        "code_read",
        json!({"ranges":[{"path":"src/a.rs","start":1,"end":1}]}),
    );
    assert_eq!(hit["isError"], false, "{hit}");
    let data: Value = serde_json::from_str(hit["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(data[0]["lines"], json!(["linked branch"]));
    let status = f.cli(&f.repo, &["status"]);
    let loads = status["data"]["daemon_admission"]["root_loads"]
        .as_object()
        .unwrap();
    assert_eq!(loads.len(), 1);
    assert!(!loads.contains_key(sibling.to_str().unwrap()));
    assert_eq!(f.cli(&f.repo, &["workspaces"]).as_array().unwrap().len(), 2);
    f.cli(&f.repo, &["authorize-worktrees", "--revoke"]);
    let revoked = f.mcp_once(&f.linked, "code_files", json!({"glob":"*.rs"}));
    assert_eq!(revoked["isError"], true);
    assert!(
        revoked["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("revoked"),
        "{revoked}"
    );
}

#[test]
fn slow_worktree_preparation_for_explicit_sync_does_not_block_a_ready_root() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    f.cli(&f.repo, &["authorize-worktrees"]);
    let directory = f.home.parent().unwrap();
    let bin = directory.join("slow-git");
    std::fs::create_dir(&bin).unwrap();
    let git = bin.join("git");
    std::fs::write(&git,"#!/bin/sh\ncase \" $* \" in *\" rev-parse \"*) printf active > \"$CCTX_TEST_GIT_MARKER\"; sleep 0.12;; *\" worktree list \"*) exit 71;; esac\nexec /usr/bin/git \"$@\"\n").unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    let marker = directory.join("git-active");
    f.daemon = Some(
        f.command()
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("CCTX_TEST_GIT_MARKER", &marker)
            .arg("daemon")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    while !f.home.join("daemon.sock").exists() {
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(2));
    }
    f.cli(&f.repo, &["ping"]);
    f.cli(&f.repo, &["sync"]);
    std::thread::scope(|scope| {
        let f = &f;
        let sync = scope.spawn(move || f.cli(&f.linked, &["sync"]));
        let started = Instant::now();
        while !marker.exists() {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
        let started = Instant::now();
        let hit = f.mcp_once(&f.repo, "code_grep", json!({"pattern":"main"}));
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "ready root was blocked by unrelated Git preparation"
        );
        assert_eq!(hit["isError"], false, "{hit}");
        let result = sync.join().unwrap();
        assert_eq!(result["data"]["last_update"]["verified_all_source"], true);
    });
}

#[test]
fn migration_requires_explicit_scopes_and_preserves_policy() {
    let mut f = Fixture::new();
    f.start();
    let before = f.cli(&f.repo, &["workspaces"])[0].clone();
    let preview = f.cli(&f.repo, &["migrate"]);
    assert_eq!(preview["after"]["scopes"], before["scopes"]);
    assert_eq!(f.cli(&f.repo, &["workspaces"])[0], before);
    let applied = f.cli(&f.repo, &["migrate", "--add-scope", ".", "--apply"]);
    assert_eq!(applied["after"]["scopes"], json!(["."]));
    let after = f.cli(&f.repo, &["workspaces"])[0].clone();
    for key in [
        "policy",
        "excludes",
        "prewarm",
        "default_excludes",
        "allow_worktrees",
    ] {
        assert_eq!(before[key], after[key], "{key}");
    }
    assert!(after["globs"].as_array().unwrap().contains(&json!("*.md")));
}

#[test]
fn mcp_only_prepares_authorized_worktree_without_using_parent_content() {
    let mut f = Fixture::new();
    f.start();
    let denied = f.mcp(&f.linked, "code_grep", json!({"pattern":"branch"}));
    assert_eq!(denied["isError"], true);
    assert!(
        denied["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("authorize")
    );
    f.cli(&f.repo, &["authorize-worktrees"]);
    let candidate_snapshot = f.cli(&f.repo, &["coverage"]);
    let candidates = candidate_snapshot["preparation_candidates"]["roots"]
        .as_array()
        .unwrap();
    assert!(
        candidates
            .iter()
            .any(|entry| entry["root"] == json!(f.linked))
    );
    let result = f.mcp(
        &f.linked,
        "code_read",
        json!({"ranges":[{"path":"src/a.rs","start":1,"end":1}]}),
    );
    assert_eq!(result["isError"], false, "{result}");
    let read: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(read[0]["lines"], json!(["linked branch"]));
    let main = f.mcp(
        &f.repo,
        "code_read",
        json!({"ranges":[{"path":"src/a.rs","start":1,"end":1}]}),
    );
    let read: Value = serde_json::from_str(main["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(read[0]["lines"], json!(["main branch"]));
    let registered = f.cli(&f.repo, &["workspaces"]);
    let linked = registered
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["root"] == json!(f.linked))
        .unwrap();
    assert_eq!(linked["managed_parent"], json!(f.repo));
    assert_eq!(linked["scopes"], json!(["src"]));
    assert_eq!(linked["allow_worktrees"], false);
    assert_eq!(linked["prewarm"], false);
    f.cli(&f.repo, &["authorize-worktrees", "--revoke"]);
    assert_eq!(
        f.mcp(&f.linked, "code_files", json!({"glob":"*.rs"}))["isError"],
        true
    );
    let coverage = f.cli(&f.repo, &["coverage"]);
    let revoked = coverage["roots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["registration"]["root"] == json!(f.linked))
        .unwrap();
    assert!(revoked["error"].as_str().unwrap().contains("revoked"));
    assert!(revoked.get("files").is_none());
}

#[test]
fn managed_worktrees_follow_parent_migration_but_not_hidden_or_secret_files() {
    let mut f = Fixture::new();
    f.start();
    f.cli(&f.repo, &["authorize-worktrees"]);
    f.cli(&f.linked, &["prepare"]);
    let before = f.mcp(&f.linked, "code_files", json!({"glob":"*.rs"}));
    assert_eq!(before["isError"], false);
    f.cli(&f.repo, &["migrate", "--add-scope", ".", "--apply"]);
    let docs = f.mcp(&f.linked, "code_files", json!({"glob":"*.md"}));
    assert_eq!(docs["isError"], false);
    let value: Value = serde_json::from_str(docs["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(value["paths"], json!(["README.md"]));
    let secret = f.mcp(&f.linked, "code_files", json!({"glob":"secrets.json"}));
    let value: Value =
        serde_json::from_str(secret["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(value["total_matches"], 0);
    let hidden = f.mcp(
        &f.linked,
        "code_read",
        json!({"ranges":[{"path":".git","start":1,"end":1}]}),
    );
    assert_eq!(hidden["isError"], true);
}

#[test]
fn untrusted_roots_cannot_be_registered_via_mcp_and_status_exposes_both_builds() {
    let mut f = Fixture::new();
    f.start();
    f.cli(&f.repo, &["authorize-worktrees"]);
    let unrelated = f._dir.path().join("unrelated");
    std::fs::create_dir(&unrelated).unwrap();
    assert!(
        !f.command()
            .arg("--root")
            .arg(&unrelated)
            .arg("prepare")
            .output()
            .unwrap()
            .status
            .success()
    );
    let status = f.mcp(&f.repo, "code_status", json!({}));
    let value: Value =
        serde_json::from_str(status["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(value["capabilities"]["schema"], 3);
    assert_eq!(
        value["mcp_client"]["executable_sha256"],
        value["capabilities"]["executable_sha256"]
    );
    let doctor = f.cli(&f.repo, &["doctor"]);
    assert_eq!(doctor["recovery"]["automatic_process_termination"], false);
    assert_eq!(doctor["root_resolution"]["registered"], true);
}

#[test]
fn pruning_removes_only_missing_managed_roots_and_retains_manual_registrations() {
    let mut f = Fixture::new();
    f.start();
    f.cli(&f.repo, &["authorize-worktrees"]);
    f.cli(&f.linked, &["prepare"]);
    assert_eq!(
        f.cli(&f.repo, &["prune-worktrees", "--apply"])["missing_managed_roots"],
        json!([])
    );
    let output = Command::new("git")
        .arg("-C")
        .arg(&f.repo)
        .args(["worktree", "remove", "--force"])
        .arg(&f.linked)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        f.cli(&f.repo, &["prune-worktrees"])["missing_managed_roots"],
        json!([f.linked])
    );
    assert_eq!(f.cli(&f.repo, &["workspaces"]).as_array().unwrap().len(), 2);
    f.cli(&f.repo, &["prune-worktrees", "--apply"]);
    assert_eq!(f.cli(&f.repo, &["workspaces"]).as_array().unwrap().len(), 1);
}

#[test]
fn persistent_mcp_observes_new_authorization_after_startup() {
    let mut f = Fixture::new();
    f.start();
    let mut child = f
        .command()
        .current_dir(&f.linked)
        .arg("shared-mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    for id in 1..=2 {
        writeln!(
            child.stdin.as_mut().unwrap(),
            "{}",
            json!({
                "jsonrpc":"2.0","id":id,"method":"tools/call",
                "params":{"name":"code_files","arguments":{"glob":"*.rs"}}
            })
        )
        .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["result"]["isError"], id == 1);
        if id == 1 {
            f.cli(&f.repo, &["authorize-worktrees"]);
            f.cli(&f.linked, &["sync"]);
        }
    }
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}

#[test]
fn mcp_status_does_not_prepare_or_load_unregistered_worktrees() {
    let mut f = Fixture::new();
    f.start();
    f.cli(&f.repo, &["authorize-worktrees"]);
    let before = f.cli(&f.repo, &["workspaces"]);
    let status = f.mcp(&f.linked, "code_status", json!({}));
    assert_eq!(status["isError"], true);
    assert!(
        status["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("status root is not registered")
    );
    assert_eq!(f.cli(&f.repo, &["workspaces"]), before);
    assert_eq!(f.cli(&f.repo, &["ping"])["loaded_roots"], 0);
    let lookup = f.mcp(&f.linked, "code_files", json!({"glob":"*.rs"}));
    assert_eq!(lookup["isError"], false, "{lookup}");
    let status = f.mcp(&f.linked, "code_status", json!({}));
    let value: Value =
        serde_json::from_str(status["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(value["index_state"], "loaded");
}

#[test]
fn registry_capacity_does_not_evict_existing_or_manual_roots() {
    let mut f = Fixture::new();
    f.start();
    f.cli(&f.repo, &["authorize-worktrees"]);
    let path = f.home.join("workspaces.json");
    let mut specs: Vec<Value> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let template = specs[0].clone();
    for index in 1..MAX_REGISTERED_ROOTS {
        let root = f._dir.path().join(format!("manual-{index}"));
        std::fs::create_dir(&root).unwrap();
        let mut spec = template.clone();
        spec["root"] = json!(root);
        spec["allow_worktrees"] = json!(false);
        if index % 2 == 0 {
            spec["managed_parent"] = json!(f.repo);
        }
        if index == 2 {
            std::fs::remove_dir(&root).unwrap();
            std::os::unix::fs::symlink(f._dir.path().join("missing-target"), &root).unwrap();
        }
        specs.push(spec);
    }

    std::fs::write(&path, serde_json::to_vec(&specs).unwrap()).unwrap();
    let output = f
        .command()
        .arg("--root")
        .arg(&f.linked)
        .arg("prepare")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains(&format!("{MAX_REGISTERED_ROOTS} registered roots")));
    let after: Vec<Value> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(after, specs);
}

#[test]
fn hardening_failed_preparation_does_not_commit_pruning() {
    let f = Fixture::new();
    f.cli(&f.repo, &["authorize-worktrees"]);
    let path = f.home.join("workspaces.json");
    let mut specs: Vec<Value> =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    specs[0]["globs"] = json!(["["]);
    for index in 0..MAX_REGISTERED_ROOTS - 1 {
        let mut spec = specs[0].clone();
        spec["root"] = json!(f._dir.path().join(format!("missing-managed-{index}")));
        spec["managed_parent"] = json!(f.repo);
        spec["allow_worktrees"] = json!(false);
        specs.push(spec);
    }
    std::fs::write(&path, serde_json::to_vec(&specs).unwrap()).unwrap();
    let output = f.command().arg("--root").arg(&f.linked).arg("prepare")
        .output().unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("CCTX_REGISTRY_RECLAIM"));
    let after: Vec<Value> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(after, specs);
}

#[test]
fn hardening_ambiguous_managed_root_error_does_not_commit_pruning() {
    let f = Fixture::new();
    f.cli(&f.repo, &["authorize-worktrees"]);
    let path = f.home.join("workspaces.json");
    let mut specs: Vec<Value> =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let non_directory = f._dir.path().join("not-a-directory");
    std::fs::write(&non_directory, "not a directory").unwrap();
    for index in 0..MAX_REGISTERED_ROOTS - 1 {
        let mut spec = specs[0].clone();
        spec["root"] = if index == MAX_REGISTERED_ROOTS - 2 {
            json!(non_directory.join("managed"))
        } else {
            json!(f._dir.path().join(format!("missing-managed-{index}")))
        };
        spec["managed_parent"] = json!(f.repo);
        spec["allow_worktrees"] = json!(false);
        specs.push(spec);
    }
    std::fs::write(&path, serde_json::to_vec(&specs).unwrap()).unwrap();
    let output = f.command().arg("--root").arg(&f.linked).arg("prepare")
        .output().unwrap();
    assert!(!output.status.success());
    let after: Vec<Value> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(after, specs);
}

#[test]
fn hardening_unrelated_registration_preserves_failure_cooldown() {
    let mut f = Fixture::new();
    f.start();
    f.cli(&f.repo, &["authorize-worktrees"]);
    let unregistered = f._dir.path().join("unregistered");
    std::fs::create_dir(&unregistered).unwrap();
    let arguments = json!({"pattern":"needle"});
    let first = f.mcp_once(&unregistered, "code_context", arguments.clone());
    assert!(first["content"][0]["text"].as_str().unwrap().contains("CACHE_MISS"));
    let started = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(3));
        let result = f.mcp_once(&unregistered, "code_context", arguments.clone());
        if result["content"][0]["text"].as_str().unwrap().contains("CACHE_LOAD_FAILED") {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    f.cli(&f.linked, &["prepare"]);
    let result = f.mcp_once(&unregistered, "code_context", arguments);
    assert!(result["content"][0]["text"].as_str().unwrap().contains("CACHE_LOAD_FAILED"));
}

#[test]
fn hardening_parent_authorization_change_releases_cached_failure() {
    let mut f = Fixture::new();
    f.start();
    let arguments = json!({"pattern":"linked branch"});
    let first = f.mcp_once(&f.linked, "code_context", arguments.clone());
    assert!(first["content"][0]["text"].as_str().unwrap().contains("CACHE_MISS"));
    let started = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(3));
        let result = f.mcp_once(&f.linked, "code_context", arguments.clone());
        if result["content"][0]["text"].as_str().unwrap().contains("CACHE_LOAD_FAILED") {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    f.cli(&f.repo, &["authorize-worktrees"]);
    let result = f.mcp_once(&f.linked, "code_context", arguments);
    assert!(result["content"][0]["text"].as_str().unwrap().contains("CACHE_MISS"));
    assert!(!result["content"][0]["text"].as_str().unwrap().contains("CACHE_LOAD_FAILED"));
}

#[test]
fn registry_capacity_reclaims_only_missing_managed_roots_for_prepare_and_register() {
    for prepare in [true, false] {
        let mut f = Fixture::new();
        f.start();
        f.cli(&f.repo, &["authorize-worktrees"]);
        let path = f.home.join("workspaces.json");
        let mut specs: Vec<Value> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let template = specs[0].clone();
        let mut existing = template.clone();
        let existing_root = f._dir.path().join("existing-managed");
        std::fs::create_dir(&existing_root).unwrap();
        existing["root"] = json!(existing_root);
        existing["managed_parent"] = json!(f.repo);
        existing["allow_worktrees"] = json!(false);
        specs.push(existing.clone());
        let mut manual = template.clone();
        manual["root"] = json!(f._dir.path().join("missing-manual"));
        manual["allow_worktrees"] = json!(false);
        specs.push(manual.clone());
        for index in 0..MAX_REGISTERED_ROOTS - 3 {
            let mut missing = existing.clone();
            missing["root"] = json!(f._dir.path().join(format!("missing-managed-{index}")));
            specs.push(missing);
        }
        assert_eq!(specs.len(), MAX_REGISTERED_ROOTS);
        std::fs::write(&path, serde_json::to_vec(&specs).unwrap()).unwrap();

        if prepare {
            f.cli(&f.linked, &["prepare"]);
        } else {
            f.cli(&f.linked, &["--scope", "src", "--glob", "*.rs", "register"]);
        }
        let after: Vec<Value> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(after.len(), 4);
        assert!(after.contains(&template));
        assert!(after.contains(&existing));
        assert!(after.contains(&manual));
        assert!(after.iter().any(|spec| spec["root"] == json!(f.linked)));
        assert_eq!(f.cli(&f.repo, &["ping"])["loaded_roots"], 0);
    }
}

#[test]
fn registry_below_capacity_does_not_recheck_missing_managed_roots() {
    let f = Fixture::new();
    let path = f.home.join("workspaces.json");
    let mut specs: Vec<Value> =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut missing = specs[0].clone();
    missing["root"] = json!(f._dir.path().join("missing-managed"));
    missing["managed_parent"] = json!(f.repo);
    missing["allow_worktrees"] = json!(false);
    specs.push(missing.clone());
    std::fs::write(&path, serde_json::to_vec(&specs).unwrap()).unwrap();
    f.cli(&f.linked, &["--scope", "src", "--glob", "*.rs", "register"]);
    let after: Vec<Value> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(after.len(), 3);
    assert!(after.contains(&missing));
}

#[test]
fn seventeen_manual_registrations_do_not_load_or_erase_other_roots() {
    let mut f = Fixture::new();
    f.start();
    let first = f.cli(&f.repo, &["workspaces"])[0].clone();
    for index in 1..17 {
        let root = f._dir.path().join(format!("registered-{index}"));
        std::fs::create_dir(&root).unwrap();
        f.cli(&root, &["--scope", ".", "--glob", "*.rs", "register"]);
    }

    let registered = f.cli(&f.repo, &["workspaces"]);
    assert_eq!(registered.as_array().unwrap().len(), 17);
    assert!(registered.as_array().unwrap().contains(&first));
    assert_eq!(f.cli(&f.repo, &["ping"])["loaded_roots"], 0);
}

#[test]
fn forty_metadata_registrations_preserve_roots_without_loading_source() {
    assert_eq!(MAX_REGISTERED_ROOTS, 256);
    let mut f = Fixture::new();
    f.start();
    let first = f.cli(&f.repo, &["workspaces"])[0].clone();
    for index in 1..40 {
        let root = f._dir.path().join(format!("metadata-root-{index}"));
        std::fs::create_dir(&root).unwrap();
        f.cli(&root, &["--scope", ".", "--glob", "*.rs", "register"]);
    }
    let registered = f.cli(&f.repo, &["workspaces"]);
    assert_eq!(registered.as_array().unwrap().len(), 40);
    assert!(registered.as_array().unwrap().contains(&first));
    let ping = f.cli(&f.repo, &["ping"]);
    assert_eq!(ping["loaded_roots"], 0);
    assert_eq!(ping["capabilities"]["max_registered_roots"], 256);
}

#[test]
fn mcp_trace_joins_host_result_client_phases_and_daemon_without_changing_snippet_shape() {
    let mut f = Fixture::new();
    f.start();
    let result = f.mcp(
        &f.repo,
        "code_read",
        json!({"ranges":[{"path":"src/a.rs","start":1,"end":1}]}),
    );
    assert_eq!(result["isError"], false);
    let snippets: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(snippets[0]["lines"], json!(["main branch"]));
    let trace = &result["_meta"]["cctx_trace"];
    let id = trace["trace_id"].as_str().unwrap();
    let footer: Value = serde_json::from_str(
        result["content"][1]["text"]
            .as_str()
            .unwrap()
            .strip_prefix("CCTX_TRACE ")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(footer["trace_id"], id);
    assert_eq!(trace["daemon"]["trace_id"], id);
    assert!(trace["started_unix_ms"].is_u64());
    assert!(
        trace["completed_unix_ms"].as_u64().unwrap() >= trace["started_unix_ms"].as_u64().unwrap()
    );
    let status = f.mcp(&f.repo, "code_status", json!({}));
    let data: Value = serde_json::from_str(status["content"][0]["text"].as_str().unwrap()).unwrap();
    let client = data["performance"]["recent_clients"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["trace_id"] == id)
        .unwrap();
    let daemon = data["performance"]["recent_requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == trace["daemon"]["daemon_request_id"])
        .unwrap();
    assert_eq!(daemon["trace_id"], id);
    assert_eq!(client["success"], true);
    assert!(client["ipc_ms"].as_f64().unwrap() >= 0.0);
    assert!(client["serialize_ms"].as_f64().unwrap() >= 0.0);
    assert!(client["write_ms"].as_f64().unwrap() >= 0.0);
    assert!(client["total_ms"].as_f64().unwrap() >= client["ipc_ms"].as_f64().unwrap());
}

#[test]
fn application_read_failure_is_not_reconnected_and_executed_twice() {
    let mut f = Fixture::new();
    f.start();
    let failed = f.mcp(
        &f.repo,
        "code_read",
        json!({"ranges":[{"path":"src/missing.rs","start":1,"end":1}]}),
    );
    assert_eq!(failed["isError"], true);
    let status = f.mcp(&f.repo, "code_status", json!({}));
    let data: Value = serde_json::from_str(status["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(data["protocol_calls"]["bincode:code_read"], 1);
    assert_eq!(data["protocol_failures"]["bincode:code_read"], 1);
}

#[test]
fn concurrent_prepare_is_idempotent_and_keeps_one_managed_entry() {
    let mut f = Fixture::new();
    f.start();
    f.cli(&f.repo, &["authorize-worktrees"]);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let f = &f;
            scope.spawn(move || {
                f.cli(&f.linked, &["prepare"]);
            });
        }
    });
    let specs = f.cli(&f.repo, &["workspaces"]);
    assert_eq!(specs.as_array().unwrap().len(), 2);
}

#[test]
fn authorized_nested_worktree_is_not_redirected_to_its_registered_ancestor() {
    let mut f = Fixture::new();
    f.start();
    let nested = f.repo.join("worktrees/nested");
    let output = Command::new("git")
        .arg("-C")
        .arg(&f.repo)
        .args(["worktree", "add", "-q", "-b", "nested"])
        .arg(&nested)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::write(nested.join("src/a.rs"), "nested branch\n").unwrap();
    f.cli(&f.repo, &["authorize-worktrees"]);
    let result = f.mcp(
        &nested,
        "code_read",
        json!({"ranges":[{"path":"src/a.rs","start":1,"end":1}]}),
    );
    assert_eq!(result["isError"], false, "{result}");
    let value: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(value[0]["lines"], json!(["nested branch"]));
}

#[test]
fn migration_rejects_binary_document_before_persisting_and_keeps_existing_budgets() {
    let mut f = Fixture::new();
    f.start();
    let before = f.cli(&f.repo, &["workspaces"]);
    std::fs::write(f.repo.join("binary.txt"), b"bad\0text").unwrap();
    let output = f
        .command()
        .arg("--root")
        .arg(&f.repo)
        .args(["migrate", "--add-scope", ".", "--apply"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("binary.txt"));
    assert_eq!(f.cli(&f.repo, &["workspaces"]), before);
    let applied = f.cli(
        &f.repo,
        &[
            "migrate",
            "--add-scope",
            ".",
            "--add-exclude",
            "binary.txt",
            "--apply",
        ],
    );
    assert_eq!(applied["source_verified"], true);
    assert_eq!(applied["after"]["policy"], before[0]["policy"]);
}
