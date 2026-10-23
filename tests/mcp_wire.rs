use code_context::shared_mcp::{dispatch, serve};
use serde_json::json;
use std::io::Cursor;

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
    assert!(serve(Cursor::new(vec![b'a'; 1024 * 1024 + 2]), Vec::new()).is_err());
    let input = b"invalid\n{\"jsonrpc\":\"2.0\",\"id\":\"x\",\"method\":\"ping\"}\n";
    let mut output = Vec::new();
    serve(Cursor::new(input), &mut output).unwrap();
    let values: Vec<serde_json::Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(values[0]["error"]["code"], -32700);
    assert_eq!(values[1]["id"], "x");
}
