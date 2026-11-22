use code_context::shared_mcp::dispatch;
use serde_json::json;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct PrivateDaemon(Child);
impl Drop for PrivateDaemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn shared_mcp_routes_host_dispatch_delays_without_relaxing_policy_or_repeating_work() {
    let initialized = dispatch(json!({"jsonrpc":"2.0","id":1,"method":"initialize"})).unwrap();
    let instructions = initialized["result"]["instructions"].as_str().unwrap();
    for expected in [
        "prefer these backend MCP tools",
        "Do not duplicate pending lookups",
        "cctx --backend shared",
        "Never switch transport to bypass",
    ] {
        assert!(instructions.contains(expected), "{expected}");
    }
    let listed = dispatch(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).unwrap();
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 7);
    for name in ["code_files", "code_grep", "code_context", "code_read"] {
        let tool = tools.iter().find(|tool| tool["name"] == name).unwrap();
        let description = tool["description"].as_str().unwrap();
        assert!(description.contains("backend MCP tools avoid native extension-host dispatch"));
        assert!(description.contains("Do not duplicate a pending native call"));
        if matches!(name, "code_grep" | "code_context") {
            assert!(description.contains("Set regex=true"));
            assert!(tool["inputSchema"]["properties"]["pattern"]["description"]
                .as_str().unwrap().contains("not semantic or keyword search"));
            assert_eq!(tool["inputSchema"]["properties"]["regex"]["default"], false);
        }
    }
}

#[test]
fn shared_mcp_rejects_invalid_request_shapes_and_identifiers() {
    for input in [
        json!([]),
        json!({"jsonrpc":"2.0","method":3}),
        json!({"jsonrpc":"2.0","method":"ping","id":{}}),
    ] {
        assert_eq!(dispatch(input).unwrap()["error"]["code"], -32600);
    }
    assert!(dispatch(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
    let value = dispatch(json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"code_grep","arguments":{"root":"relative","pattern":"x"}}}))
    .unwrap();
    assert_eq!(value["result"]["isError"], true);
}

#[test]
fn shared_stdio_enforces_frame_budget_and_json_only_output() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("state");
    let log = directory.path().join("daemon.log");
    let binary = env!("CARGO_BIN_EXE_cctx");
    let mut daemon = PrivateDaemon(Command::new(binary).env("CCTX_HOME", &home)
        .env_remove("CCTX_IO_GUARD").env_remove("CCTX_MEMORY_GUARD")
        .arg("daemon").stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap()).spawn().unwrap());
    let started = Instant::now();
    loop {
        let ping = Command::new(binary).env("CCTX_HOME", &home).arg("ping").output().unwrap();
        if ping.status.success() { break; }
        assert!(daemon.0.try_wait().unwrap().is_none(), "{}", std::fs::read_to_string(&log).unwrap());
        assert!(started.elapsed() < Duration::from_secs(5), "private daemon did not become ready");
        std::thread::sleep(Duration::from_millis(2));
    }
    let run = |input: &[u8]| {
        let mut child = Command::new(binary).env("CCTX_HOME", &home)
            .arg("shared-mcp").stdin(Stdio::piped())
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        if let Err(error) = child.stdin.take().unwrap().write_all(input) {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        }
        child.wait_with_output().unwrap()
    };
    let oversized = run(&vec![b'a'; 1024 * 1024 + 2]);
    assert!(!oversized.status.success());
    assert!(String::from_utf8_lossy(&oversized.stderr).contains("MCP input exceeds 1 MiB"));
    let input = b"invalid\n{\"jsonrpc\":\"2.0\",\"id\":\"x\",\"method\":\"ping\"}\n";
    let output = run(input);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let values: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(values[0]["error"]["code"], -32700);
    assert_eq!(values[1]["id"], "x");
}

#[test]
fn shared_mcp_coerces_subagent_aliases_off_by_one_ranges_and_mixed_hidden_paths() {
    let directory = tempfile::tempdir().unwrap();
    let repo = directory.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let lines: String = (1..=60).map(|i| format!("line {i}\n")).collect();
    std::fs::write(repo.join("src/lib.rs"), lines).unwrap();
    let ws = code_context::workspace::Workspace::new(
        &repo,
        vec![".".into()],
        vec!["*.rs".into()],
    )
    .unwrap();

    // 1. Off-by-one pagination (start=51, end=50) expands forward instead of swapping to 50..51
    let read_res = code_context::mcp::call(
        &ws,
        "code_read",
        json!({
            "ranges": [
                {"path": "src/lib.rs", "start": "51", "end": "50"},
                {"path": "src/lib.rs", "start": 51, "end": 60}
            ]
        }),
    )
    .unwrap();
    let arr = read_res.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["start"], 51);
    assert_eq!(arr[0]["end"], 60);

    // 2. Context clamps limit=100 -> 64 and radius="120" -> 100
    let ctx_res = code_context::mcp::call(
        &ws,
        "code_context",
        json!({
            "query": "line 10",
            "limit": "100",
            "radius": "120",
            "isRegexp": "false"
        }),
    )
    .unwrap();
    assert_eq!(ctx_res["search"]["total_matches"], 1);
}

#[test]
fn shared_mcp_initializes_without_daemon_negotiates_version_and_supports_content_length_and_init_cli() {
    use std::io::{BufRead, BufReader, Read, Write};
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("cctx-home");
    let fake_user_home = temp.path().join("user-home");
    let repo = fake_user_home.join("dev/demo");
    let subdir = repo.join("packages/sub");
    std::fs::create_dir_all(&subdir).unwrap();
    let git_init = Command::new("git")
        .arg("init")
        .arg("-q")
        .arg(&repo)
        .output()
        .unwrap();
    assert!(git_init.status.success());
    std::fs::write(subdir.join("app.rs"), "pub fn init_ready_probe() -> usize { 42 }\n").unwrap();
    let binary = env!("CARGO_BIN_EXE_cctx");

    // 1. Start shared-mcp BEFORE any daemon exists and with cwd = $HOME:
    //    initialize + tools/list must succeed, negotiate protocolVersion, support Content-Length,
    //    and never bind $HOME into MCP instructions.
    let mut mcp_proc = Command::new(binary)
        .env("CCTX_HOME", &home)
        .env("HOME", &fake_user_home)
        .env("CCTX_NO_AUTO_START", "1")
        .current_dir(&fake_user_home)
        .arg("shared-mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = mcp_proc.stdin.take().unwrap();
    let mut stdout = BufReader::new(mcp_proc.stdout.take().unwrap());

    let init_body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {}
        }
    }))
    .unwrap();
    write!(
        stdin,
        "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
        init_body.len()
    )
    .unwrap();
    stdin.write_all(&init_body).unwrap();
    stdin.flush().unwrap();

    let mut hdr_line = String::new();
    stdout.read_line(&mut hdr_line).unwrap();
    assert!(hdr_line.starts_with("Content-Length: "), "expected Content-Length reply, got {hdr_line:?}");
    let reply_len: usize = hdr_line["Content-Length: ".len()..].trim().parse().unwrap();
    let mut sep_line = String::new();
    stdout.read_line(&mut sep_line).unwrap();
    assert_eq!(sep_line, "\r\n");
    let mut reply_buf = vec![0_u8; reply_len];
    stdout.read_exact(&mut reply_buf).unwrap();
    let init_reply: serde_json::Value = serde_json::from_slice(&reply_buf).unwrap();
    assert_eq!(init_reply["result"]["protocolVersion"], "2024-11-05");
    let instructions = init_reply["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("root=<abs_root>"), "unexpected home root binding: {instructions}");

    // 2. Now start private daemon and run `cctx --root <repo> init` (idempotent CLI initialization).
    struct DaemonGuard(std::process::Child);
    impl Drop for DaemonGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut daemon_child = DaemonGuard(
        Command::new(binary)
            .env("CCTX_HOME", &home)
            .env("HOME", &fake_user_home)
            .env_remove("CCTX_IO_GUARD")
            .env_remove("CCTX_MEMORY_GUARD")
            .arg("daemon")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let wait_start = Instant::now();
    loop {
        if Command::new(binary).env("CCTX_HOME", &home).arg("ping").output().unwrap().status.success() {
            break;
        }
        assert!(daemon_child.0.try_wait().unwrap().is_none());
        assert!(wait_start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(5));
    }

    let refused_home = Command::new(binary)
        .env("CCTX_HOME", &home)
        .env("HOME", &fake_user_home)
        .arg("--root")
        .arg(&fake_user_home)
        .arg("init")
        .output()
        .unwrap();
    assert!(!refused_home.status.success(), "cctx init must refuse $HOME without --allow-root-scope");

    let init_first = Command::new(binary)
        .env("CCTX_HOME", &home)
        .env("HOME", &fake_user_home)
        .arg("--root")
        .arg(&repo)
        .arg("init")
        .output()
        .unwrap();
    assert!(init_first.status.success(), "{}", String::from_utf8_lossy(&init_first.stderr));
    let init_json: serde_json::Value = serde_json::from_slice(&init_first.stdout).unwrap();
    assert_eq!(init_json["initialized"], true);
    assert_eq!(init_json["registration"], "registered");
    assert_eq!(init_json["daemon_ready"], true);

    let init_second = Command::new(binary)
        .env("CCTX_HOME", &home)
        .env("HOME", &fake_user_home)
        .arg("--root")
        .arg(&repo)
        .arg("init")
        .output()
        .unwrap();
    assert!(init_second.status.success());
    let init_json2: serde_json::Value = serde_json::from_slice(&init_second.stdout).unwrap();
    assert_eq!(init_json2["registration"], "existing");

    // Unauthorized nested Git repo beneath registered `repo` (while allow_worktrees=false) must NOT bypass policy
    let unauthorized_nested = repo.join("nested-untrusted");
    std::fs::create_dir_all(&unauthorized_nested).unwrap();
    assert!(
        Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(&unauthorized_nested)
            .output()
            .unwrap()
            .status
            .success()
    );
    let refused_nested = Command::new(binary)
        .env("CCTX_HOME", &home)
        .env("HOME", &fake_user_home)
        .arg("--root")
        .arg(&unauthorized_nested)
        .arg("init")
        .output()
        .unwrap();
    assert!(
        !refused_nested.status.success(),
        "cctx init must refuse unauthorized nested git repo beneath registered parent"
    );

    // Authorize worktrees on repo and verify code_warm_hint on a subdirectory of allow_worktrees=true queues/warms cleanly
    let auth_out = Command::new(binary)
        .env("CCTX_HOME", &home)
        .arg("--root")
        .arg(&repo)
        .arg("authorize-worktrees")
        .output()
        .unwrap();
    assert!(auth_out.status.success());

    // 3. Using the SAME already-open shared-mcp process (which started when daemon was down & cwd=$HOME!),
    //    send a JSONL initialize with workspaceFolders pointing at subdir, then read app.rs without passing root!
    let repo_uri = format!("file://{}", subdir.display());
    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "workspaceFolders": [{"uri": repo_uri, "name": "sub"}]
            }
        })
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "code_read",
                "arguments": {
                    "ranges": [{"path": format!("{}/app.rs", subdir.display()), "start": 1, "end": 5}]
                }
            }
        })
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut line2 = String::new();
    stdout.read_line(&mut line2).unwrap();
    let resp2: serde_json::Value = serde_json::from_str(&line2).unwrap();
    assert_eq!(resp2["result"]["protocolVersion"], "2025-11-25");

    let mut line3 = String::new();
    stdout.read_line(&mut line3).unwrap();
    let resp3: serde_json::Value = serde_json::from_str(&line3).unwrap();
    assert_eq!(resp3["result"]["isError"], false, "expected lazy daemon connect + root inference to succeed: {resp3}");
    let resp3_text = resp3["result"]["content"][0]["text"].as_str().unwrap();
    assert!(resp3_text.contains("init_ready_probe"));
    let footer_text = resp3["result"]["content"][1]["text"].as_str().unwrap();
    let footer_json: serde_json::Value =
        serde_json::from_str(footer_text.strip_prefix("CCTX_TRACE ").expect("expected CCTX_TRACE footer")).unwrap();
    assert!(footer_json["total_ms"].as_f64().is_some_and(|ms| ms >= 0.0), "expected numeric total_ms in CCTX_TRACE footer: {footer_json}");

    drop(stdin);
    let status = mcp_proc.wait().unwrap();
    assert!(status.success());

    // 4. Blackhole / unresponsive daemon socket MUST NOT block MCP `initialize` or `tools/list`.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let hung_home = temp.path().join("hung-cctx-home");
        std::fs::create_dir_all(&hung_home).unwrap();
        std::fs::set_permissions(&hung_home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let hung_sock = hung_home.join("daemon.sock");
        let blackhole = std::os::unix::net::UnixListener::bind(&hung_sock).unwrap();
        std::fs::set_permissions(&hung_sock, std::fs::Permissions::from_mode(0o600)).unwrap();
        let _accepted_holder = std::thread::spawn(move || {
            let mut held = Vec::new();
            for conn in blackhole.incoming().take(2).flatten() {
                held.push(conn);
                std::thread::sleep(Duration::from_secs(5));
            }
        });
        let started = Instant::now();
        let mut hung_mcp = Command::new(binary)
            .env("CCTX_HOME", &hung_home)
            .env("HOME", &fake_user_home)
            .env("CCTX_NO_AUTO_START", "1")
            .current_dir(&repo)
            .arg("shared-mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut h_in = hung_mcp.stdin.take().unwrap();
        let mut h_out = BufReader::new(hung_mcp.stdout.take().unwrap());
        writeln!(
            h_in,
            "{}",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})
        )
        .unwrap();
        writeln!(h_in, "{}", json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).unwrap();
        h_in.flush().unwrap();
        let mut r1 = String::new();
        let mut r2 = String::new();
        h_out.read_line(&mut r1).unwrap();
        h_out.read_line(&mut r2).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "MCP initialize/tools/list blocked on unresponsive daemon: {:?}",
            started.elapsed()
        );
        let v1: serde_json::Value = serde_json::from_str(&r1).unwrap();
        let v2: serde_json::Value = serde_json::from_str(&r2).unwrap();
        assert_eq!(v1["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(v2["result"]["tools"].as_array().unwrap().len(), 7);
        drop(h_in);
        let _ = hung_mcp.wait();
    }
}
