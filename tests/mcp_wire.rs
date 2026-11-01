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
