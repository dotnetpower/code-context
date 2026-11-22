use crate::context::{ContextRequest, ReadRequest};
use crate::engine::{Backend, Engine};
use crate::search::Search;
use crate::workspace::Workspace;
use crate::{Result, invalid};
use serde_json::{Value, json};
use std::io::{BufRead, Write};

pub const MAX_MESSAGE: usize = 1024 * 1024;

pub fn tools() -> Value {
    let search_properties = json!({
        "pattern": {"type":"string", "minLength":1,
            "description":"Literal default, not semantic or keyword search. Set regex=true for a|b."},
        "regex": {"type":"boolean", "default":false},
        "ignore_case": {"type":"boolean", "default":false},
        "limit": {"type":"integer", "minimum":1, "maximum":1000, "default":200}
    });
    let mut context_properties = search_properties.clone();
    context_properties["limit"] = json!({"type":"integer","minimum":1,"maximum":64,"default":20});
    context_properties["radius"] = json!({"type":"integer","minimum":0,"maximum":100,"default":3});
    json!({"tools":[
        {
            "name":"code_files",
            "description":"Find files by filename, path, or glob pattern without reading file contents.",
            "inputSchema":{"type":"object","properties":{
                "glob":{"type":"string","minLength":1,"maxLength":1024,"description":"Path/name glob (e.g. *.rs, src/**/*.ts)."},
                "paths":{"type":"array","maxItems":64,"items":{"type":"string"}},
                "limit":{"type":"integer","minimum":1,"maximum":1000,"default":200}
            },"required":["glob"],"additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}
        },
        {
            "name":"code_grep",
            "description":"Search workspace source lines for exact text or regex matches (literal default; regex=true for alternation).",
            "inputSchema":{"type":"object","properties":search_properties,"required":["pattern"],"additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}
        },
        {
            "name":"code_read",
            "description":"Read file lines, batched 1-based line ranges, or markdown heading sections (<=150 lines).",
            "inputSchema":{"type":"object","properties":{"ranges":{"type":"array","minItems":1,"maxItems":64,"items":{"type":"object","properties":{"path":{"type":"string"},"start":{"type":"integer","minimum":1},"end":{"type":"integer","minimum":1}},"required":["path","start","end"],"additionalProperties":false}}},"required":["ranges"],"additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}
        },
        {
            "name":"code_context",
            "description":"Find symbol definitions, references, callers, and surrounding function body in 1 call.",
            "inputSchema":{"type":"object","properties":context_properties,"required":["pattern"],"additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}
        }
    ]})
}

pub fn call(workspace: &Workspace, name: &str, arguments: Value) -> Result<Value> {
    call_engine(&mut Engine::from(workspace.clone()), name, arguments)
}

pub fn call_engine(engine: &mut Engine, name: &str, mut arguments: Value) -> Result<Value> {
    if matches!(name, "code_files" | "code_grep" | "code_read" | "code_context")
        && let Some(map) = arguments.as_object_mut()
    {
        crate::shared_mcp::normalize_mcp_argument_aliases(name, map);
    }
    match name {
        "code_files" => Ok(serde_json::to_value(engine.files(
            &serde_json::from_value::<crate::files::FileQuery>(arguments)?,
        )?)?),
        "code_grep" => Ok(serde_json::to_value(
            engine.search(&serde_json::from_value::<Search>(arguments)?)?,
        )?),
        "code_read" => Ok(serde_json::to_value(
            engine.read_many(&serde_json::from_value::<ReadRequest>(arguments)?.ranges)?,
        )?),
        "code_context" => {
            let mut arguments = arguments;
            if arguments.get("limit").is_none()
                && let Some(object) = arguments.as_object_mut()
            {
                object.insert("limit".into(), json!(20));
            }
            Ok(serde_json::to_value(engine.context(
                &serde_json::from_value::<ContextRequest>(arguments)?,
            )?)?)
        }
        "code_refresh" => {
            if arguments != json!({}) {
                return Err(invalid("code_refresh accepts an empty arguments object").into());
            }
            engine.refresh()
        }
        "code_status" => {
            if arguments != json!({}) {
                return Err(invalid("code_status accepts an empty arguments object").into());
            }
            Ok(engine.status())
        }
        _ => Err(invalid(format!("unknown tool: {name}")).into()),
    }
}

pub fn handle(workspace: &Workspace, request: Value) -> Option<Value> {
    let mut engine = Engine::from(workspace.clone());
    handle_engine(&mut engine, request)
}

pub fn handle_engine(engine: &mut Engine, request: Value) -> Option<Value> {
    let id = request.get("id")?.clone();
    let method = request["method"].as_str().unwrap_or("");
    let params = &request["params"];
    let result = match method {
        "initialize" => {
            let version = params["protocolVersion"].as_str().unwrap_or("");
            let supported = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
            Ok(json!({
                "protocolVersion":if supported.contains(&version) { version } else { "2025-06-18" },
                "capabilities":{"tools":{"listChanged":false}},
                "serverInfo":{"name":"code-context-experiment","version":env!("CARGO_PKG_VERSION")},
                "instructions":format!("Read-only experiment. Root={}, scopes={:?}, globs={:?}. Backend={:?}. Resident backends serve immutable saved-file snapshots; refresh explicitly after changes; revalidate live files before editing. No editor buffers.", engine.workspace.root.display(), engine.workspace.scopes, engine.workspace.globs, engine.backend)
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_for(engine.backend)),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let outcome = call_engine(engine, name, params["arguments"].clone());
            Ok(match outcome {
                Ok(value) => {
                    json!({"content":[{"type":"text","text":value.to_string()}],"isError":false})
                }
                Err(error) => {
                    json!({"content":[{"type":"text","text":error.to_string()}],"isError":true})
                }
            })
        }
        _ => Err((-32601, format!("unknown method: {method}"))),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err((code, message)) => {
            json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
        }
    })
}

pub fn serve(workspace: &Workspace, input: impl BufRead, mut output: impl Write) -> Result<()> {
    serve_engine(&mut Engine::from(workspace.clone()), input, &mut output)
}

pub(crate) fn write_response(output: &mut impl Write, response: &Value) -> Result<(f64, f64)> {
    let started = std::time::Instant::now();
    let mut bytes = Vec::with_capacity(512);
    serde_json::to_writer(&mut bytes, response)?;
    bytes.push(b'\n');
    let serialize_ms = started.elapsed().as_secs_f64() * 1000.0;
    let started = std::time::Instant::now();
    output.write_all(&bytes)?;
    output.flush()?;
    Ok((serialize_ms, started.elapsed().as_secs_f64() * 1000.0))
}

fn tools_for(backend: Backend) -> Value {
    let mut tools = tools();
    if backend != Backend::Live
        && let Some(items) = tools["tools"].as_array_mut()
    {
        for tool in items
            .iter_mut()
            .filter(|t| t["name"] != "code_read" && t["name"] != "code_files")
        {
            let purpose = if tool["name"] == "code_context" {
                "Find a literal and return matching lines PLUS surrounding source in one call; merge overlapping ranges."
            } else {
                "Like rg -F: find a literal and return matching paths, 1-based lines, text, total count and truncation."
            };
            tool["description"] = json!(format!(
                "{purpose} Searches the configured immutable RAM snapshot, NOT live files. Call code_refresh after filesystem or ignore-policy changes. Case-sensitive single-line literals only; no regex or case folding."
            ));
            if let Some(properties) = tool["inputSchema"]["properties"].as_object_mut() {
                properties.remove("regex");
                properties.remove("ignore_case");
            }
        }
        items.push(json!({
                "name":"code_refresh",
                "description":"Explicitly rebuild the RAM snapshot/index from currently permitted saved files. Replaces the old generation only on success. Full rebuild, not incremental.",
                "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
                "annotations":{"readOnlyHint":false,"destructiveHint":false,"openWorldHint":false}
            }));
        items.push(json!({
                "name":"code_status","description":"Show snapshot generation, construction times, source size and approximate owned-memory estimate. Does not check live freshness.",
                "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
                "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}
            }));
        items[1]["description"] = json!(
            "Read ranges from the immutable RAM snapshot, not current live files. Returns snapshot source SHA-256. Refresh after edits or ignore-policy changes; revalidate live files before editing."
        );
    }
    tools
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpFraming {
    JsonLines,
    ContentLength,
}

fn starts_header_block(line: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(line) else {
        return false;
    };
    let trimmed = text.trim_matches(['\r', '\n']);
    let Some((name, _)) = trimmed.split_once(':') else {
        return false;
    };
    let key = name.trim();
    key.eq_ignore_ascii_case("content-length") || key.eq_ignore_ascii_case("content-type")
}

pub(crate) fn read_mcp_frame<R: BufRead>(
    input: &mut R,
    bytes: &mut Vec<u8>,
    max_message: usize,
    exceed_msg: &'static str,
) -> Result<Option<McpFraming>> {
    loop {
        bytes.clear();
        let count = std::io::Read::take(&mut *input, max_message as u64 + 1)
            .read_until(b'\n', bytes)?;
        if count == 0 {
            return Ok(None);
        }
        if count > max_message {
            return Err(invalid(exceed_msg).into());
        }
        if starts_header_block(bytes) {
            let mut header_bytes = count;
            let mut header_lines: Vec<Vec<u8>> = vec![bytes.clone()];
            loop {
                let mut line_buf = Vec::with_capacity(128);
                let hdr_count = std::io::Read::take(&mut *input, 4096)
                    .read_until(b'\n', &mut line_buf)?;
                if hdr_count == 0 {
                    return Err(invalid("unexpected EOF in MCP headers").into());
                }
                header_bytes = header_bytes.saturating_add(hdr_count);
                if header_bytes > 16 * 1024 || header_lines.len() >= 32 {
                    return Err(invalid("MCP headers exceed limit").into());
                }
                if line_buf == b"\r\n" || line_buf == b"\n" {
                    break;
                }
                header_lines.push(line_buf);
            }
            let mut content_len: Option<usize> = None;
            for raw_hdr in &header_lines {
                let hdr_str = std::str::from_utf8(raw_hdr)
                    .map_err(|_| invalid("invalid UTF-8 in MCP header"))?
                    .trim_matches(['\r', '\n']);
                let (name, value) = hdr_str
                    .split_once(':')
                    .ok_or_else(|| invalid("malformed MCP header line"))?;
                if name.trim().eq_ignore_ascii_case("content-length") {
                    let digits = value.trim();
                    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                        return Err(invalid("invalid MCP Content-Length value").into());
                    }
                    let parsed = digits
                        .parse::<usize>()
                        .map_err(|_| invalid("invalid MCP Content-Length value"))?;
                    if let Some(prev) = content_len
                        && prev != parsed
                    {
                        return Err(invalid("conflicting MCP Content-Length headers").into());
                    }
                    content_len = Some(parsed);
                }
            }
            let body_len = content_len
                .ok_or_else(|| invalid("missing Content-Length in MCP header block"))?;
            if body_len > max_message {
                return Err(invalid(exceed_msg).into());
            }
            bytes.clear();
            bytes.resize(body_len, 0);
            std::io::Read::read_exact(input, bytes)?;
            return Ok(Some(McpFraming::ContentLength));
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        return Ok(Some(McpFraming::JsonLines));
    }
}

pub(crate) fn encode_mcp_frame(
    framing: McpFraming,
    response: &Value,
    out: &mut Vec<u8>,
) -> Result<()> {
    out.clear();
    match framing {
        McpFraming::JsonLines => {
            serde_json::to_writer(&mut *out, response)?;
            out.push(b'\n');
        }
        McpFraming::ContentLength => {
            let body = serde_json::to_vec(response)?;
            write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
            out.extend_from_slice(&body);
        }
    }
    Ok(())
}

pub fn serve_engine(
    engine: &mut Engine,
    input: impl BufRead,
    mut output: impl Write,
) -> Result<()> {
    let mut input = input;
    let mut bytes = Vec::with_capacity(512);
    let mut out_buf = Vec::with_capacity(1024);
    loop {
        let Some(framing) = read_mcp_frame(
            &mut input,
            &mut bytes,
            MAX_MESSAGE,
            "MCP message exceeds 1 MiB",
        )?
        else {
            return Ok(());
        };
        let response = match serde_json::from_slice(&bytes) {
            Ok(request) => handle_engine(engine, request),
            Err(error) => Some(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":error.to_string()}}),
            ),
        };
        if let Some(response) = response {
            if framing == McpFraming::JsonLines {
                write_response(&mut output, &response)?;
            } else {
                encode_mcp_frame(framing, &response, &mut out_buf)?;
                output.write_all(&out_buf)?;
                output.flush()?;
            }
        }
    }
}

#[cfg(test)]
mod write_tests {
    use super::*;

    #[derive(Default)]
    struct CountingWriter {
        bytes: Vec<u8>,
        writes: usize,
        flushes: usize,
    }
    impl Write for CountingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn mcp_response_is_one_write_instead_of_thousands_of_escaped_fragments() {
        let source = (0..1000)
            .map(|i| format!("line {i}: \"quoted\" \\value\n"))
            .collect::<String>();
        let response = json!({"jsonrpc":"2.0","id":1,
            "result":{"content":[{"type":"text","text":json!({"lines":source}).to_string()}]}});
        let mut fragmented = CountingWriter::default();
        serde_json::to_writer(&mut fragmented, &response).unwrap();
        fragmented.write_all(b"\n").unwrap();
        assert!(fragmented.writes > 1000);
        let mut batched = CountingWriter::default();
        write_response(&mut batched, &response).unwrap();
        assert_eq!(batched.writes, 1);
        assert_eq!(batched.flushes, 1);
        assert_eq!(batched.bytes, fragmented.bytes);
        assert_eq!(
            serde_json::from_slice::<Value>(&batched.bytes).unwrap(),
            response
        );
    }
}
