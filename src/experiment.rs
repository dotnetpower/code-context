use crate::context::{ContextRequest, ContextResult, ReadRange, Snippet, context, read_many};
use crate::engine::Backend;
use crate::search::{Search, SearchResult, search};
use crate::workspace::{Workspace, capture};
use crate::{Result, invalid};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Instant;

pub(crate) struct Client {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

impl Client {
    fn start(workspace: &Workspace) -> Result<Self> {
        Self::start_mode(workspace, Backend::Live)
    }

    pub(crate) fn start_mode(workspace: &Workspace, backend: Backend) -> Result<Self> {
        let mut command = binary(workspace)?;
        command.args([
            "--backend",
            match backend {
                Backend::Shared => {
                    return Err(invalid("snapshot benchmark does not use shared backend").into());
                }
                Backend::Live => "live",
                Backend::Ram => "ram",
                Backend::Index => "index",
            },
        ]);
        let mut child = command
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let input = child.stdin.take().ok_or_else(|| invalid("missing stdin"))?;
        let output = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| invalid("missing stdout"))?,
        );
        let mut client = Self {
            child,
            input,
            output,
            next_id: 0,
        };
        let result = client.rpc("initialize", json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"cctx-bench","version":"1"}}))?;
        if result["protocolVersion"] != "2025-06-18" {
            return Err(invalid("unexpected MCP initialization").into());
        }
        writeln!(
            client.input,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )?;
        client.input.flush()?;
        let tools = client.rpc("tools/list", json!({}))?;
        let expected = if backend == Backend::Live { 4 } else { 6 };
        if tools["tools"].as_array().map(Vec::len) != Some(expected) {
            return Err(invalid("unexpected MCP tool list").into());
        }
        Ok(client)
    }

    fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0","id":self.next_id,"method":method,"params":params})
        )?;
        self.input.flush()?;
        let mut line = String::new();
        if self.output.read_line(&mut line)? == 0 {
            return Err(invalid("MCP process exited without a response").into());
        }
        let response: Value = serde_json::from_str(&line)?;
        if response["id"] != self.next_id || response.get("error").is_some() {
            return Err(invalid(format!("invalid MCP response: {response}")).into());
        }
        Ok(response["result"].clone())
    }

    pub(crate) fn tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        let result = self.rpc("tools/call", json!({"name":name,"arguments":arguments}))?;
        if result["isError"] != false {
            return Err(invalid(format!("MCP tool error: {result}")).into());
        }

        let text = result["content"][0]["text"]
            .as_str()
            .ok_or_else(|| invalid("missing tool content"))?;
        Ok(serde_json::from_str(text)?)
    }

    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn binary(workspace: &Workspace) -> Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command.arg("--root").arg(&workspace.root);
    for scope in &workspace.scopes {
        command.arg("--scope").arg(scope);
    }
    for glob in &workspace.globs {
        command.arg("--glob").arg(glob);
    }
    Ok(command)
}

fn oracle(workspace: &Workspace, query: &str) -> Result<Vec<u8>> {
    let mut command = workspace.command();
    command
        .args([
            "--line-number",
            "--no-heading",
            "--color",
            "never",
            "--fixed-strings",
            "--",
        ])
        .arg(query)
        .args(&workspace.scopes);
    capture(&mut command)
}

pub(crate) fn native_context(workspace: &Workspace, query: &str) -> Result<Vec<u8>> {
    let mut command = workspace.command();
    command
        .args([
            "--line-number",
            "--no-heading",
            "--color",
            "never",
            "--fixed-strings",
            "--context",
            "3",
            "--",
        ])
        .arg(query)
        .args(&workspace.scopes);
    capture(&mut command)
}

pub(crate) fn check_oracle(workspace: &Workspace, query: &str) -> Result<usize> {
    let expected = String::from_utf8(oracle(workspace, query)?)?;
    let expected: BTreeSet<_> = expected
        .split_terminator('\n')
        .map(|line| line.strip_prefix("./").unwrap_or(line).to_owned())
        .collect();
    let result = search(
        workspace,
        &Search {
            pattern: query.into(),
            regex: false,
            ignore_case: false,
            limit: 1000,
        },
    )?;
    let actual: BTreeSet<_> = result
        .matches
        .iter()
        .map(|h| format!("{}:{}:{}", h.path, h.line, h.text))
        .collect();
    if result.truncated || actual != expected {
        return Err(invalid(format!(
            "oracle mismatch for {query:?}; narrow queries if over 1000 matches"
        ))
        .into());
    }
    Ok(actual.len())
}

fn ranges_for(result: &SearchResult) -> Vec<ReadRange> {
    result
        .matches
        .iter()
        .map(|hit| ReadRange {
            path: hit.path.clone(),
            start: hit.line.saturating_sub(3).max(1),
            end: hit.line + 3,
        })
        .collect()
}

#[derive(Serialize)]
struct Sample {
    milliseconds: f64,
    output_bytes: usize,
    tool_calls: usize,
}

fn sample(workspace: &Workspace, client: &mut Client, mode: usize, query: &str) -> Result<Sample> {
    let start = Instant::now();
    let arguments = json!({"pattern":query,"limit":8});
    let (bytes, calls) = match mode {
        0 => (oracle(workspace, query)?.len(), 1),
        1 => {
            let mut command = binary(workspace)?;
            command.args(["--backend", "live", "grep", query, "--limit", "8"]);
            (capture(&mut command)?.len(), 1)
        }
        2 => (client.tool("code_grep", arguments)?.to_string().len(), 1),
        3 => {
            let hits = client.tool("code_grep", arguments)?;
            let parsed: SearchResult = serde_json::from_value(hits.clone())?;
            let ranges = ranges_for(&parsed);
            if ranges.is_empty() {
                (hits.to_string().len(), 1)
            } else {
                let snippets = client.tool("code_read", json!({"ranges":ranges}))?;
                (hits.to_string().len() + snippets.to_string().len(), 2)
            }
        }
        4 => (client.tool("code_context", arguments)?.to_string().len(), 1),
        5 => (native_context(workspace, query)?.len(), 1),
        _ => return Err(invalid("invalid benchmark mode").into()),
    };
    Ok(Sample {
        milliseconds: start.elapsed().as_secs_f64() * 1000.0,
        output_bytes: bytes,
        tool_calls: calls,
    })
}

pub(crate) fn freeze(
    source: &Workspace,
    destination: &Path,
) -> Result<(Workspace, usize, usize, String)> {
    let files = source.files()?;
    let mut bytes = 0;
    let mut sorted: Vec<_> = files.iter().collect();
    sorted.sort();
    let mut digest = Sha256::new();
    for path in sorted {
        let contents = source
            .read_allowed(
                path.to_str().ok_or_else(|| invalid("non-UTF8 path"))?,
                &files,
            )
            .map_err(|e| invalid(format!("cannot freeze {}: {e}", path.display())))?;
        let target = destination.join(path);
        std::fs::create_dir_all(target.parent().ok_or_else(|| invalid("missing parent"))?)?;
        bytes += contents.len();
        let name = path
            .to_str()
            .ok_or_else(|| invalid("non-UTF8 path"))?
            .as_bytes();
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(name);
        digest.update((contents.len() as u64).to_le_bytes());
        digest.update(&contents);
        std::fs::write(target, contents)?;
    }
    for scope in &source.scopes {
        std::fs::create_dir_all(destination.join(scope))?;
    }
    Ok((
        Workspace::new(destination, source.scopes.clone(), source.globs.clone())?,
        files.len(),
        bytes,
        format!("{:x}", digest.finalize()),
    ))
}

fn verify_context(workspace: &Workspace, client: &mut Client, query: &str) -> Result<()> {
    let request = ContextRequest {
        search: Search {
            pattern: query.into(),
            regex: false,
            ignore_case: false,
            limit: 8,
        },
        radius: 3,
    };
    let expected = context(workspace, &request)?;
    let received: ContextResult =
        serde_json::from_value(client.tool("code_context", json!({"pattern":query,"limit":8}))?)?;
    if serde_json::to_value(&expected)? != serde_json::to_value(received)? {
        return Err(invalid("MCP/direct context mismatch").into());
    }
    if !expected.search.matches.is_empty() {
        let ranges = ranges_for(&expected.search);
        let snippets = read_many(workspace, &ranges)?;
        verify_snippets(&expected, &snippets)?;
    }
    Ok(())
}

fn verify_snippets(context: &ContextResult, snippets: &[Snippet]) -> Result<()> {
    for snippet in snippets {
        for (offset, line) in snippet.lines.iter().enumerate() {
            let number = snippet.start + offset;
            let matches = context.snippets.iter().any(|s| {
                s.path == snippet.path
                    && s.sha256 == snippet.sha256
                    && number >= s.start
                    && number <= s.end
                    && s.lines[number - s.start] == *line
            });
            if !matches {
                return Err(invalid("batched/context source mismatch").into());
            }
        }
    }
    Ok(())
}

fn percentile(values: &[f64], fraction: f64) -> f64 {
    values[((values.len() as f64 * fraction).ceil() as usize).saturating_sub(1)]
}

pub fn run(source: &Workspace, queries_path: &Path, output: &Path, rounds: usize) -> Result<()> {
    if !(10..=200).contains(&rounds) {
        return Err(invalid("rounds must be 10..200").into());
    }
    let queries: Vec<String> = serde_json::from_reader(std::fs::File::open(queries_path)?)?;
    if queries.is_empty() || queries.len() > 100 || queries.iter().any(String::is_empty) {
        return Err(invalid("provide 1..100 nonempty queries").into());
    }
    if output.exists() {
        return Err(invalid("refusing to overwrite benchmark report").into());
    }
    let snapshot = tempfile::tempdir()?;
    let start = Instant::now();
    let (workspace, file_count, source_bytes, corpus_sha256) = freeze(source, snapshot.path())?;
    let freeze_ms = start.elapsed().as_secs_f64() * 1000.0;
    let startup = Instant::now();
    let mut client = Client::start(&workspace)?;
    let startup_ms = startup.elapsed().as_secs_f64() * 1000.0;
    let mut oracle_matches = BTreeMap::new();
    for query in &queries {
        oracle_matches.insert(query.clone(), check_oracle(&workspace, query)?);
        verify_context(&workspace, &mut client, query)?;
    }
    let modes = [
        "raw_rg",
        "cli_grep",
        "mcp_grep",
        "mcp_search_then_batch_read",
        "mcp_context",
        "raw_rg_context",
    ];
    let mut samples: BTreeMap<String, BTreeMap<String, Vec<Sample>>> = BTreeMap::new();
    for round in 0..=rounds {
        for (q, query) in queries.iter().enumerate() {
            for offset in 0..modes.len() {
                let mode = (round + q + offset) % modes.len();
                let observation = sample(&workspace, &mut client, mode, query)?;
                if round > 0 {
                    samples
                        .entry(modes[mode].into())
                        .or_default()
                        .entry(query.clone())
                        .or_default()
                        .push(observation);
                }
            }
        }
        eprintln!("benchmark {round}/{rounds}");
    }
    let mut summaries = BTreeMap::new();
    for (mode, per_query) in &samples {
        let mut values: Vec<f64> = per_query
            .values()
            .flatten()
            .map(|s| s.milliseconds)
            .collect();
        values.sort_by(f64::total_cmp);
        let count = values.len() as f64;
        summaries.insert(mode.clone(), json!({
            "p50_ms":percentile(&values,0.5), "p95_ms":percentile(&values,0.95),
            "mean_ms":values.iter().sum::<f64>()/count,
            "mean_output_bytes":per_query.values().flatten().map(|s| s.output_bytes as f64).sum::<f64>()/count,
            "mean_tool_calls":per_query.values().flatten().map(|s| s.tool_calls as f64).sum::<f64>()/count
        }));
    }
    let report = json!({
        "schema_version":1, "version":env!("CARGO_PKG_VERSION"),
        "corpus":{"root":source.root,"scopes":source.scopes,"globs":source.globs,
            "files":file_count,"bytes":source_bytes,"sha256":corpus_sha256,"freeze_ms":freeze_ms,
            "consistency":"private frozen copy; not an atomic snapshot of the original repository"},
        "method":{"rounds":rounds,"warmup_rounds":1,"ordering":"rotating modes per query/round",
            "cache":"OS caches left intact; frozen-copy writes warm page cache",
            "mcp":"real persistent stdio child; initialize, tools/list, tools/call",
            "comparison":"grep modes share search scope; context modes return matching lines +/-3; raw rg returns all hits, wrapper limits 8. raw_rg_context is native rg -C3: same surrounding-source capability, but no file hashes",
            "agent_selection":"NOT MEASURED","end_to_end_agent_time":"NOT MEASURED",
            "output_measure":"payload bytes, not tokenizer tokens or full MCP wire bytes"},
        "mcp_startup_and_discovery_ms":startup_ms,
        "oracle_matches":oracle_matches,"summaries":summaries,"samples":samples
    });
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    serde_json::to_writer_pretty(&mut file, &report)?;
    file.write_all(b"\n")?;
    println!("{}", serde_json::to_string_pretty(&report["summaries"])?);
    Ok(())
}
