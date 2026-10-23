use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    root: PathBuf,
    child: Child,
}

struct GuardedChild(Child);
impl Drop for GuardedChild {
    fn drop(&mut self) {
        let _=self.0.kill();
        let _=self.0.wait();
    }
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("state");
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn needle() {}\n").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_cctx"))
            .env("CCTX_HOME", &home)
            .args([
                "--root",
                root.to_str().unwrap(),
                "--scope",
                "src",
                "--glob",
                "*.rs",
                "register",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let child = Command::new(env!("CARGO_BIN_EXE_cctx"))
            .env("CCTX_HOME", &home)
            .arg("daemon")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let fixture = Self {
            _dir: dir,
            home,
            root,
            child,
        };
        let start = Instant::now();
        while fixture.rpc("ping", None, json!({})).is_err() {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
        fixture
    }
    fn rpc(
        &self,
        name: &str,
        root: Option<&Path>,
        arguments: Value,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        // These functional tests opt into a ready index; raw RPC tests cover cold misses.
        if matches!(
            name,
            "code_files"
                | "code_grep"
                | "code_context"
                | "code_read"
                | "code_profile"
                | "code_profile_full"
        ) {
            let status = rpc(&self.home, "code_status", root, json!({}))?;
            if status["data"]["index_state"] != "loaded" {
                rpc(&self.home, "code_sync", root, json!({}))?;
            }
        }
        rpc(&self.home, name, root, arguments)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn rpc(
    home: &Path,
    name: &str,
    root: Option<&Path>,
    arguments: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let mut stream = UnixStream::connect(home.join("daemon.sock"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    writeln!(
        stream,
        "{}",
        json!({"root":root,"name":name,"arguments":arguments})
    )?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let response: Value = serde_json::from_str(&line)?;
    if response.get("error").is_some() {
        return Err(response["error"].to_string().into());
    }
    Ok(response["result"].clone())
}

#[test]
fn daemon_shared_queries_use_one_process_and_validate_roots() {
    let fixture = Fixture::new();
    let first = fixture.rpc("ping", None, json!({})).unwrap()["pid"].clone();
    for _ in 0..3 {
        let value = fixture
            .rpc(
                "code_grep",
                Some(&fixture.root),
                json!({"pattern":"needle"}),
            )
            .unwrap();
        assert_eq!(value["data"]["total_matches"], 1);
    }

    assert_eq!(fixture.rpc("ping", None, json!({})).unwrap()["pid"], first);
    assert!(
        fixture
            .rpc(
                "code_grep",
                Some(&fixture.home),
                json!({"pattern":"needle"})
            )
            .is_err()
    );
    assert!(
        fixture
            .rpc("unknown", Some(&fixture.root), json!({}))
            .is_err()
    );
}

#[test]
fn deleted_runtime_socket_stops_the_daemon_without_an_orphaned_poll_loop() {
    let mut fixture=Fixture::new();
    std::fs::remove_file(fixture.home.join("daemon.sock")).unwrap();
    let started=Instant::now();
    loop {
        if let Some(status)=fixture.child.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        assert!(started.elapsed()<Duration::from_secs(5),"daemon kept running without a callable socket");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn hardening_broadened_runtime_lock_permissions_stop_the_daemon() {
    use std::os::unix::fs::PermissionsExt;
    let mut fixture = Fixture::new();
    std::fs::set_permissions(
        fixture.home.join("daemon.lock"),
        std::fs::Permissions::from_mode(0o666),
    ).unwrap();
    let started = Instant::now();
    loop {
        if let Some(status) = fixture.child.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(5),
            "daemon accepted a non-private runtime lock");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn daemon_excludes_second_owner_and_recovers_stale_socket_on_restart() {
    use std::os::unix::fs::PermissionsExt;
    let mut fixture = Fixture::new();
    let second = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &fixture.home)
        .arg("daemon")
        .output()
        .unwrap();
    assert!(!second.status.success());
    fixture.child.kill().unwrap();
    fixture.child.wait().unwrap();
    std::fs::set_permissions(
        fixture.home.join("daemon.lock"),
        std::fs::Permissions::from_mode(0o644),
    ).unwrap();
    fixture.child = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &fixture.home)
        .arg("daemon")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    loop {
        if let Ok(value) = fixture.rpc("ping", None, json!({})) {
            assert_eq!(value["ready"], true);
            assert_eq!(std::fs::metadata(fixture.home.join("daemon.lock"))
                .unwrap().permissions().mode() & 0o777, 0o600);
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn concurrent_clients_receive_correct_responses_and_mcp_routes() {
    let fixture = Fixture::new();
    fixture
        .rpc("code_sync", Some(&fixture.root), json!({}))
        .unwrap();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let home = &fixture.home;
            let root = &fixture.root;
            scope.spawn(move||{
                for _ in 0..10 {
                    assert_eq!(rpc(home,"code_grep",Some(root),json!({"pattern":"needle"})).unwrap()["data"]["total_matches"],1);
                }

            });
        }
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &fixture.home)
        .current_dir(&fixture.root)
        .arg("shared-mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let input = child.stdin.as_mut().unwrap();
        writeln!(input,"{}",json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})).unwrap();
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
        )
        .unwrap();
        writeln!(input,"{}",json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"code_context","arguments":{"pattern":"needle"}}})).unwrap();
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let values: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(values[1]["result"]["tools"].as_array().unwrap().len(), 7);
    assert_eq!(values[2]["result"]["isError"], false, "{}", values[2]);
    let context: Value =
        serde_json::from_str(values[2]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(context.get("root").is_none());
    assert!(context.get("data").is_none());
    assert!(context.get("consistency").is_none());
    assert!(context["search"].get("consistency").is_none());
    assert!(context["search"]["matches"][0].get("text").is_none());
    assert_eq!(context["freshness"]["basis"], "watcher_snapshot");
    assert_eq!(context["freshness"]["returned_file_check"], "sha256");
    assert_eq!(context["freshness"]["full_disk_verified"], false);
    assert!(
        context["freshness"]["notice"]
            .as_str()
            .unwrap()
            .contains("live disk read")
    );
}

#[test]
fn native_stdio_bridge_preserves_framed_requests_and_application_errors() {
    use std::io::Read;
    let fixture = Fixture::new();
    fixture.rpc("code_sync", Some(&fixture.root), json!({})).unwrap();
    let mut child = GuardedChild(Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &fixture.home).arg("native-bridge")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap());
    let mut reader = BufReader::new(child.0.stdout.take().unwrap());
    let mut ready = String::new();
    reader.read_line(&mut ready).unwrap();
    assert_eq!(ready, "CCTXN1\n");
    let input = child.0.stdin.as_mut().unwrap();
    input.write_all(b"CCTXJ1\n").unwrap();
    let mut call = |name: &str, arguments: Value| {
        let bytes = serde_json::to_vec(&json!({
            "root":fixture.root,"name":name,"arguments":arguments,"compact":true,
        })).unwrap();
        input.write_all(&(bytes.len() as u32).to_be_bytes()).unwrap();
        input.write_all(&bytes).unwrap();
        input.flush().unwrap();
        let mut size = [0;4];
        reader.read_exact(&mut size).unwrap();
        let mut bytes = vec![0;u32::from_be_bytes(size) as usize];
        reader.read_exact(&mut bytes).unwrap();
        serde_json::from_slice::<Value>(&bytes).unwrap()
    };
    let result = call("code_context", json!({
        "pattern":"needle","paths":["src/a.rs"],"radius":0,"limit":1,
        "_cctx_trace":"native-stdio-check",
    }));
    assert_eq!(result["result"]["_cctx_trace"]["trace_id"], "native-stdio-check");
    assert_eq!(result["result"]["value"]["search"]["total_matches"], 1);
    let error = call("code_context", json!({"pattern":"x","paths":[".hidden"]}));
    assert!(error["error"].as_str().unwrap().contains("non-hidden"));
    assert_eq!(call("ping", json!({}))["result"]["pid"], fixture.child.id());
    child.0.stdin.take();
    let started = Instant::now();
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn cli_and_unregister_do_not_leave_old_root_accessible() {
    let fixture = Fixture::new();
    fixture
        .rpc("code_sync", Some(&fixture.root), json!({}))
        .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &fixture.home)
        .args(["--root", fixture.root.to_str().unwrap(), "grep", "needle"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["data"]["total_matches"],
        1
    );
    let output = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &fixture.home)
        .args(["--root", fixture.root.to_str().unwrap(), "unregister"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        fixture
            .rpc(
                "code_grep",
                Some(&fixture.root),
                json!({"pattern":"needle"})
            )
            .is_err()
    );
}

#[test]
fn incomplete_client_does_not_block_normal_requests() {
    let fixture = Fixture::new();
    let mut stalled = UnixStream::connect(fixture.home.join("daemon.sock")).unwrap();
    stalled.write_all(b"{").unwrap();
    let start = Instant::now();
    assert_eq!(fixture.rpc("ping", None, json!({})).unwrap()["ready"], true);
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "incomplete client blocked accept path"
    );
}

#[test]
fn oversized_opening_frame_is_bounded_and_daemon_recovers() {
    let fixture = Fixture::new();
    let mut stream = UnixStream::connect(fixture.home.join("daemon.sock")).unwrap();
    stream
        .write_all(&vec![b'x'; code_context::daemon::MAX_WIRE + 1])
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    assert!(response.contains("opening frame exceeds"));
    assert_eq!(fixture.rpc("ping", None, json!({})).unwrap()["ready"], true);
}

#[test]
fn framed_json_protocol_supports_persistent_bounded_requests() {
    let fixture = Fixture::new();
    let mut stream = UnixStream::connect(fixture.home.join("daemon.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream.write_all(b"CCTXJ1\n").unwrap();
    for index in 0..2 {
        let request =
            serde_json::to_vec(&json!({"root":null,"name":"ping","arguments":{},"compact":true}))
                .unwrap();
        stream
            .write_all(&u32::try_from(request.len()).unwrap().to_be_bytes())
            .unwrap();
        stream.write_all(&request).unwrap();
        let mut length = [0_u8; 4];
        std::io::Read::read_exact(&mut stream, &mut length).unwrap();
        let mut response = vec![0; u32::from_be_bytes(length) as usize];
        std::io::Read::read_exact(&mut stream, &mut response).unwrap();
        let response: Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(response["result"]["ready"], true);
        assert_eq!(
            response["result"]["protocol_calls"]["framed_json:ping"],
            index + 1
        );
    }
}

#[test]
fn protocol_call_metrics_bound_unknown_operation_cardinality() {
    let fixture = Fixture::new();
    for index in 0..100 {
        assert!(
            fixture
                .rpc(&format!("unknown-{index}"), None, json!({}))
                .is_err()
        );
    }
    let status = fixture.rpc("ping", None, json!({})).unwrap();
    let calls = status["protocol_calls"].as_object().unwrap();
    let failures = status["protocol_failures"].as_object().unwrap();
    assert_eq!(calls["json_lines:unknown"], 100);
    assert_eq!(failures["json_lines:unknown"], 100);
    assert!(
        calls
            .keys()
            .all(|key| !key.starts_with("json_lines:unknown-"))
    );
}

#[test]
fn code_status_exposes_protocol_success_and_failure_observability() {
    let fixture = Fixture::new();
    assert!(fixture.rpc("invalid", None, json!({})).is_err());
    let status = fixture
        .rpc("code_status", Some(&fixture.root), json!({}))
        .unwrap();
    let data = &status["data"];
    assert_eq!(data["protocol_calls"]["json_lines:code_status"], 1);
    assert_eq!(data["protocol_calls"]["json_lines:unknown"], 1);
    assert_eq!(data["protocol_failures"]["json_lines:unknown"], 1);
}

#[test]
fn repeated_status_checks_never_load_or_refresh_source() {
    let fixture = Fixture::new();
    // Indexing this file must fail, but merely opening/reloading a window and
    // inspecting readiness must not inspect its contents.
    std::fs::write(fixture.root.join("src/binary.rs"), b"not\0text").unwrap();
    for _ in 0..4 {
        let status = fixture
            .rpc("code_status", Some(&fixture.root), json!({}))
            .unwrap();
        assert_eq!(status["data"]["index_state"], "unloaded");
        assert!(status["data"]["stats"].is_null());
        assert_eq!(status["data"]["daemon_admission"]["loaded_roots"], 0);
        assert!(
            status["data"]["daemon_admission"]["root_loads"]
                .as_object()
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        fixture
            .rpc(
                "code_grep",
                Some(&fixture.root),
                json!({"pattern":"needle"})
            )
            .is_err()
    );
    std::fs::remove_file(fixture.root.join("src/binary.rs")).unwrap();
    assert_eq!(
        fixture
            .rpc(
                "code_grep",
                Some(&fixture.root),
                json!({"pattern":"needle"})
            )
            .unwrap()["data"]["total_matches"],
        1
    );
    let before = fixture
        .rpc("code_status", Some(&fixture.root), json!({}))
        .unwrap();
    std::fs::write(fixture.root.join("src/a.rs"), b"changed\0source").unwrap();
    for _ in 0..4 {
        let status = fixture
            .rpc("code_status", Some(&fixture.root), json!({}))
            .unwrap();
        assert_eq!(status["data"]["index_state"], "loaded");
        assert_eq!(
            status["data"]["stats"]["generation"],
            before["data"]["stats"]["generation"]
        );
        assert_eq!(status["data"]["io_profile"], before["data"]["io_profile"]);
        assert_eq!(
            status["data"]["daemon_admission"]["root_loads"],
            before["data"]["daemon_admission"]["root_loads"]
        );
    }
    assert!(
        fixture
            .rpc(
                "code_grep",
                Some(&fixture.root),
                json!({"pattern":"needle"})
            )
            .is_err()
    );
}

#[test]
fn sequential_connection_churn_releases_worker_threads() {
    let fixture = Fixture::new();
    let pid = fixture.rpc("ping", None, json!({})).unwrap()["pid"]
        .as_u64()
        .unwrap();
    let thread_count = || {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("Threads:"))
            .unwrap()
            .trim()
            .parse::<usize>()
            .unwrap()
    };
    let before = thread_count();
    for _ in 0..500 {
        assert_eq!(fixture.rpc("ping", None, json!({})).unwrap()["ready"], true);
    }
    let started = Instant::now();
    while thread_count() > before + 2 {
        assert!(started.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn malformed_protocol_and_unknown_fields_return_errors_without_crashing() {
    let fixture = Fixture::new();
    for message in [
        "not-json\n",
        "{\"name\":\"ping\",\"arguments\":{},\"unknown\":1}\n",
    ] {
        let mut stream = UnixStream::connect(fixture.home.join("daemon.sock")).unwrap();
        stream.write_all(message.as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        assert!(
            serde_json::from_str::<Value>(&line)
                .unwrap()
                .get("error")
                .is_some()
        );
    }

    assert_eq!(fixture.rpc("ping", None, json!({})).unwrap()["ready"], true);
}

#[test]
fn legacy_benchmark_retains_live_cli_after_shared_becomes_default() {
    let fixture = Fixture::new();
    let queries = fixture.home.join("queries.json");
    let output = fixture.home.join("legacy.json");
    std::fs::write(&queries, "[\"needle\"]").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &fixture.home)
        .args([
            "--root",
            fixture.root.to_str().unwrap(),
            "--scope",
            "src",
            "--glob",
            "*.rs",
            "bench",
            "--queries",
            queries.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--rounds",
            "10",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(output.exists());
}

#[test]
fn cli_resolves_registered_ancestor_from_subdirectory() {
    let fixture = Fixture::new();
    fixture
        .rpc("code_sync", Some(&fixture.root), json!({}))
        .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &fixture.home)
        .current_dir(fixture.root.join("src"))
        .args(["grep", "needle"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&result.stdout).unwrap()["data"]["total_matches"],
        1
    );
}

#[test]
fn deleted_registered_root_can_still_be_revoked() {
    let fixture = Fixture::new();
    std::fs::remove_file(fixture.root.join("src/a.rs")).unwrap();
    std::fs::remove_dir(fixture.root.join("src")).unwrap();
    std::fs::remove_dir(&fixture.root).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &fixture.home)
        .args(["--root", fixture.root.to_str().unwrap(), "unregister"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fixture
            .rpc("code_workspaces", None, json!({}))
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn persistent_mcp_connection_recovers_after_daemon_restart() {
    let mut fixture = Fixture::new();
    fixture
        .rpc("code_sync", Some(&fixture.root), json!({}))
        .unwrap();
    let mut proxy = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &fixture.home)
        .current_dir(&fixture.root)
        .arg("shared-mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(proxy.stdout.take().unwrap());
    let input = proxy.stdin.as_mut().unwrap();
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})
    )
    .unwrap();
    writeln!(input,"{}",json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"code_grep","arguments":{"pattern":"needle"}}})).unwrap();
    input.flush().unwrap();
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    line.clear();
    output.read_line(&mut line).unwrap();
    let first: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(first["result"]["isError"], false, "{first}");

    fixture.child.kill().unwrap();
    fixture.child.wait().unwrap();
    fixture.child = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &fixture.home)
        .arg("daemon")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while fixture.rpc("ping", None, json!({})).is_err() {
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    fixture
        .rpc("code_sync", Some(&fixture.root), json!({}))
        .unwrap();

    let input = proxy.stdin.as_mut().unwrap();
    writeln!(input,"{}",json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"code_grep","arguments":{"pattern":"needle"}}})).unwrap();
    input.flush().unwrap();
    line.clear();
    output.read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["result"]["isError"],
        false
    );
    drop(proxy.stdin.take());
    assert!(proxy.wait().unwrap().success());
}

#[test]
fn legacy_prewarm_stays_unloaded_until_actual_source_demand() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("state");
    let root = directory.path().join("project");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "fn ready() {}\n").unwrap();
    let register = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &home)
        .args([
            "--root",
            root.to_str().unwrap(),
            "--scope",
            "src",
            "--glob",
            "*.rs",
            "--prewarm",
            "register",
        ])
        .output()
        .unwrap();
    assert!(register.status.success());
    let mut daemon = GuardedChild(Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &home)
        .arg("daemon")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap());
    let started = Instant::now();
    while rpc(&home, "ping", None, json!({})).is_err() {
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(
            rpc(&home, "ping", None, json!({})).unwrap()["loaded_roots"],
            0
        );
        let status = rpc(&home, "code_status", Some(&root), json!({})).unwrap();
        assert_eq!(status["data"]["index_state"], "unloaded");
        assert_eq!(status["data"]["cache"]["automatic_prewarm"], false);
    }
    let start = Instant::now();
    let miss = rpc(&home, "code_grep", Some(&root), json!({"pattern":"ready"})).unwrap_err();
    assert!(start.elapsed() < Duration::from_millis(500));
    assert!(miss.to_string().contains("CACHE_MISS "));
    let start = Instant::now();
    while rpc(&home, "code_status", Some(&root), json!({})).unwrap()["data"]["index_state"]
        != "loaded"
    {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        rpc(&home, "code_grep", Some(&root), json!({"pattern":"ready"})).unwrap()["data"]["total_matches"],
        1
    );
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
}

#[test]
fn source_plan_update_preserves_existing_registration_settings() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("state");
    let root = directory.path().join("project");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "fn ready() {}\n").unwrap();
    let register = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &home)
        .args([
            "--root",
            root.to_str().unwrap(),
            "--scope",
            "src",
            "--glob",
            "*.rs",
            "--exclude",
            "generated/**",
            "--prewarm",
            "--max-files",
            "123",
            "register",
        ])
        .output()
        .unwrap();
    assert!(
        register.status.success(),
        "{}",
        String::from_utf8_lossy(&register.stderr)
    );
    let update = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &home)
        .args([
            "--root",
            root.to_str().unwrap(),
            "--scope",
            ".",
            "--glob",
            "*.rs",
            "--glob",
            "*.ts",
            "--preserve-registration-settings",
            "--allow-root-scope",
            "register",
        ])
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );
    let result: Value = serde_json::from_slice(&update.stdout).unwrap();
    let plan = &result["plan"];
    assert_eq!(plan["scopes"], json!(["."]));
    assert_eq!(plan["globs"], json!(["*.rs", "*.ts"]));
    assert_eq!(plan["excludes"], json!(["generated/**"]));
    assert_eq!(plan["prewarm"], true);
    assert_eq!(plan["policy"]["max_files"], 123);
}
