use crate::{Result, catalog, daemon, invalid};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

pub const CAPABILITY_SCHEMA: u32 = 3;

pub fn executable_hash(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    loop {
        let read = file.read(&mut bytes)?;
        if read == 0 {
            break;
        }
        hash.update(&bytes[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn capabilities() -> Result<Value> {
    static HASH: std::sync::OnceLock<std::result::Result<String, String>> =
        std::sync::OnceLock::new();
    let hash = HASH
        .get_or_init(|| executable_hash(Path::new("/proc/self/exe")).map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| invalid(e.clone()))?;
    Ok(json!({"schema":CAPABILITY_SCHEMA,"executable_sha256":hash,
        "package_version":env!("CARGO_PKG_VERSION"),"tool_count":7,
        "code_files":true,"context_radius_max":100,"authorized_worktrees":true,
        "registration_migration":true,"read_pages":true,"background_workers":1,"request_tracing":true,
        "cache_peak_admission":"minimum_lru","prewarm_priority":"disabled","trace_clock":"unix_ms",
        "source_cache":"nonblocking_root_cache","cache_pending_roots":8,"cache_miss":"CACHE_MISS",
        "read_policy_cache":true,"adaptive_memory":true,
        "client_trace_outcomes":true,"host_memory_reporting":true,
        "idle_maintenance_interval_ms":1000,
        "active_poll_interval_ms":25,"runtime_identity_guard":true,
        "max_registered_roots":crate::catalog::MAX_REGISTERED_ROOTS,
        "idle_source_lease_seconds":crate::watched::IDLE_ROOT_TIMEOUT.as_secs()}))
}

pub fn doctor(root: &Path) -> Result<Value> {
    let local = capabilities()?;
    let ping = daemon::request(&daemon::Request {
        root: None,
        name: "ping".into(),
        arguments: json!({}),
        compact: true,
    });
    let (remote, daemon_error) = match ping {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(error.to_string())),
    };
    let specs = catalog::load()?;
    let root_resolution = match catalog::resolve_root(root) {
        Ok(root) => json!({"registered":true,"root":root}),
        Err(error) => json!({"registered":false,"error":error.to_string()}),
    };
    let mut processes = Vec::new();
    let mut inspection_errors = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let exe = entry.path().join("exe");
        let target = match std::fs::read_link(&exe) {
            Ok(target) => target,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                continue;
            }
            Err(error) => {
                inspection_errors.push(error.to_string());
                continue;
            }
        };
        match entry.metadata() {
            Ok(metadata) if metadata.uid() == unsafe { libc::geteuid() } => {}
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                inspection_errors.push(error.to_string());
                continue;
            }
        }
        let name = target
            .to_string_lossy()
            .trim_end_matches(" (deleted)")
            .to_owned();
        if Path::new(&name).file_name().is_none_or(|n| n != "cctx") {
            continue;
        }
        let hash = match executable_hash(&exe) {
            Ok(hash) => hash,
            Err(error) => {
                inspection_errors.push(error.to_string());
                continue;
            }
        };
        let command = match std::fs::read(entry.path().join("cmdline")) {
            Ok(command) => command,
            Err(error) => {
                inspection_errors.push(error.to_string());
                continue;
            }
        };
        let role = if command.split(|b| *b == 0).any(|arg| arg == b"shared-mcp") {
            "shared-mcp"
        } else if command.split(|b| *b == 0).any(|arg| arg == b"daemon") {
            "daemon"
        } else {
            "client"
        };
        processes.push(
            json!({"pid":entry.file_name().to_string_lossy(),"role":role,
            "current_build":hash==local["executable_sha256"],"executable_sha256":hash,
            "cwd":std::fs::read_link(entry.path().join("cwd")).ok()}),
        );
    }
    let stale = processes
        .iter()
        .filter(|p| p["role"] == "shared-mcp" && p["current_build"] == false)
        .count();
    let daemon_matches_client = remote
        .as_ref()
        .and_then(|value| value.get("capabilities"))
        .and_then(|value| value.get("executable_sha256"))
        .map(|hash| hash == &local["executable_sha256"]);
    Ok(
        json!({"client":local,"daemon":remote,"daemon_error":daemon_error,
        "root_resolution":root_resolution,"registrations":specs,"processes":processes,
        "stale_mcp_processes":stale,"daemon_matches_client":daemon_matches_client,
        "inspection_errors":inspection_errors,
        "chat_exposure":"unverified; executable/cwd is not a session-to-process mapping",
        "recovery":{
            "interrupts_active_sessions":false,
            "steps":["Finish in-flight tool requests before reconnecting the host's code-context MCP server.",
                "For native tools, reload the extension host at a safe boundary, then refresh tools in the affected chat.",
                "Verify seven tools including code_files and radius 100; do not infer readiness from the installed VSIX alone."],
            "automatic_process_termination":false
        }}),
    )
}

pub fn coverage() -> Result<Value> {
    let mut roots = Vec::new();
    let specs = catalog::load()?;
    for saved in &specs {
        let spec = match crate::registration::effective_spec(saved, &specs) {
            Ok(spec) => spec,
            Err(error) => {
                roots.push(json!({"registration":saved,"error":error.to_string()}));
                continue;
            }
        };
        match spec
            .workspace()
            .and_then(|workspace| workspace.files_limited(&spec.policy))
        {
            Ok(inventory) => {
                let mut files: Vec<_> = inventory
                    .files
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                files.sort();
                roots.push(json!({"registration":spec,"files":files}));
            }
            Err(error) => roots.push(json!({"registration":spec,"error":error.to_string()})),
        }
    }
    let preparation_candidates = crate::registration::preparation_candidates(&specs);
    Ok(json!({"generated_at_unix":std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?.as_secs(),"roots":roots,
        "preparation_candidates":preparation_candidates,
        "contract":"point-in-time permitted inventory, not historical eligibility or host content-policy authorization"}))
}
