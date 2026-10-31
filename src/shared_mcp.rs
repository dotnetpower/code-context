use crate::daemon::{self, Request};
use crate::mcp::MAX_MESSAGE;
use crate::{Result, invalid};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Instant;

const CACHE_GUIDANCE: &str = "CCTX is an optional nonblocking root cache. CACHE_MISS returns immediately and queues only the requested root; continue that lookup with permitted host search/read. Do not poll/repeat, switch CCTX transports or run code_sync to wait for warmup. CACHE_LOAD_FAILED reports the load failure. Neither means zero matches or a missing disk file. Startup and legacy prewarm flags do not load/pin source. Inactive roots expire after15minutes and may be evicted earlier by LRU/memory limits. Do not enumerate sibling worktrees speculatively. Explicitly needed code_sync remains a waiting barrier.";

pub fn tools() -> Value {
    let mut tools = crate::mcp::tools();
    let items = tools["tools"]
        .as_array_mut()
        .expect("static tool schema is an array");
    for tool in items.iter_mut() {
        tool["inputSchema"]["properties"]["root"] = json!({"type":"string","description":"Optional absolute registered project root. Omit for the current workspace; the server resolves only an explicitly registered ancestor."});
        if tool["name"] != "code_read" && tool["name"] != "code_files" {
            tool["inputSchema"]["properties"]["paths"] = json!({"type":"array","maxItems":64,"items":{"type":"string"},"description":"Optional relative path prefixes."});
            tool["inputSchema"]["properties"]["glob"] = json!({"type":"string","description":"Optional filename/path glob."});
            tool["inputSchema"]["properties"]["max_matches_per_file"] = json!({"type":"integer","minimum":1,"maximum":64,"description":"Optional per-file match cap; prioritizes symbol definitions."});
        }
        if tool["name"] == "code_read" {
            tool["inputSchema"]["properties"]["page"] = json!({"type":"boolean","default":false,
                "description":"Paginate large reads (<256KiB) returning snippets, complete, and next_ranges."});
            tool["inputSchema"]["properties"]["split_ranges"] = json!({
                "type":"boolean","default":true,
                "description":"Split broad ranges into <=500-line segments (max 64 segments, 256KiB)."
            });
        }
        let prefix = match tool["name"].as_str() {
            Some("code_files") => {
                "Use FIRST for filename/path glob discovery, NOT content."
            }
            Some("code_grep") => {
                "Use FIRST for saved-source search. Literal by default; Set regex=true for alternation (a|b)."
            }
            Some("code_context") => {
                "Use FIRST for a symbol plus nearby saved source in ONE call. Literal by default; Set regex=true for alternation (a|b)."
            }
            _ => {
                "Use FIRST for known saved-source ranges with live SHA-256 verification."
            }
        };
        tool["description"] = json!(format!(
            "{prefix} CACHE_MISS returns immediately and schedules only this root; continue with permitted host search/read. These backend MCP tools avoid native extension-host dispatch in background sessions. Do not duplicate a pending native call or run code_sync to wait."
        ));
    }
    for (name, description) in [
        (
            "code_status",
            "Show index state (including unloaded), cache queue/misses/load errors, coverage and memory without loading, reconciling or hashing source. Ordinary cold lookups return CACHE_MISS immediately and schedule only that root; no startup prewarm.",
        ),
        (
            "code_sync",
            "Explicit full-source synchronization barrier: recheck ignore policy, hash permitted source, update changed files; use before exhaustive absence claims or after edits.",
        ),
    ] {
        items.push(json!({"name":name,"description":description,
            "inputSchema":{"type":"object","properties":{"root":{"type":"string","description":"Optional absolute registered root; omit for current workspace."}},"additionalProperties":false},
            "annotations":{"readOnlyHint":name=="code_status","destructiveHint":false,"openWorldHint":false}}));
    }
    items.push(json!({"name":"code_workspaces","description":"List explicitly registered source roots and permitted scopes/globs. Use the current workspace root for all searches.",
        "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}));
    tools
}

pub fn dispatch(request: Value) -> Option<Value> {
    let mut pending_traces = std::collections::VecDeque::new();
    dispatch_with_root(request, None, None, &mut pending_traces)
}

fn flush_pending_traces(
    client: &mut daemon::Client,
    pending_traces: &mut std::collections::VecDeque<Value>,
) {
    while let Some(trace) = pending_traces.pop_front() {
        if let Err(error) = client.call(&Request {
            root: None,
            name: "code_trace".into(),
            arguments: trace,
            compact: true,
        }) {
            eprintln!("CCTX_TRACE_REPORT_FAILED: {error}");
        }
    }
}

fn dispatch_with_root(
    request: Value,
    default_root: Option<&PathBuf>,
    client: Option<&mut daemon::Client>,
    pending_traces: &mut std::collections::VecDeque<Value>,
) -> Option<Value> {
    if !request.is_object() || request["jsonrpc"] != "2.0" || !request["method"].is_string() {
        return Some(
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"invalid JSON-RPC request"}}),
        );
    }
    let id = request.get("id")?.clone();
    if !(id.is_null() || id.is_string() || id.is_i64() || id.is_u64()) {
        return Some(
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"request ID must be a string or integer"}}),
        );
    }
    let method = request["method"].as_str()?;
    let result = match method {
        "initialize" => Ok(
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},
            "serverInfo":{"name":"code-context","version":env!("CARGO_PKG_VERSION")},
            "instructions":format!("{CACHE_GUIDANCE} Use these tools before shell search/read for saved files in a registered workspace. Use code_files for filename/path glob discovery, not a content pattern. Omit root for the current workspace. Prefer code_context for surrounding source, code_grep for content, code_read for known files. In a background session where native tools depend on unavailable foreground host dispatch, prefer these backend MCP tools. Do not duplicate pending lookups or run code_sync to repair host dispatch. If both client surfaces are unavailable but a backend terminal is usable, use cctx --backend shared with the same authorized root, except CACHE_MISS requires a permitted host read instead. Never switch transport to bypass content exclusions, trust decisions or access denials. Empty results do not cover files outside registered scopes/globs. Hidden/ignored/excluded targets require policy-aware host tools. No editor buffers or cross-root search.")}),
        ),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools()),
        "tools/call" => {
            let started = Instant::now();
            let mut trace = client.as_ref().filter(|c| c.supports_tracing()).map(|_| json!({
                "trace_id":daemon::new_trace_id(),"origin":"mcp",
                "started_unix_ms":daemon::trace_unix_ms(),"completed_unix_ms":null,
                "operation":request["params"]["name"],"root":request["params"]["arguments"]["root"],
                "queue_ms":0.0,"prepare_ms":0.0,"ipc_ms":0.0,"available":true,
                "serialize_ms":0.0,"write_ms":0.0,"total_ms":0.0,"success":false,
                "daemon":null,
            }));
            let outcome = tool_call(
                &request["params"],
                default_root,
                client,
                &mut trace,
                pending_traces,
            );
            if let Some(trace) = trace.as_mut() {
                trace["prepare_ms"] = json!(
                    (started.elapsed().as_secs_f64() * 1000.0
                        - trace["ipc_ms"].as_f64().expect("trace IPC time"))
                    .max(0.0)
                );
            }
            let formatting = Instant::now();
            let mut result = match outcome {
                Ok(text) => {
                    json!({"content":[{"type":"text","text":text}],"isError":false})
                }
                Err(error) => {
                    json!({"content":[{"type":"text","text":error.to_string()}],"isError":true})
                }
            };
            if let Some(mut trace) = trace {
                trace["serialize_ms"] = json!(formatting.elapsed().as_secs_f64() * 1000.0);
                trace["total_ms"] = json!(started.elapsed().as_secs_f64() * 1000.0);
                trace["completed_unix_ms"] = json!(daemon::trace_unix_ms());
                trace["success"] = json!(result["isError"] == false);
                let compact_footer = json!({
                    "trace_id": trace["trace_id"],
                    "operation": trace["operation"],
                    "success": trace["success"],
                });
                result["content"]
                    .as_array_mut()
                    .expect("tool content")
                    .push(json!({
                        "type":"text","text":format!("CCTX_TRACE {compact_footer}")
                    }));
                result["_meta"] = json!({"cctx_trace":trace});
            }
            Ok(result)
        }
        _ => Err((-32601, "unknown method")),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err((code, message)) => {
            json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
        }
    })
}

fn tool_call(
    params: &Value,
    default_root: Option<&PathBuf>,
    client: Option<&mut daemon::Client>,
    trace: &mut Option<Value>,
    pending_traces: &mut std::collections::VecDeque<Value>,
) -> Result<String> {
    let prepare_started = Instant::now();
    let name = params["name"]
        .as_str()
        .ok_or_else(|| invalid("tool name required"))?;
    if ![
        "code_files",
        "code_grep",
        "code_read",
        "code_context",
        "code_status",
        "code_sync",
        "code_workspaces",
    ]
    .contains(&name)
    {
        return Err(invalid("unknown tool").into());
    }
    let mut arguments = params["arguments"]
        .as_object()
        .ok_or_else(|| invalid("arguments must be an object"))?
        .clone();
    let root = arguments
        .remove("root")
        .map(serde_json::from_value::<PathBuf>)
        .transpose()?;
    if root.as_ref().is_some_and(|path| !path.is_absolute()) {
        return Err(invalid("root must be absolute when provided").into());
    }
    if name == "code_workspaces" && (!arguments.is_empty() || root.is_some()) {
        return Err(invalid("code_workspaces takes empty arguments").into());
    }
    let root = if name == "code_workspaces" {
        None
    } else {
        Some(match root {
            Some(root) => root,
            None => default_root
                .cloned()
                .ok_or_else(|| invalid("current directory is not inside a registered workspace"))?,
        })
    };
    if arguments.contains_key(daemon::TRACE_ARGUMENT)
        || arguments.contains_key(daemon::PREV_TRACE_ARGUMENT)
    {
        return Err(invalid("reserved internal trace argument").into());
    }
    if (name == "code_status" || pending_traces.len() >= 64) && !pending_traces.is_empty() {
        let items: Vec<Value> = pending_traces.drain(..).collect();
        arguments.insert(daemon::PREV_TRACE_ARGUMENT.into(), Value::Array(items));
    }
    if let Some(trace) = trace.as_mut() {
        trace["root"] = json!(root);
        arguments.insert(daemon::TRACE_ARGUMENT.into(), trace["trace_id"].clone());
        trace["prepare_ms"] = json!(prepare_started.elapsed().as_secs_f64() * 1000.0);
    }
    let request = Request {
        root,
        name: name.into(),
        arguments: Value::Object(arguments),
        compact: true,
    };
    let ipc_started = Instant::now();
    if let Some(client) = client {
        let raw_result = match client.call_raw(&request) {
            Ok(bytes) => Ok(bytes),
            Err(error) if daemon::is_application_error(error.as_ref()) => Err(error),
            Err(first) => {
                *client = daemon::Client::connect_traced().map_err(|reconnect| {
                    invalid(format!(
                        "daemon request failed ({first}); reconnect failed ({reconnect})"
                    ))
                })?;
                client.call_raw(&request)
            }
        };
        if let Some(trace) = trace.as_mut() {
            trace["ipc_ms"] = json!(ipc_started.elapsed().as_secs_f64() * 1000.0);
        }
        let raw_bytes = raw_result?;
        let text = if let Some(trace) = trace.as_mut() {
            let expected_id = trace["trace_id"].as_str().expect("generated trace ID");
            let (raw_value, daemon_trace) = daemon::unwrap_trace_raw(&raw_bytes, expected_id)?;
            trace["daemon"] = daemon_trace;
            raw_value.get().to_owned()
        } else {
            String::from_utf8(raw_bytes)?
        };
        if name == "code_status" {
            let mut value: Value = serde_json::from_str(&text)?;
            let object = value
                .as_object_mut()
                .ok_or_else(|| invalid("daemon status is not an object"))?;
            object.insert("mcp_client".into(), crate::diagnostics::capabilities()?);
            return Ok(serde_json::to_string(&value)?);
        }
        return Ok(text);
    }
    let mut value = daemon::request(&request)?;
    if name == "code_status" {
        let object = value
            .as_object_mut()
            .ok_or_else(|| invalid("daemon status is not an object"))?;
        object.insert("mcp_client".into(), crate::diagnostics::capabilities()?);
    }
    Ok(serde_json::to_string(&value)?)
}

pub fn serve(input: impl BufRead, mut output: impl Write) -> Result<()> {
    let mut input = input;
    let default_root = Some(crate::catalog::canonicalize(&std::env::current_dir()?)?);
    let mut client = daemon::Client::connect_traced()?;
    let mut pending_traces: std::collections::VecDeque<Value> =
        std::collections::VecDeque::with_capacity(64);
    let trace_stderr = std::env::var_os("CCTX_TRACE_STDERR").is_some();
    let mut bytes = Vec::with_capacity(512);
    let mut out_buf = Vec::with_capacity(4096);
    loop {
        bytes.clear();
        let count = std::io::Read::take(&mut input, MAX_MESSAGE as u64 + 1)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            flush_pending_traces(&mut client, &mut pending_traces);
            return Ok(());
        }
        if count > MAX_MESSAGE {
            flush_pending_traces(&mut client, &mut pending_traces);
            return Err(invalid("MCP input exceeds 1 MiB").into());
        }
        let started = Instant::now();
        let response = match serde_json::from_slice(&bytes) {
            Ok(value) => dispatch_with_root(
                value,
                default_root.as_ref(),
                Some(&mut client),
                &mut pending_traces,
            ),
            Err(error) => Some(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":error.to_string()}}),
            ),
        };
        if let Some(response) = response {
            let ser_started = Instant::now();
            out_buf.clear();
            serde_json::to_writer(&mut out_buf, &response)?;
            out_buf.push(b'\n');
            let serialize_ms = ser_started.elapsed().as_secs_f64() * 1000.0;
            let write_started = Instant::now();
            output.write_all(&out_buf)?;
            output.flush()?;
            let write_ms = write_started.elapsed().as_secs_f64() * 1000.0;
            if let Some(mut trace) = response["result"]["_meta"].get("cctx_trace").cloned() {
                trace["serialize_ms"] = json!(
                    trace["serialize_ms"]
                        .as_f64()
                        .expect("trace serialization time")
                        + serialize_ms
                );
                trace["write_ms"] = json!(write_ms);
                trace["total_ms"] = json!(started.elapsed().as_secs_f64() * 1000.0);
                trace["completed_unix_ms"] = json!(daemon::trace_unix_ms());
                if trace_stderr {
                    eprintln!("CCTX_CLIENT_TRACE {trace}");
                }
                if pending_traces.len() >= 64 {
                    pending_traces.pop_front();
                }
                pending_traces.push_back(trace);
            }
        }
    }
}
