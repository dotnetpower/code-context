use crate::context::ContextRequest;
use crate::engine::{Backend, Engine};
use crate::experiment::{Client, freeze, native_context};
use crate::search::Search;
use crate::workspace::{Workspace, capture};
use crate::{Result, invalid};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

#[derive(Serialize)]
struct Observation {
    ms: f64,
    payload_bytes: usize,
}

fn normalized(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.remove("consistency");
        if let Some(search) = object.get_mut("search").and_then(Value::as_object_mut) {
            search.remove("consistency");
        }
    }
    value
}

fn assert_same(expected: &Value, actual: &Value) -> Result<()> {
    if normalized(expected.clone()) != normalized(actual.clone()) {
        return Err(
            invalid("resident/live result mismatch (excluding consistency metadata only)").into(),
        );
    }
    Ok(())
}

fn search_request(query: &str) -> Search {
    Search {
        pattern: query.into(),
        regex: false,
        ignore_case: false,
        limit: 8,
    }
}

fn process_memory(pid: u32) -> Result<Value> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let fields: BTreeMap<_, _> = text
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            ["VmRSS", "VmHWM"]
                .contains(&key)
                .then(|| (key.to_owned(), value.trim().to_owned()))
        })
        .collect();
    Ok(json!(fields))
}

fn read_bytes(pid: u32) -> Result<Option<u64>> {
    let path = format!("/proc/{pid}/io");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            eprintln!("physical I/O telemetry unavailable: {path}: {error}");
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    text.lines()
        .find_map(|line| line.strip_prefix("read_bytes: "))
        .ok_or_else(|| invalid("missing process read_bytes"))?
        .trim()
        .parse()
        .map(Some)
        .map_err(Into::into)
}

fn verify(
    workspace: &Workspace,
    engines: &[Engine; 3],
    clients: &mut [Client; 3],
    queries: &[String],
) -> Result<BTreeMap<String, usize>> {
    let mut counts = BTreeMap::new();
    for query in queries {
        let request = search_request(query);
        let live_result = engines[0].search(&request)?;
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
        let raw = String::from_utf8(capture(&mut command)?)?;
        let oracle: BTreeSet<_> = raw
            .split_terminator('\n')
            .map(|line| line.strip_prefix("./").unwrap_or(line).to_owned())
            .collect();
        if oracle.len() != live_result.total_matches
            || live_result
                .matches
                .iter()
                .any(|hit| !oracle.contains(&format!("{}:{}:{}", hit.path, hit.line, hit.text)))
        {
            return Err(invalid(format!("raw rg mismatch: {query:?}")).into());
        }
        counts.insert(query.clone(), oracle.len());
        let expected = serde_json::to_value(live_result)?;
        let context_request = ContextRequest {
            search: request,
            radius: 3,
        };
        let expected_context = serde_json::to_value(engines[0].context(&context_request)?)?;
        for (engine, client) in engines.iter().zip(clients.iter_mut()) {
            assert_same(
                &expected,
                &serde_json::to_value(engine.search(&search_request(query))?)?,
            )?;
            assert_same(
                &expected_context,
                &serde_json::to_value(engine.context(&context_request)?)?,
            )?;
            assert_same(
                &expected,
                &client.tool("code_grep", json!({"pattern":query,"limit":8}))?,
            )?;
            assert_same(
                &expected_context,
                &client.tool("code_context", json!({"pattern":query,"limit":8}))?,
            )?;
        }
    }
    Ok(counts)
}

fn observe(
    engines: &[Engine; 3],
    clients: &mut [Client; 3],
    query: &str,
    mode: usize,
) -> Result<Observation> {
    let start = Instant::now();
    let arguments = json!({"pattern":query,"limit":8});
    let value = match mode {
        0..=2 => serde_json::to_value(engines[mode].search(&search_request(query))?)?,
        3..=5 => clients[mode - 3].tool("code_grep", arguments)?,
        6..=8 => clients[mode - 6].tool("code_context", arguments)?,
        9 => {
            let bytes = native_context(&engines[0].workspace, query)?;
            return Ok(Observation {
                ms: start.elapsed().as_secs_f64() * 1000.0,
                payload_bytes: bytes.len(),
            });
        }
        _ => return Err(invalid("invalid resident benchmark mode").into()),
    };
    // Serialize the same normalized contract for core and transport comparisons.
    let payload_bytes = serde_json::to_vec(&normalized(value))?.len();
    Ok(Observation {
        ms: start.elapsed().as_secs_f64() * 1000.0,
        payload_bytes,
    })
}

fn verify_refresh(client: &mut Client) -> Result<Value> {
    let status_before = client.tool("code_status", json!({}))?;
    let start = Instant::now();
    let after = client.tool("code_refresh", json!({}))?;
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    if after["stats"]["generation"].as_u64()
        != status_before["stats"]["generation"].as_u64().map(|g| g + 1)
        || after["stats"]["source_sha256"] != status_before["stats"]["source_sha256"]
    {
        return Err(invalid(
            "refresh changed immutable benchmark contents or failed to advance generation",
        )
        .into());
    }
    Ok(json!({"full_rebuild_ms":elapsed_ms,"before":status_before,"after":after}))
}

fn percentile(values: &[f64], fraction: f64) -> f64 {
    values[((values.len() as f64 * fraction).ceil() as usize).saturating_sub(1)]
}

fn summarize(samples: &BTreeMap<String, BTreeMap<String, Vec<Observation>>>) -> Value {
    let mut summary = BTreeMap::new();
    for (mode, queries) in samples {
        let mut values: Vec<_> = queries.values().flatten().map(|o| o.ms).collect();
        values.sort_by(f64::total_cmp);
        summary.insert(mode, json!({
            "p50_ms":percentile(&values,0.5),"p95_ms":percentile(&values,0.95),
            "mean_ms":values.iter().sum::<f64>()/values.len() as f64,
            "mean_payload_bytes":queries.values().flatten().map(|o|o.payload_bytes as f64).sum::<f64>()/values.len() as f64
        }));
    }
    json!(summary)
}

pub fn run(source: &Workspace, queries_file: &Path, output: &Path, rounds: usize) -> Result<()> {
    if !(10..=200).contains(&rounds) || output.exists() {
        return Err(invalid("rounds must be 10..200; output must not exist").into());
    }
    let queries: Vec<String> = serde_json::from_reader(std::fs::File::open(queries_file)?)?;
    if queries.is_empty()
        || queries.len() > 100
        || queries.iter().any(|q| q.is_empty() || q.contains('\n'))
    {
        return Err(invalid("provide 1..100 nonempty single-line literal queries").into());
    }
    let temporary = tempfile::tempdir()?;
    eprintln!("resident experiment 1/5: freezing permitted source files");
    let (workspace, files, bytes, digest) =
        freeze(source, temporary.path()).map_err(|e| invalid(format!("freeze failed: {e}")))?;
    eprintln!("resident experiment 2/5: building pure Rust scan and inverted index");
    let live = Engine::new(workspace.clone(), Backend::Live)?;
    let ram = Engine::new(workspace.clone(), Backend::Ram)?;
    let index = Engine::new(workspace.clone(), Backend::Index)?;
    let engines = [live, ram, index];
    let mut startup_ms = Vec::new();
    let mut children = Vec::new();
    for backend in [Backend::Live, Backend::Ram, Backend::Index] {
        let start = Instant::now();
        eprintln!("resident experiment 3/5: initializing {backend:?} MCP child");
        children.push(
            Client::start_mode(&workspace, backend)
                .map_err(|e| invalid(format!("{backend:?} MCP initialization failed: {e}")))?,
        );
        startup_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let mut clients: [Client; 3] = children
        .try_into()
        .map_err(|_| invalid("expected three clients"))?;
    eprintln!("resident experiment 4/5: verifying output equality against rg");
    let counts = verify(&workspace, &engines, &mut clients, &queries)
        .map_err(|e| invalid(format!("verification failed: {e}")))?;
    eprintln!("resident experiment 5/5: timing warmed engines and transports");
    let io_before = [read_bytes(clients[1].pid())?, read_bytes(clients[2].pid())?];
    let modes = [
        "rg_engine",
        "ram_scan_engine",
        "ram_index_engine",
        "rg_mcp_grep",
        "ram_scan_mcp_grep",
        "ram_index_mcp_grep",
        "rg_mcp_context",
        "ram_scan_mcp_context",
        "ram_index_mcp_context",
        "native_rg_context",
    ];
    let mut samples: BTreeMap<String, BTreeMap<String, Vec<Observation>>> = BTreeMap::new();
    for round in 0..=rounds {
        for (q, query) in queries.iter().enumerate() {
            for offset in 0..modes.len() {
                let mode = (round + q + offset) % modes.len();
                let observation = observe(&engines, &mut clients, query, mode)?;
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
        eprintln!("resident benchmark {round}/{rounds}");
    }
    let summary = summarize(&samples);
    let mut process_stats = Vec::new();
    for (i, client) in clients.iter_mut().enumerate() {
        process_stats.push(json!({"backend":format!("{:?}",engines[i].backend),
            "startup_and_discovery_ms":startup_ms[i],
            "memory":process_memory(client.pid())?,
            "status":client.tool("code_status",json!({}))?}));
    }
    let mut io_delta = Vec::new();
    for (i, before) in io_before.iter().enumerate() {
        let after = read_bytes(clients[i + 1].pid())?;
        io_delta.push(match (*before, after) {
            (Some(before), Some(after)) => Some(
                after
                    .checked_sub(before)
                    .ok_or_else(|| invalid("process I/O counter decreased"))?,
            ),
            _ => None,
        });
    }
    let refresh = [
        verify_refresh(&mut clients[1])?,
        verify_refresh(&mut clients[2])?,
    ];
    let after_refresh_memory = [
        process_memory(clients[1].pid())?,
        process_memory(clients[2].pid())?,
    ];
    let candidate_work: BTreeMap<_,_> = queries.iter().map(|query|
        (query, json!({"scan":engines[1].candidate_work(query),"index":engines[2].candidate_work(query)}))
    ).collect();
    let report = json!({
        "schema_version":2,"experiment":"actual resident scan and file-level byte-trigram inverted index",
        "corpus":{"files":files,"bytes":bytes,"sha256":digest,"root":source.root,"scopes":source.scopes,"globs":source.globs},
        "method":{"rounds":rounds,"warmup_rounds":1,"mode_order":"rotating per round and query",
            "semantics":"case-sensitive single-line literal; same full count, sorted first 8 matching lines; context radius 3 and identical file hashes",
            "cache":"warm OS page cache, no cold-SSD claim; resident backends never open files or spawn rg during search/read",
            "normalization":"consistency metadata removed; all other result fields and normalized payload bytes equal",
            "native_rg_context":"additional existing-capability baseline; no hashes, all hits, different output contract",
            "engine_time":"includes creation and serialization of normalized result; rg_engine includes subprocess/traversal/parsing",
            "transport":"same persistent MCP stdio clients for all three backends",
            "freshness":"resident explicit snapshots; full rebuild measured separately; no automatic watcher",
            "physical_io_telemetry":"null means kernel did not expose /proc/PID/io; it does not mean zero disk I/O. Pure resident code paths and source-removal tests establish no source-file reads during query.",
            "copilot_selection":"NOT MEASURED","agent_end_to_end":"NOT MEASURED"},
        "oracle_matches":counts,"summaries":summary,"processes":process_stats,
        "resident_query_physical_read_bytes":io_delta,"candidate_work":candidate_work,
        "refresh":refresh,"after_refresh_memory":after_refresh_memory,"samples":samples
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
