use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    _directory: tempfile::TempDir,
    home: PathBuf,
    warm: PathBuf,
    cold: PathBuf,
    child: Child,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("state");
        let warm = directory.path().join("warm");
        let cold = directory.path().join("cold");
        for root in [&warm, &cold] {
            std::fs::create_dir(root).unwrap();
            std::fs::write(root.join("a.rs"), "needle\n").unwrap();
            let result = Command::new(env!("CARGO_BIN_EXE_cctx"))
                .env("CCTX_HOME", &home)
                .arg("--root")
                .arg(root)
                .args(["--scope", ".", "--glob", "*.rs", "register"])
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        let bin = directory.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let rg = bin.join("rg");
        std::fs::write(&rg,"#!/bin/sh\ncase \"$PWD\" in */cold) case \" $* \" in *\" --files \"*) sleep 0.25;; esac;; esac\nexec /usr/bin/rg \"$@\"\n").unwrap();
        std::fs::set_permissions(&rg, std::fs::Permissions::from_mode(0o755)).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_cctx"))
            .env("CCTX_HOME", &home)
            .env("CCTX_MEMORY_MAX_MIB", "4096")
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .arg("daemon")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut fixture = Self {
            _directory: directory,
            home,
            warm,
            cold,
            child,
        };
        let started = Instant::now();
        while UnixStream::connect(fixture.home.join("daemon.sock")).is_err() {
            assert!(fixture.child.try_wait().unwrap().is_none(),"daemon exited before accepting connections");
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(rpc(&fixture.home, None, "ping", json!({}))["ready"], true);
        assert_eq!(
            rpc(&fixture.home, Some(&fixture.warm), "code_sync", json!({}))["data"]["last_update"]
                ["verified_all_source"],
            true
        );
        fixture
    }
    fn background_active(&self) {
        let started = Instant::now();
        loop {
            let status = rpc(&self.home, Some(&self.warm), "code_status", json!({}));
            if status["data"]["performance"]["active_background"].is_object() {
                return;
            }
            assert!(started.elapsed() < Duration::from_secs(5), "{status}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

fn rpc(home: &Path, root: Option<&Path>, name: &str, arguments: Value) -> Value {
    let value = rpc_raw(home, root, name, arguments);
    assert!(value.get("error").is_none(), "{value}");
    value["result"].clone()
}

fn rpc_raw(home: &Path, root: Option<&Path>, name: &str, arguments: Value) -> Value {
    let mut stream = UnixStream::connect(home.join("daemon.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    writeln!(
        stream,
        "{}",
        json!({"root":root,"name":name,"arguments":arguments})
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    let value: Value = serde_json::from_str(&line).unwrap();
    value
}

fn wait_for_cache(home: &Path, root: &Path) -> Value {
    let deadline = Instant::now();
    loop {
        let status = rpc(home, Some(root), "code_status", json!({}));
        if status["data"]["index_state"] == "loaded" {
            return status;
        }
        assert!(deadline.elapsed() < Duration::from_secs(10), "{status}");
        std::thread::sleep(Duration::from_millis(3));
    }
}

#[test]
fn lru_eviction_releases_a_root_and_later_demand_lazily_loads_it_again() {
    let fixture = Fixture::new();
    for number in 0..8 {
        let root = fixture
            .home
            .parent()
            .unwrap()
            .join(format!("cache-{number}"));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.rs"), "needle\n").unwrap();
        let registered = Command::new(env!("CARGO_BIN_EXE_cctx"))
            .env("CCTX_HOME", &fixture.home)
            .arg("--root")
            .arg(&root)
            .args([
                "--scope",
                ".",
                "--glob",
                "*.rs",
                "--prewarm",
                "--allow-root-scope",
                "register",
            ])
            .output()
            .unwrap();
        assert!(
            registered.status.success(),
            "{}",
            String::from_utf8_lossy(&registered.stderr)
        );
        let miss = rpc_raw(
            &fixture.home,
            Some(&root),
            "code_grep",
            json!({"pattern":"needle"}),
        );
        assert!(
            miss["error"].as_str().unwrap().starts_with("CACHE_MISS "),
            "{miss}"
        );
        let status = wait_for_cache(&fixture.home, &root);
        assert_eq!(status["data"]["cache"]["prewarm_pins_residency"], false);
        assert!(
            status["data"]["daemon_admission"]["loaded_roots"]
                .as_u64()
                .unwrap()
                <= 8
        );
    }
    let before = rpc(&fixture.home, Some(&fixture.warm), "code_status", json!({}));
    assert_eq!(before["data"]["index_state"], "unloaded");
    assert_eq!(
        before["data"]["daemon_admission"]["root_loads"][fixture.warm.to_str().unwrap()],
        1
    );
    let started = Instant::now();
    let miss = rpc_raw(
        &fixture.home,
        Some(&fixture.warm),
        "code_grep",
        json!({"pattern":"needle"}),
    );
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(
        miss["error"].as_str().unwrap().starts_with("CACHE_MISS "),
        "{miss}"
    );
    let after = wait_for_cache(&fixture.home, &fixture.warm);
    assert_eq!(
        after["data"]["daemon_admission"]["root_loads"][fixture.warm.to_str().unwrap()],
        2
    );
    assert!(
        after["data"]["daemon_admission"]["loaded_roots"]
            .as_u64()
            .unwrap()
            <= 8
    );
    assert_eq!(
        rpc(
            &fixture.home,
            Some(&fixture.warm),
            "code_grep",
            json!({"pattern":"needle"})
        )["data"]["total_matches"],
        1
    );
}

#[test]
fn cold_misses_return_without_waiting_and_coalesce_only_the_requested_root() {
    let fixture = Fixture::new();
    std::thread::scope(|scope| {
        let mut calls = Vec::new();
        for _ in 0..3 {
            let f = &fixture;
            calls.push(scope.spawn(move || {
                let started = Instant::now();
                let miss = rpc_raw(
                    &f.home,
                    Some(&f.cold),
                    "code_grep",
                    json!({"pattern":"needle"}),
                );
                let elapsed = started.elapsed();
                assert!(
                    elapsed < Duration::from_millis(500),
                    "cold miss elapsed={elapsed:?}; response={miss}; status={}",
                    rpc(&f.home, Some(&f.warm), "code_status", json!({}))["data"]["performance"]
                );
                assert!(
                    miss["error"].as_str().unwrap().starts_with("CACHE_MISS "),
                    "{miss}"
                );
                assert!(
                    miss["error"].as_str().unwrap().contains("permitted host"),
                    "{miss}"
                );
                miss
            }));
        }
        fixture.background_active();
        let files = rpc_raw(
            &fixture.home,
            Some(&fixture.cold),
            "code_files",
            json!({"glob":"*.rs"}),
        );
        assert!(
            files["error"].as_str().unwrap().starts_with("CACHE_MISS "),
            "{files}"
        );
        let context = rpc_raw(
            &fixture.home,
            Some(&fixture.cold),
            "code_context",
            json!({"pattern":"needle","radius":1,"limit":1}),
        );
        assert!(
            context["error"]
                .as_str()
                .unwrap()
                .starts_with("CACHE_MISS "),
            "{context}"
        );
        let start = Instant::now();
        assert_eq!(
            rpc(
                &fixture.home,
                Some(&fixture.warm),
                "code_grep",
                json!({"pattern":"needle"})
            )["data"]["total_matches"],
            1
        );
        assert_eq!(rpc(&fixture.home, None, "ping", json!({}))["ready"], true);
        assert!(
            start.elapsed() < Duration::from_millis(900),
            "warm query blocked by cold loader"
        );
        for call in calls {
            assert!(call.join().unwrap()["error"].is_string());
        }
    });
    let wait = Instant::now();
    while rpc(&fixture.home, Some(&fixture.cold), "code_status", json!({}))["data"]["index_state"]
        != "loaded"
    {
        assert!(wait.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(3));
    }
    assert_eq!(
        rpc(
            &fixture.home,
            Some(&fixture.cold),
            "code_grep",
            json!({"pattern":"needle"})
        )["data"]["total_matches"],
        1
    );
    let status = rpc(&fixture.home, Some(&fixture.cold), "code_status", json!({}));
    assert_eq!(
        status["data"]["daemon_admission"]["root_loads"][fixture.cold.to_str().unwrap()],
        1
    );
    assert_eq!(
        status["data"]["performance"]["recent_background"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|job| job["kind"] == "cold_load"
                && job["root"] == fixture.cold.to_string_lossy().as_ref())
            .count(),
        1
    );
    let target = status["data"]["daemon_admission"]["target_bytes"].as_u64().unwrap();
    assert!(target > 0 && target <= 3_u64 * 1024 * 1024 * 1024);
    assert_eq!(status["data"]["process_memory"]["adaptive"]["mode"], "capped_auto");
}

#[test]
fn strict_sync_is_a_root_local_barrier_and_has_separate_timing() {
    let fixture = Fixture::new();
    rpc(&fixture.home, Some(&fixture.cold), "code_sync", json!({}));
    std::thread::scope(|scope| {
        let f = &fixture;
        let sync = scope.spawn(move || rpc(&f.home, Some(&f.cold), "code_sync", json!({})));
        fixture.background_active();
        let started = Instant::now();
        let miss = rpc_raw(
            &fixture.home,
            Some(&fixture.cold),
            "code_read",
            json!({"ranges":[{"path":"a.rs","start":1,"end":1}]}),
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(
            miss["error"].as_str().unwrap().contains("state=syncing"),
            "{miss}"
        );
        let start = Instant::now();
        let warm = rpc(
            &fixture.home,
            Some(&fixture.warm),
            "code_grep",
            json!({"pattern":"needle"}),
        );
        assert_eq!(warm["data"]["total_matches"], 1);
        assert!(
            start.elapsed() < Duration::from_millis(900),
            "other root blocked by strict sync"
        );
        assert_eq!(
            sync.join().unwrap()["data"]["last_update"]["verified_all_source"],
            true
        );
    });
    std::thread::sleep(Duration::from_millis(10));
    let status = rpc(&fixture.home, Some(&fixture.cold), "code_status", json!({}));
    let perf = &status["data"]["performance"];
    assert!(
        perf["recent_background"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "strict_sync"
                && entry["elapsed_ms"].as_f64().unwrap() > 200.0)
    );
    assert!(
        perf["recent_requests"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["id"].as_u64().is_some()
                && entry["queue_ms"].as_f64().unwrap() >= 0.0
                && entry["execute_ms"].as_f64().unwrap() >= 0.0
                && entry["response_ms"].as_f64().unwrap() >= 0.0)
    );
}
