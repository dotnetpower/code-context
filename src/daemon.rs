use crate::catalog::{self, Spec};
use crate::policy::WORKER_STACK_BYTES;
#[cfg(test)]
use crate::policy::GLOBAL_RESIDENT_TARGET;
use crate::watched::{IDLE_ROOT_TIMEOUT, Watched};
use crate::{Result, invalid};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::{
    fs::PermissionsExt,
    net::{UnixListener, UnixStream},
};
#[cfg(windows)]
type RpcStream = std::net::TcpStream;
#[cfg(unix)]
type RpcStream = UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const MAX_WIRE: usize = 2 * 1024 * 1024;
const BINARY_MAGIC: &[u8] = b"CCTXB1\n";
const FRAMED_JSON_MAGIC: &[u8] = b"CCTXJ1\n";
const MAX_PENDING_ROOTS: usize = 8;
const MAX_CACHE_FAILURES: usize = 32;
const MAX_COLD_LOAD_RETRIES: u8 = 6;
const MIN_COLD_RETRY_RESERVE: usize = 128 * 1024 * 1024;
const CACHE_FAILURE_COOLDOWN: Duration = Duration::from_secs(60);
const IDLE_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(1);
const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(5);
const SERVICE_READ_BPS: u64 = 32_000_000;
const SERVICE_WRITE_BPS: u64 = 1_000_000;
type WireResult = std::result::Result<Value, String>;
pub(crate) const TRACE_ARGUMENT: &str = "_cctx_trace";
pub(crate) const PREV_TRACE_ARGUMENT: &str = "_cctx_prev_trace";

#[cfg(unix)]
fn current_cgroup_dir()->Result<PathBuf> {
    let cgroup=std::fs::read_to_string("/proc/self/cgroup")?;
    let relative=cgroup.lines().find_map(|line|line.strip_prefix("0::"))
        .ok_or_else(||invalid("unified cgroup path is unavailable"))?;
    let path=std::path::Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/')).canonicalize()?;
    if !path.starts_with("/sys/fs/cgroup") {
        return Err(invalid("cgroup path escaped the unified hierarchy").into());
    }
    Ok(path)
}

#[cfg(windows)]
fn current_cgroup_dir() -> Result<PathBuf> {
    catalog::state_dir()
}

#[cfg_attr(windows, allow(dead_code))]
fn service_io_guard_rule(device:&str,current:&str)->Result<String> {
    let existing=current.lines().find(|line|line.split_whitespace().next()==Some(device));
    let limit=|key:&str,ceiling:u64|->Result<u64>{
        let value=existing.and_then(|line|line.split_whitespace()
            .find_map(|entry|entry.strip_prefix(key)));
        match value {
            None|Some("max")=>Ok(ceiling),
            Some(value)=>Ok(value.parse::<u64>()?.min(ceiling)),
        }
    };
    Ok(format!("{device} rbps={} wbps={}",limit("rbps=",SERVICE_READ_BPS)?,
        limit("wbps=",SERVICE_WRITE_BPS)?))
}

#[cfg(unix)]
fn apply_service_io_guard()->Result<()> {
    match std::env::var_os("CCTX_IO_GUARD") {
        None=>return Ok(()),
        Some(value) if value=="1"=>{},
        _=>return Err(invalid("CCTX_IO_GUARD must be1 when enabled").into()),
    }
    let directory=current_cgroup_dir()?;
    if directory.file_name().is_none_or(|name|name!="code-context.service") {
        return Err(invalid("CCTX_IO_GUARD may only constrain the dedicated code-context.service cgroup").into());
    }
    let home=std::env::var_os("HOME").ok_or_else(||invalid("HOME is required for the service IO guard"))?;
    let device=std::fs::metadata(home)?.dev();
    let device=format!("{}:{}",libc::major(device),libc::minor(device));
    let path=directory.join("io.max");
    let rule=service_io_guard_rule(&device,&std::fs::read_to_string(&path)?)?;
    std::fs::write(&path,format!("{rule}\n"))?;
    let actual=std::fs::read_to_string(&path)?;
    let actual=actual.lines().find(|line|line.split_whitespace().next()==Some(device.as_str()))
        .ok_or_else(||invalid("service IO guard was not realized in the kernel"))?;
    for expected in rule.split_whitespace().skip(1) {
        if !actual.split_whitespace().any(|value|value==expected) {
            return Err(invalid("service IO guard verification failed; refusing unbounded service startup").into());
        }
    }
    eprintln!("CCTX_IO_GUARD {rule}");
    Ok(())
}

#[cfg(windows)]
fn apply_service_io_guard() -> Result<()> {
    Ok(())
}

#[cfg(unix)]
struct RuntimeIdentity {
    paths: [PathBuf;3],
    identities: [(u64,u64);3],
    last_check: Instant,
}

#[cfg(unix)]
impl RuntimeIdentity {
    fn capture(directory:&std::path::Path,socket:&std::path::Path,lock:&std::fs::File)->Result<Self> {
        let paths=[directory.to_path_buf(),socket.to_path_buf(),directory.join("daemon.lock")];
        let directory_meta=std::fs::symlink_metadata(&paths[0])?;
        let socket_meta=std::fs::symlink_metadata(&paths[1])?;
        let lock_meta=lock.metadata()?;
        let mut value=Self {
            paths,
            identities:[(directory_meta.dev(),directory_meta.ino()),
                (socket_meta.dev(),socket_meta.ino()),(lock_meta.dev(),lock_meta.ino())],
            last_check:Instant::now()-IDLE_MAINTENANCE_INTERVAL,
        };
        value.check()?;
        Ok(value)
    }

    fn check(&mut self)->Result<()> {
        use std::os::unix::fs::FileTypeExt;
        if self.last_check.elapsed()<IDLE_MAINTENANCE_INTERVAL {return Ok(());}
        for (index,path) in self.paths.iter().enumerate() {
            let metadata=std::fs::symlink_metadata(path).map_err(|error|
                invalid(format!("RUNTIME_UNAVAILABLE path={}: {error}; daemon is stopping instead of polling an inaccessible runtime",path.display())))?;
            let valid_kind=match index {
                0=>metadata.is_dir() && metadata.mode()&0o077==0,
                1=>metadata.file_type().is_socket() && metadata.mode()&0o777==0o600,
                _=>metadata.is_file() && metadata.mode()&0o077==0,
            };
            if !valid_kind || metadata.file_type().is_symlink()
                || metadata.uid()!=unsafe{libc::geteuid()}
                || (metadata.dev(),metadata.ino())!=self.identities[index] {
                return Err(invalid(format!("RUNTIME_REPLACED_OR_UNSAFE path={}; daemon is stopping",path.display())).into());
            }
        }
        self.last_check=Instant::now();
        Ok(())
    }
}

#[cfg(windows)]
struct RuntimeIdentity {
    paths: [PathBuf; 3],
    socket_contents: String,
    last_check: Instant,
}

#[cfg(windows)]
impl RuntimeIdentity {
    fn capture(directory: &std::path::Path, socket: &std::path::Path, _lock: &std::fs::File) -> Result<Self> {
        let paths = [directory.to_path_buf(), socket.to_path_buf(), directory.join("daemon.lock")];
        let socket_contents = std::fs::read_to_string(&paths[1])?;
        let mut value = Self {
            paths,
            socket_contents,
            last_check: Instant::now() - IDLE_MAINTENANCE_INTERVAL,
        };
        value.check()?;
        Ok(value)
    }

    fn check(&mut self) -> Result<()> {
        if self.last_check.elapsed() < IDLE_MAINTENANCE_INTERVAL {
            return Ok(());
        }
        for (index, path) in self.paths.iter().enumerate() {
            let metadata = std::fs::symlink_metadata(path).map_err(|error| {
                invalid(format!(
                    "RUNTIME_UNAVAILABLE path={}: {error}; daemon is stopping instead of polling an inaccessible runtime",
                    path.display()
                ))
            })?;
            let valid_kind = if index == 0 {
                metadata.is_dir()
            } else {
                metadata.is_file()
            };
            if !valid_kind || metadata.file_type().is_symlink() {
                return Err(invalid(format!(
                    "RUNTIME_REPLACED_OR_UNSAFE path={}; daemon is stopping",
                    path.display()
                ))
                .into());
            }
        }
        if std::fs::read_to_string(&self.paths[1]).ok().as_ref() != Some(&self.socket_contents) {
            return Err(invalid(format!(
                "RUNTIME_REPLACED_OR_UNSAFE path={}; daemon is stopping",
                self.paths[1].display()
            ))
            .into());
        }
        self.last_check = Instant::now();
        Ok(())
    }
}

pub(crate) fn new_trace_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    static NONCE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let nonce = NONCE.get_or_init(|| {
        use std::hash::{BuildHasher, Hasher};
        std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish()
    });
    format!(
        "{:x}-{:x}-{:x}",
        std::process::id(),
        nonce,
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

pub(crate) fn trace_unix_ms() -> Option<u64> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())
        .and_then(|duration| {
            u64::try_from(duration.as_millis()).map_err(|error| error.to_string())
        });
    match timestamp {
        Ok(value) => Some(value),
        Err(error) => {
            eprintln!("CCTX_TRACE_CLOCK_UNAVAILABLE: {error}");
            None
        }
    }
}

fn validate_trace_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(
            invalid("trace ID must be 1..96 ASCII letters, digits, '-', '_' or '.'").into(),
        );
    }
    Ok(())
}

#[derive(Deserialize)]
struct TracedRawEnvelope<'a> {
    _cctx_trace: Value,
    #[serde(borrow)]
    value: &'a serde_json::value::RawValue,
}

pub(crate) fn unwrap_trace_raw<'a>(
    bytes: &'a [u8],
    trace_id: &str,
) -> Result<(&'a serde_json::value::RawValue, Value)> {
    let envelope: TracedRawEnvelope<'a> = serde_json::from_slice(bytes)?;
    if envelope._cctx_trace["trace_id"] != trace_id {
        return Err(invalid("daemon trace is missing or does not match the request").into());
    }
    Ok((envelope.value, envelope._cctx_trace))
}

#[cfg(test)]
fn unwrap_trace(value: Value, trace_id: &str) -> Result<(Value, Value)> {
    let bytes = serde_json::to_vec(&value)?;
    let (raw, trace) = unwrap_trace_raw(&bytes, trace_id)?;
    Ok((serde_json::from_str(raw.get())?, trace))
}

#[derive(Debug)]
struct ApplicationError(String);
impl std::fmt::Display for ApplicationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for ApplicationError {}
pub(crate) fn is_application_error(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<ApplicationError>().is_some()
}
#[derive(Clone, Copy)]
enum Protocol {
    JsonLines,
    Bincode,
    FramedJson,
}

#[derive(Serialize)]
struct BinaryRequestRef<'a> {
    root: &'a Option<PathBuf>,
    name: &'a str,
    arguments_json: &'a [u8],
    compact: bool,
}

#[derive(Deserialize)]
struct BinaryRequestBorrow<'a> {
    root: Option<PathBuf>,
    name: String,
    #[serde(borrow)]
    arguments_json: &'a [u8],
    compact: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub root: Option<PathBuf>,
    pub name: String,
    pub arguments: Value,
    #[serde(default)]
    pub compact: bool,
}

struct Envelope {
    request: Request,
    protocol: Protocol,
    reply: std::sync::mpsc::SyncSender<TimedReply>,
    queued_at: Instant,
    id: u64,
    trace_id: Option<String>,
    prev_trace: Option<Value>,
    first_dispatch: bool,
}

struct TimedReply {
    result: WireResult,
    sample: RequestTiming,
}

#[derive(Serialize)]
struct RequestTiming {
    id: u64,
    trace_id: Option<String>,
    operation: String,
    root: Option<PathBuf>,
    queue_ms: f64,
    execute_ms: f64,
    response_ms: f64,
    serialize_ms: f64,
    socket_write_ms: f64,
    success: bool,
    outcome: &'static str,
}

struct BackgroundRoot {
    spec: Spec,
    receiver: std::sync::mpsc::Receiver<Result<(Watched, Option<WireResult>)>>,
    started: Instant,
    budget: usize,
    owner: Option<(u64, bool)>,
    reply_abandoned: bool,
    retry: u8,
}

struct PendingRoot {
    requested: PathBuf,
    retry: u8,
}

struct RootPreparation {
    requested: PathBuf,
    receiver: std::sync::mpsc::Receiver<Result<Spec>>,
    retry: u8,
    policy_snapshot: Vec<Spec>,
}

impl RootPreparation {
    fn policy_changed(&self, current: &[Spec]) -> bool {
        self.policy_snapshot.iter().ne(current.iter().filter(|spec| spec.managed_parent.is_none()))
    }
}

#[derive(Debug)]
struct CacheMiss {
    root: PathBuf,
    state: &'static str,
    scheduled: bool,
    previous_failure: Option<String>,
}

impl std::fmt::Display for CacheMiss {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let previous = match &self.previous_failure {
            Some(error) => format!(" Previous background load failed: {error}."),
            None => String::new(),
        };
        write!(
            formatter,
            "CACHE_MISS root={} state={} background_load={}: no cached source result is available.{previous} Skip CCTX for this lookup and continue with a permitted host search/read under the same access policy. Do not poll, repeat this request, switch CCTX transports or run code_sync merely to wait for warmup. A later independent lookup can use the cache once ready; this is not a zero-match or absence result.",
            self.root.display(),
            self.state,
            self.scheduled
        )
    }
}
impl std::error::Error for CacheMiss {}

struct ActiveConnection(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[cfg(unix)]
fn connect_rpc_stream() -> Result<RpcStream> {
    Ok(UnixStream::connect(catalog::socket_path()?)?)
}

#[cfg(windows)]
fn connect_rpc_stream() -> Result<RpcStream> {
    let path = catalog::socket_path()?;
    let contents = std::fs::read_to_string(&path)?;
    let mut lines = contents.lines();
    let addr: std::net::SocketAddr = lines
        .next()
        .ok_or_else(|| invalid("daemon endpoint file is empty"))?
        .trim()
        .parse()
        .map_err(|e| invalid(format!("invalid daemon endpoint address: {e}")))?;
    let secret = lines
        .next()
        .ok_or_else(|| invalid("daemon endpoint file is missing auth token"))?
        .trim();
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(secret.as_bytes())?;
    stream.write_all(b"\n")?;
    Ok(stream)
}

pub struct Client {
    stream: RpcStream,
    reader: BufReader<RpcStream>,
    tracing: bool,
    write_buf: Vec<u8>,
    args_buf: Vec<u8>,
}

impl Client {
    pub fn connect() -> Result<Self> {
        let mut stream = connect_rpc_stream()
            .map_err(|error| invalid(format!("daemon unavailable: {error}")))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        stream.write_all(BINARY_MAGIC)?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            stream,
            reader,
            tracing: false,
            write_buf: Vec::with_capacity(512),
            args_buf: Vec::with_capacity(256),
        })
    }

    pub(crate) fn connect_traced() -> Result<Self> {
        let mut client = Self::connect()?;
        let ping = client.call(&Request {
            root: None,
            name: "ping".into(),
            arguments: json!({}),
            compact: true,
        })?;
        client.tracing = ping["capabilities"]["request_tracing"] == true
            && ping["capabilities"]["trace_clock"] == "unix_ms";
        Ok(client)
    }

    pub(crate) fn supports_tracing(&self) -> bool {
        self.tracing
    }

    pub(crate) fn call_raw(&mut self, request: &Request) -> Result<Vec<u8>> {
        self.args_buf.clear();
        serde_json::to_writer(&mut self.args_buf, &request.arguments)?;
        self.write_buf.clear();
        self.write_buf.extend_from_slice(&[0_u8; 4]);
        bincode::serialize_into(
            &mut self.write_buf,
            &BinaryRequestRef {
                root: &request.root,
                name: &request.name,
                arguments_json: &self.args_buf,
                compact: request.compact,
            },
        )?;
        let body_len = self.write_buf.len() - 4;
        if body_len > MAX_WIRE {
            return Err(invalid("request too large").into());
        }
        let len_bytes = u32::try_from(body_len)?.to_be_bytes();
        self.write_buf[..4].copy_from_slice(&len_bytes);
        self.stream.write_all(&self.write_buf)?;
        let mut header = [0_u8; 13];
        std::io::Read::read_exact(&mut self.reader, &mut header)?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        if !(9..=MAX_WIRE).contains(&length) {
            return Err(invalid("missing or oversized daemon response").into());
        }
        let ok = match header[4] {
            0 => false,
            1 => true,
            _ => return Err(invalid("invalid binary response status").into()),
        };
        let payload_len = u64::from_le_bytes([
            header[5], header[6], header[7], header[8], header[9], header[10], header[11],
            header[12],
        ]) as usize;
        if payload_len != length - 9 {
            return Err(invalid("corrupt binary response payload length").into());
        }
        let mut payload = vec![0_u8; payload_len];
        std::io::Read::read_exact(&mut self.reader, &mut payload)?;
        if ok {
            Ok(payload)
        } else {
            Err(ApplicationError(String::from_utf8(payload)?).into())
        }
    }

    pub fn call(&mut self, request: &Request) -> Result<Value> {
        let payload = self.call_raw(request)?;
        Ok(serde_json::from_slice(&payload)?)
    }
}

pub fn request(request: &Request) -> Result<Value> {
    let mut stream = connect_rpc_stream().map_err(|e| {
        invalid(format!(
            "daemon unavailable: {e}; start cctx daemon or the user service"
        ))
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let bytes = serde_json::to_vec(request)?;
    if bytes.len() > MAX_WIRE {
        return Err(invalid("request too large").into());
    }

    stream.write_all(&bytes)?;
    stream.write_all(b"\n")?;
    let mut response = Vec::new();
    std::io::Read::take(BufReader::new(stream), MAX_WIRE as u64 + 1)
        .read_until(b'\n', &mut response)?;
    if response.len() > MAX_WIRE {
        return Err(invalid("daemon response too large").into());
    }
    decode_response(&response)
}

fn decode_response(response: &[u8]) -> Result<Value> {
    let response: Value = serde_json::from_slice(response)?;
    if let Some(error) = response.get("error") {
        return Err(ApplicationError(error.as_str().unwrap_or("daemon error").into()).into());
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| invalid("missing daemon response result").into())
}

#[cfg(windows)]
fn start_windows_daemon() -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
        .spawn()?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(3) {
        if connect_rpc_stream().is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

pub fn native_bridge(mut input: impl std::io::Read, mut output: impl Write) -> Result<()> {
    let mut client = match Client::connect() {
        Ok(client) => client,
        #[cfg(windows)]
        Err(_) if std::env::var_os("CCTX_NO_AUTO_START").is_none() => {
            let _ = start_windows_daemon();
            Client::connect()?
        }
        Err(error) => return Err(error),
    };
    output.write_all(b"CCTXN1\n")?;
    output.flush()?;
    let mut magic = [0; 7];
    input.read_exact(&mut magic)?;
    if magic != FRAMED_JSON_MAGIC {
        return Err(invalid("native bridge requires CCTXJ1 framed IPC, not MCP").into());
    }
    let mut read_buf = Vec::with_capacity(512);
    let mut frame = Vec::with_capacity(4096);
    while read_native_frame_buf(&mut input, &mut read_buf)? {
        let request: Request = serde_json::from_slice(&read_buf)?;
        frame.clear();
        frame.extend_from_slice(&[0_u8; 4]);
        let call_outcome = match client.call_raw(&request) {
            Ok(payload) => Ok(payload),
            Err(error) if is_application_error(error.as_ref()) => Err(error),
            Err(_transport_error) => {
                if let Ok(reconnected) = Client::connect() {
                    client = reconnected;
                    client.call_raw(&request)
                } else {
                    #[cfg(windows)]
                    if std::env::var_os("CCTX_NO_AUTO_START").is_none() {
                        let _ = start_windows_daemon();
                        if let Ok(reconnected) = Client::connect() {
                            client = reconnected;
                            client.call_raw(&request)
                        } else {
                            Err(_transport_error)
                        }
                    } else {
                        Err(_transport_error)
                    }
                    #[cfg(not(windows))]
                    {
                        Err(_transport_error)
                    }
                }
            }
        };
        match call_outcome {
            Ok(payload) => {
                frame.reserve(payload.len() + 11);
                frame.extend_from_slice(b"{\"result\":");
                frame.extend_from_slice(&payload);
                frame.push(b'}');
            }
            Err(error) if is_application_error(error.as_ref()) => {
                serde_json::to_writer(&mut frame, &json!({"error":error.to_string()}))?;
            }
            Err(error) => return Err(error),
        }
        let body_len = frame.len() - 4;
        if body_len > MAX_WIRE {
            return Err(invalid("native bridge response exceeds2MiB").into());
        }
        let len_bytes = u32::try_from(body_len)?.to_be_bytes();
        frame[..4].copy_from_slice(&len_bytes);
        output.write_all(&frame)?;
        output.flush()?;
    }
    Ok(())
}

#[cfg(test)]
fn read_native_frame(input: &mut impl std::io::Read) -> Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    if read_native_frame_buf(input, &mut buf)? {
        Ok(Some(buf))
    } else {
        Ok(None)
    }
}

fn read_native_frame_buf(input: &mut impl std::io::Read, buf: &mut Vec<u8>) -> Result<bool> {
    let mut size = [0; 4];
    match input.read_exact(&mut size[..1]) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    input.read_exact(&mut size[1..])?;
    let size = u32::from_be_bytes(size) as usize;
    if size == 0 || size > MAX_WIRE {
        return Err(invalid("native bridge frame must contain1..2MiB").into());
    }
    buf.resize(size, 0);
    input.read_exact(buf)?;
    Ok(true)
}

pub struct Server {
    memory: crate::memory::Controller,
    outcome_counts: BTreeMap<String, u64>,
    roots: BTreeMap<PathBuf, Watched>,
    registry: Vec<Spec>,
    registry_path: Option<PathBuf>,
    registry_stamp: Option<(u64, u64, std::time::SystemTime)>,
    root_size_hints: BTreeMap<PathBuf, (Spec, usize)>,
    root_loads: BTreeMap<PathBuf, u64>,
    evictions: VecDeque<Value>,
    request_timings: VecDeque<RequestTiming>,
    client_timings: VecDeque<Value>,
    background_timings: VecDeque<Value>,
    background: Option<BackgroundRoot>,
    preparation: Option<RootPreparation>,
    pending_roots: VecDeque<PendingRoot>,
    root_aliases: BTreeMap<PathBuf, PathBuf>,
    cache_failures: BTreeMap<PathBuf, (Instant, String)>,
    cache_hits: u64,
    cache_misses: u64,
    background_results: BTreeMap<u64, WireResult>,
    last_pressure_check: Option<std::time::Instant>,
    last_idle_maintenance: Option<std::time::Instant>,
    idle_timeout: Duration,
    active_connections: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    protocol_calls: BTreeMap<String, u64>,
    protocol_failures: BTreeMap<String, u64>,
    runtime: Option<RuntimeIdentity>,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            memory: crate::memory::Controller::default(),
            outcome_counts: BTreeMap::new(),
            roots: BTreeMap::new(),
            registry: Vec::new(),
            registry_path: None,
            registry_stamp: None,
            root_size_hints: BTreeMap::new(),
            root_loads: BTreeMap::new(),
            evictions: VecDeque::new(),
            request_timings: VecDeque::new(),
            client_timings: VecDeque::new(),
            background_timings: VecDeque::new(),
            background: None,
            preparation: None,
            pending_roots: VecDeque::new(),
            root_aliases: BTreeMap::new(),
            cache_failures: BTreeMap::new(),
            cache_hits: 0,
            cache_misses: 0,
            background_results: BTreeMap::new(),
            last_pressure_check: None,
            last_idle_maintenance: None,
            idle_timeout: IDLE_ROOT_TIMEOUT,
            active_connections: None,
            protocol_calls: BTreeMap::new(),
            protocol_failures: BTreeMap::new(),
            runtime: None,
        }
    }
}

impl Server {
    pub fn call(&mut self, request: Request) -> Result<Value> {
        if request.name == "code_trace" {
            if request.root.is_some() {
                return Err(invalid("code_trace takes no root").into());
            }
            self.record_client_timing(request.arguments)?;
            return Ok(json!({"recorded":true}));
        }
        self.refresh_registry()?;
        if self.evict_idle_roots() {
            release_allocator_memory();
        }
        match request.name.as_str() {
            "ping" => {
                return Ok(
                    json!({"ready":true,"pid":std::process::id(),"loaded_roots":self.roots.len(),
                    "capabilities":crate::diagnostics::capabilities()?,
                    "prewarm_failures":0,
                    "prewarm_deferred_until_more_headroom":{},
                    "protocol_calls":self.protocol_calls,
                    "protocol_failures":self.protocol_failures,
                    "active_connections":self.active_connections.as_ref().map_or(0, |value|
                        value.load(std::sync::atomic::Ordering::Acquire))}),
                );
            }
            "code_workspaces" => return Ok(serde_json::to_value(&self.registry)?),
            "code_status" => {
                if request.arguments != json!({}) {
                    return Err(invalid("code_status takes empty arguments").into());
                }
                let requested = catalog::canonicalize(
                    request
                        .root
                        .as_ref()
                        .ok_or_else(|| invalid("root is required; use code_workspaces"))?,
                )?;
                let spec = self.registry.iter().filter(|spec| requested.starts_with(&spec.root))
                    .max_by_key(|spec| spec.root.components().count())
                    .ok_or_else(|| invalid("status root is not registered; prepare it explicitly or issue a source lookup"))?;
                let effective = crate::registration::registered_policy(spec, &self.registry)?;
                let root = spec.root.clone();
                let mut value = match self.roots.get(&root) {
                    Some(watched) if watched.spec == effective => watched.status_snapshot()?,
                    _ => json!({
                        "root":root, "index_state":"unloaded", "stats":null,
                        "blocked":null,
                        "status_freshness":"observational snapshot; no source loaded or reconciled",
                        "next_step":"A cold source lookup returns CACHE_MISS immediately and schedules only this root for background loading. Continue with a permitted host read; do not poll or use code_sync merely to wait for warmup."
                    }),
                };
                self.add_status_metrics(&mut value, &effective)?;
                return if request.compact {
                    Ok(value)
                } else {
                    Ok(json!({"root":root,"data":value}))
                };
            }
            _ => {}
        }
        if ![
            "code_files",
            "code_grep",
            "code_context",
            "code_read",
            "code_status",
            "code_sync",
            "code_profile",
            "code_profile_full",
        ]
        .contains(&request.name.as_str())
        {
            return Err(invalid("unknown source operation").into());
        }
        let requested_root = request
            .root
            .ok_or_else(|| invalid("root is required; use code_workspaces"))?;
        let canonical = if matches!(
            request.name.as_str(),
            "code_files" | "code_grep" | "code_context" | "code_read"
        ) && self.roots.contains_key(&requested_root)
            && self
                .registry
                .iter()
                .any(|spec| spec.root == requested_root && !spec.allow_worktrees)
        {
            requested_root
        } else {
            catalog::canonicalize(&requested_root)?
        };
        let spec = if matches!(
            request.name.as_str(),
            "code_files" | "code_grep" | "code_context" | "code_read"
        ) {
            match self.requested_registration(&canonical)? {
                Some(spec)
                    if self
                        .roots
                        .get(&spec.root)
                        .is_some_and(|entry| entry.spec == spec) =>
                {
                    self.cache_hits = self.cache_hits.saturating_add(1);
                    spec
                }
                other => {
                    let target = other.map_or(canonical, |spec| spec.root);
                    return Err(self.request_cache(target)?.into());
                }
            }
        } else {
            self.source_spec_canonical(&canonical)?
        };
        let root = spec.root.clone();
        if self
            .roots
            .get(&root)
            .is_some_and(|entry| entry.spec != spec)
        {
            self.evict(&root, "registration_changed");
            release_allocator_memory();
        }
        if !self.roots.contains_key(&root) {
            return Err(self.request_cache(root)?.into());
        }
        let compact = request.compact;
        let root_budget = if self.roots.len() <= 1 {
            self.available_cache_budget()
        } else {
            self.available_cache_budget().saturating_sub(
                self.roots
                    .iter()
                    .filter(|(path, _)| **path != root)
                    .map(|(_, entry)| entry.estimated_bytes())
                    .sum::<usize>(),
            )
        };
        let entry = self
            .roots
            .get_mut(&root)
            .ok_or_else(|| invalid("missing registered root"))?;
        let previous = entry.restrict_admission(root_budget);
        let mut outcome = entry.call_with_mode(&request.name, request.arguments.clone(), compact);
        for _ in 0..3 {
            if let Err(ref error) = outcome
                && matches!(
                    request.name.as_str(),
                    "code_files" | "code_grep" | "code_context"
                )
                && entry.recover_stale_search(&error.to_string()).unwrap_or(false)
            {
                outcome = entry.call_with_mode(&request.name, request.arguments.clone(), compact);
            } else {
                break;
            }
        }
        entry.restore_admission(previous);
        let result = outcome?;
        if let Some(watched) = self.roots.get(&root) {
            let estimated = watched.estimated_bytes();
            match self.root_size_hints.get_mut(&root) {
                Some((saved_spec, bytes)) if *saved_spec == watched.spec => {
                    *bytes = estimated;
                }
                _ => {
                    self.root_size_hints
                        .insert(root.clone(), (watched.spec.clone(), estimated));
                }
            }
        }
        while self.roots.len() > 1
            && self
                .roots
                .values()
                .map(Watched::estimated_bytes)
                .sum::<usize>()
                > self.available_cache_budget()
        {
            if let Some(oldest) = self.eviction_candidate(Some(&root)) {
                self.evict(&oldest, "resident_budget");
                release_allocator_memory();
            } else {
                break;
            }
        }
        if compact {
            Ok(Self::compact_result(&request.name, result))
        } else {
            Ok(json!({"root":root,"data":result}))
        }
    }

    fn add_status_metrics(&self, value: &mut Value, spec: &Spec) -> Result<()> {
        let object = value
            .as_object_mut()
            .ok_or_else(|| invalid("status is not an object"))?;
        object.insert("capabilities".into(), crate::diagnostics::capabilities()?);
        object.insert("coverage".into(), serde_json::to_value(spec)?);
        object.insert("registry_admission".into(),json!({
            "registered_roots":self.registry.len(),"limit":catalog::MAX_REGISTERED_ROOTS,
            "managed_worktrees":self.registry.iter().filter(|spec|spec.managed_parent.is_some()).count(),
            "idle_source_lease_seconds":IDLE_ROOT_TIMEOUT.as_secs(),
            "policy":"source lookups extend the idle lease; status polling does not; active/manual registrations are not automatically removed",
        }));
        object.insert("process_memory".into(), self.process_memory());
        object.insert("outcome_counts".into(), json!(self.outcome_counts));
        object.insert("protocol_calls".into(), json!(self.protocol_calls));
        object.insert("protocol_failures".into(), json!(self.protocol_failures));
        object.insert("cache".into(), json!({
            "mode":"nonblocking_root_cache","granularity":"registered_root",
            "state":if self.roots.get(&spec.root).is_some_and(|entry|entry.spec==*spec) {"ready"}
                else if let Some(job)=self.background.as_ref().filter(|job|job.spec.root==spec.root) {
                    if job.owner.is_some() {"syncing"}else{"loading"}
                }
                else if self.preparation.as_ref().is_some_and(|job|job.requested==spec.root) {"preparing"}
                else if self.pending_roots.iter().any(|job|job.requested==spec.root) {"queued"}
                else if self.cache_failures.contains_key(&spec.root) {"failed"}else{"unloaded"},
            "hits":self.cache_hits,"misses":self.cache_misses,
            "pending_limit":MAX_PENDING_ROOTS,
            "load_retry_limit":MAX_COLD_LOAD_RETRIES,
            "pending_roots":self.pending_roots.iter().map(|job|&job.requested).collect::<Vec<_>>(),
            "preparing_root":self.preparation.as_ref().map(|job|&job.requested),
            "loading_root":self.background.as_ref().map(|job|&job.spec.root),
            "automatic_prewarm":false,"prewarm_pins_residency":false,
            "last_load_error":self.cache_failures.get(&spec.root).map(|(_,error)|error),
            "failure_cooldown_seconds":CACHE_FAILURE_COOLDOWN.as_secs(),
            "cold_lookup":"CACHE_MISS immediately; use a permitted host read, not another CCTX transport",
        }));
        object.insert("performance".into(), json!({
            "recent_requests":self.request_timings,"recent_background":self.background_timings,
            "recent_clients":self.client_timings,
            "evictions":self.evictions,
            "active_background":self.background.as_ref().map(|job| json!({
                "root":job.spec.root,"kind":if job.owner.is_some() {"strict_sync"}else{"cold_load"},
                "elapsed_ms":job.started.elapsed().as_secs_f64()*1000.0,
                "reserved_bytes":job.budget
            })),
            "timing_contract":"queue_ms includes initialization/barrier wait; execute_ms is main-thread processing; serialize_ms and socket_write_ms split response_ms. Background duration is separate; numeric request IDs are daemon-local; recent history is bounded.",
        }));
        object.insert(
            "daemon_admission".into(),
            json!({
                "loaded_roots":self.roots.len(),
                "estimated_bytes":self.roots.values().map(Watched::estimated_bytes).sum::<usize>(),
                "target_bytes":self.memory.limits.target,
                "resident_root_limit":self.memory.limits.root_limit(), "root_loads":self.root_loads,
            }),
        );
        Ok(())
    }

    fn available_cache_budget(&self) -> usize {
        self.memory.limits.target.saturating_sub(self.background.as_ref().map_or(0, |job| job.budget))
    }

    fn cold_load_budget(&self, spec: &Spec, used: usize) -> usize {
        let hint = self
            .root_size_hints
            .get(&spec.root)
            .filter(|(saved, _)| saved == spec)
            .map(|(_, bytes)| *bytes)
            .or_else(|| {
                spec.managed_parent.as_ref().and_then(|parent| {
                    self.root_size_hints
                        .get(parent)
                        .map(|(_, bytes)| *bytes)
                        .or_else(|| self.roots.get(parent).map(Watched::estimated_bytes))
                })
            });
        if let Some(bytes) = hint {
            return bytes
                .saturating_add(spec.policy.max_staged_bytes)
                .min(self.memory.limits.target);
        }
        if used == 0 {
            self.memory.limits.target
        } else {
            self.memory.limits.target
                .saturating_sub(used)
                .saturating_sub(128 * 1024 * 1024)
        }
    }

    fn cold_retry_budget_with_limit(spec: &Spec, previous: usize, required: usize, retry: u8, limit: usize) -> Option<usize> {
        if required > limit
            || previous >= limit
            || retry >= MAX_COLD_LOAD_RETRIES
        {
            return None;
        }
        let reserve = spec.policy.max_staged_bytes.max(MIN_COLD_RETRY_RESERVE);
        let growth = if retry == 0 { reserve } else { reserve.max(previous) };
        Some(required.max(previous).saturating_add(growth).min(limit))
    }

    #[cfg(test)]
    fn cold_retry_budget(spec: &Spec, previous: usize, required: usize, retry: u8) -> Option<usize> {
        Self::cold_retry_budget_with_limit(spec, previous, required, retry, GLOBAL_RESIDENT_TARGET)
    }

    fn requested_registration(&self, canonical: &std::path::Path) -> Result<Option<Spec>> {
        self.registry
            .iter()
            .filter(|spec| canonical.starts_with(&spec.root))
            .max_by_key(|spec| spec.root.components().count())
            .filter(|spec| {
                canonical == spec.root
                    || !spec.allow_worktrees
                    || self.root_aliases.get(canonical) == Some(&spec.root)
            })
            .map(|spec| crate::registration::registered_policy(spec, &self.registry))
            .transpose()
    }

    fn source_spec_canonical(&mut self, canonical: &std::path::Path) -> Result<Spec> {
        let registered = self
            .registry
            .iter()
            .filter(|spec| canonical.starts_with(&spec.root))
            .max_by_key(|spec| spec.root.components().count());
        let root = if let Some(spec) = registered.filter(|spec| {
            spec.managed_parent.is_none() && (!spec.allow_worktrees || canonical == spec.root)
        }) {
            spec.root.clone()
        } else {
            let root = crate::registration::prepare_root(canonical)?;
            self.refresh_registry()?;
            root
        };
        self.registry
            .iter()
            .find(|spec| spec.root == root)
            .cloned()
            .ok_or_else(|| invalid("root is not explicitly registered").into())
    }

    fn request_cache(&mut self, requested: PathBuf) -> Result<CacheMiss> {
        self.cache_misses = self.cache_misses.saturating_add(1);
        self.queue_root(requested)
    }

    fn queue_root(&mut self, requested: PathBuf) -> Result<CacheMiss> {
        if let Some((at, error)) = self.cache_failures.get(&requested)
            && at.elapsed() < CACHE_FAILURE_COOLDOWN
        {
            return Err(invalid(format!(
                    "CACHE_LOAD_FAILED root={}: {error}. Continue with a permitted host read; no cached absence result is available. Automatic retry is suppressed for {}s; do not poll or widen access.",
                    requested.display(), CACHE_FAILURE_COOLDOWN.as_secs())).into());
        }
        let previous_failure = self.cache_failures.remove(&requested).map(|(_, error)| error);
        let state = if let Some(job) = self
            .background
            .as_ref()
            .filter(|job| job.spec.root == requested)
        {
            if job.owner.is_some() {
                "syncing"
            } else {
                "loading"
            }
        } else if self
            .preparation
            .as_ref()
            .is_some_and(|job| job.requested == requested)
        {
            "preparing"
        } else if self
            .pending_roots
            .iter()
            .any(|job| job.requested == requested)
        {
            "queued"
        } else if self.pending_roots.len() >= MAX_PENDING_ROOTS {
            return Ok(CacheMiss {
                root: requested,
                state: "queue_full",
                scheduled: false,
                previous_failure,
            });
        } else {
            self.pending_roots.push_back(PendingRoot {
                requested: requested.clone(),
                retry: 0,
            });
            "queued"
        };
        Ok(CacheMiss {
            root: requested,
            state,
            scheduled: true,
            previous_failure,
        })
    }

    fn record_cache_failure(&mut self, root: PathBuf, error: String) {
        eprintln!("CCTX_CACHE_LOAD_FAILED root={}: {error}", root.display());
        if self.cache_failures.len() >= MAX_CACHE_FAILURES
            && !self.cache_failures.contains_key(&root)
            && let Some(oldest) = self
                .cache_failures
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(root, _)| root.clone())
        {
            self.cache_failures.remove(&oldest);
        }
        self.cache_failures.insert(root, (Instant::now(), error));
    }

    fn abandon_background_reply(&mut self, id: u64) {
        self.background_results.remove(&id);
        if let Some(job) = self.background.as_mut()
            && job.owner.is_some_and(|(owner, _)| owner == id)
        {
            job.reply_abandoned = true;
        }
    }

    fn finish_background_reply(&mut self, job: &BackgroundRoot, result: WireResult) {
        if !job.reply_abandoned && let Some((id, _)) = job.owner {
            self.background_results.insert(id, result);
        }
    }

    fn fail_background_root(&mut self, job: &BackgroundRoot, error: String) {
        self.record_cache_failure(job.spec.root.clone(), error.clone());
        self.finish_background_reply(job, Err(error));
    }

    fn drive_lazy_loads(&mut self) {
        if let Some(job) = self.preparation.take() {
            if self.background.is_some() {
                self.preparation = Some(job);
                return;
            }
            let outcome = match job.receiver.try_recv() {
                Ok(value) => value,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.preparation = Some(job);
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Err(invalid("root preparation worker disconnected").into())
                }
            };
            if outcome.is_err() {
                if let Err(error) = self.refresh_registry() {
                    self.record_cache_failure(job.requested, error.to_string());
                    return;
                }
                if job.policy_changed(&self.registry) && job.retry < MAX_COLD_LOAD_RETRIES
                    && self.pending_roots.len() < MAX_PENDING_ROOTS
                {
                    eprintln!("CCTX_PREPARATION_RETRY root={} reason=authorization_changed", job.requested.display());
                    self.pending_roots.push_front(PendingRoot {
                        requested: job.requested, retry: job.retry + 1,
                    });
                    return;
                }
            }
            let outcome = outcome.and_then(|spec| {
                self.refresh_registry()?;
                if !self.registry.iter().any(|current| current == &spec) {
                    return Err(invalid("registration changed during lazy preparation").into());
                }
                self.root_aliases
                    .insert(job.requested.clone(), spec.root.clone());
                if self.root_aliases.len() > 128 {
                    self.root_aliases.retain(|alias, _| alias == &job.requested);
                }
                if self
                    .roots
                    .get(&spec.root)
                    .is_none_or(|entry| entry.spec != spec)
                {
                    self.start_root_job(spec, None, job.retry)?;
                }
                Ok(())
            });
            if let Err(error) = outcome {
                self.record_cache_failure(job.requested, error.to_string());
            }
        }
        if self.background.is_some() || self.preparation.is_some() {
            return;
        }
        if self.pending_roots.len() > 1
            && let Some(idx) = self.pending_roots.iter().position(|candidate| {
                self.root_size_hints
                    .get(&candidate.requested)
                    .is_some_and(|(_, bytes)| *bytes <= 220 * 1024 * 1024)
            })
            && idx > 0
            && let Some(small_job) = self.pending_roots.remove(idx)
        {
            self.pending_roots.push_front(small_job);
        }
        let Some(job) = self.pending_roots.pop_front() else {
            return;
        };
        if let Some(direct_spec) = self
            .registry
            .iter()
            .find(|spec| spec.root == job.requested && spec.managed_parent.is_none())
            .cloned()
        {
            let outcome = crate::registration::effective_spec(&direct_spec, &self.registry)
                .and_then(|spec| {
                    self.root_aliases
                        .insert(job.requested.clone(), spec.root.clone());
                    if self
                        .roots
                        .get(&spec.root)
                        .is_none_or(|entry| entry.spec != spec)
                    {
                        self.start_root_job(spec, None, job.retry)?;
                    }
                    Ok(())
                });
            if let Err(error) = outcome {
                self.record_cache_failure(job.requested, error.to_string());
            }
            return;
        }
        let requested = job.requested.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("cctx-root-prepare".into())
            .stack_size(WORKER_STACK_BYTES)
            .spawn(move || {
                let result = (|| {
                    let root = crate::registration::prepare_root(&requested)?;
                    let registry = catalog::load()?;
                    let spec = registry
                        .iter()
                        .find(|spec| spec.root == root)
                        .ok_or_else(|| invalid("prepared root is not registered"))?;
                    crate::registration::effective_spec(spec, &registry)
                })();
                let _ = sender.send(result);
            });
        match spawned {
            Ok(_) => {
                self.preparation = Some(RootPreparation {
                    requested: job.requested,
                    receiver,
                    retry: job.retry,
                    policy_snapshot: self.registry.iter()
                        .filter(|spec|spec.managed_parent.is_none()).cloned().collect(),
                })
            }
            Err(error) => self.record_cache_failure(job.requested, error.to_string()),
        }
    }

    fn evict(&mut self, root: &PathBuf, reason: &str) {
        if self.roots.remove(root).is_some() {
            self.evictions.push_back(json!({"root":root,"reason":reason,
                "at_unix":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |duration|duration.as_secs())}));
            while self.evictions.len() > 32 {
                self.evictions.pop_front();
            }
        }
    }

    fn prepare_background(&mut self, envelope: &Envelope) -> Result<bool> {
        if !matches!(
            envelope.request.name.as_str(),
            "code_sync" | "code_profile" | "code_profile_full"
        ) {
            return Ok(true);
        }
        self.refresh_registry()?;
        let canonical = catalog::canonicalize(
            envelope
                .request
                .root
                .as_ref()
                .ok_or_else(|| invalid("root is required"))?,
        )?;
        if envelope.request.name == "code_sync" && envelope.request.arguments != json!({}) {
            return Err(invalid("code_sync takes empty arguments").into());
        }
        let spec = self.requested_registration(&canonical)?;
        if envelope.request.name == "code_sync" && envelope.first_dispatch {
            let root = spec.as_ref().map_or(&canonical, |spec| &spec.root);
            self.cache_failures.remove(root);
        }
        if envelope.request.name != "code_sync"
            && spec.as_ref().is_some_and(|spec| {
                self.roots
                    .get(&spec.root)
                    .is_some_and(|entry| entry.spec == *spec)
            })
        {
            return Ok(true);
        }
        if self.background.is_some() || self.preparation.is_some() {
            return Ok(false);
        }
        let Some(spec) = spec else {
            if !self.queue_root(canonical)?.scheduled {
                return Err(invalid(
                    "CACHE_QUEUE_FULL: explicit root preparation could not be scheduled",
                )
                .into());
            }
            return Ok(false);
        };
        if let Some((at, error)) = self.cache_failures.get(&spec.root)
            && at.elapsed() < CACHE_FAILURE_COOLDOWN
        {
            return Err(invalid(error.clone()).into());
        }
        if self
            .roots
            .get(&spec.root)
            .is_none_or(|entry| entry.spec != spec)
        {
            if !self.queue_root(spec.root)?.scheduled {
                return Err(invalid(
                    "CACHE_QUEUE_FULL: explicit root loading could not be scheduled",
                )
                .into());
            }
            return Ok(false);
        }
        self.start_root_job(spec, Some((envelope.id, envelope.request.compact)), 0)?;
        Ok(false)
    }

    fn start_root_job(
        &mut self,
        spec: Spec,
        requester: Option<(u64, bool)>,
        retry: u8,
    ) -> Result<()> {
        if self
            .roots
            .get(&spec.root)
            .is_some_and(|entry| entry.spec != spec)
        {
            self.evict(&spec.root, "registration_changed");
        }
        let existing = requester.and_then(|_| self.roots.remove(&spec.root));
        let owner = existing.as_ref().and(requester);
        let used = self
            .roots
            .values()
            .map(Watched::estimated_bytes)
            .sum::<usize>();
        let budget = if let Some(watched) = &existing {
            watched
                .estimated_bytes()
                .saturating_add(spec.policy.max_staged_bytes)
                .min(self.memory.limits.target)
        } else {
            self.cold_load_budget(&spec, used)
        };
        while !self.roots.is_empty()
            && (self.roots.len() >= self.memory.limits.root_limit()
                || self
                    .roots
                    .values()
                    .map(Watched::estimated_bytes)
                    .sum::<usize>()
                    .saturating_add(budget)
                    > self.memory.limits.target)
        {
            let oldest = self
                .eviction_candidate(None)
                .ok_or_else(|| invalid("missing eviction candidate"))?;
            self.evict(&oldest, "background_admission");
        }
        release_allocator_memory();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let worker_spec = spec.clone();
        std::thread::Builder::new()
            .name("cctx-root-work".into())
            .stack_size(WORKER_STACK_BYTES)
            .spawn(move || {
                let result = match existing {
                    Some(mut watched) => {
                        let previous = watched.restrict_admission(budget);
                        let outcome = watched
                            .call("code_sync", json!({}))
                            .map_err(|error| error.to_string());
                        watched.restore_admission(previous);
                        Ok((watched, Some(outcome)))
                    }
                    None => {
                        Watched::new_with_budget(worker_spec, budget).map(|watched| (watched, None))
                    }
                };
                let result = result.and_then(|(watched, outcome)| {
                    let registry = catalog::load()?;
                    if crate::registration::effective_spec(&watched.spec, &registry)?
                        != watched.spec
                    {
                        return Err(invalid(
                            "registration or parent policy changed during root build",
                        )
                        .into());
                    }
                    Ok((watched, outcome))
                });
                let _ = sender.send(result);
            })?;
        self.background = Some(BackgroundRoot {
            spec,
            receiver,
            started: Instant::now(),
            budget,
            owner,
            reply_abandoned: false,
            retry,
        });
        Ok(())
    }

    fn poll_background(&mut self) {
        let received = match self.background.as_ref() {
            Some(job) => match job.receiver.try_recv() {
                Ok(value) => Some(value),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some(Err(invalid("background root worker disconnected").into()))
                }
            },
            None => None,
        };
        let Some(received) = received else {
            return;
        };
        let job = self.background.take().expect("background job exists");
        self.background_timings
            .push_back(json!({"root":job.spec.root,
            "kind":if job.owner.is_some(){"strict_sync"}else{"cold_load"},
            "elapsed_ms":job.started.elapsed().as_secs_f64()*1000.0,
            "reserved_bytes":job.budget,
            "error":match &received {
                Err(error)=>Some(error.to_string().chars().take(2048).collect::<String>()),
                Ok((_,Some(Err(error))))=>Some(error.chars().take(2048).collect::<String>()),
                _=>None,
            },
            "required_bytes":received.as_ref().err().and_then(|error|
                crate::resident::admission_required_bytes(error.as_ref()).map(Watched::budget_for_resident)),
            "success":received.as_ref().is_ok_and(|(_,outcome)|outcome.as_ref().is_none_or(|result|result.is_ok()))}));
        while self.background_timings.len() > 32 {
            self.background_timings.pop_front();
        }
        match received {
            Ok((watched, outcome)) => {
                if let Err(error) = self.refresh_registry() {
                    self.fail_background_root(&job, error.to_string());
                    return;
                }
                if !self.registry.iter().any(|spec| spec == &job.spec) {
                    self.fail_background_root(
                        &job,
                        "registration changed during background work".into(),
                    );
                    return;
                }
                match crate::registration::registered_policy(&job.spec, &self.registry) {
                    Ok(current) if current == job.spec => {}
                    Ok(_) => {
                        self.fail_background_root(
                            &job,
                            "parent policy changed during background work".into(),
                        );
                        return;
                    }
                    Err(error) => {
                        self.fail_background_root(&job, error.to_string());
                        return;
                    }
                }
                self.root_size_hints.insert(
                    job.spec.root.clone(),
                    (job.spec.clone(), watched.estimated_bytes()),
                );
                if job.owner.is_none() {
                    *self.root_loads.entry(job.spec.root.clone()).or_default() += 1;
                }
                self.roots.insert(job.spec.root.clone(), watched);
                self.cache_failures.remove(&job.spec.root);
                if let Some((_, compact)) = job.owner {
                    let result = outcome.expect("sync outcome").map(|value| {
                        if compact {
                            value
                        } else {
                            json!({"root":job.spec.root,"data":value})
                        }
                    });
                    self.finish_background_reply(&job, result);
                }
            }
            Err(error) => {
                if crate::resident::is_admission_error(error.as_ref()) {
                    let required = Watched::budget_for_resident(
                        crate::resident::admission_required_bytes(error.as_ref())
                            .expect("typed admission error"),
                    );
                    let Some(retry_budget) = Self::cold_retry_budget_with_limit(
                        &job.spec, job.budget, required, job.retry, self.memory.limits.target,
                    ) else {
                        self.fail_background_root(&job, error.to_string());
                        return;
                    };
                    self.root_size_hints
                        .insert(job.spec.root.clone(), (
                            job.spec.clone(),
                            retry_budget.saturating_sub(job.spec.policy.max_staged_bytes),
                        ));
                    while self
                        .roots
                        .values()
                        .map(Watched::estimated_bytes)
                        .sum::<usize>()
                        .saturating_add(retry_budget)
                        > self.memory.limits.target
                    {
                        let Some(oldest) = self.eviction_candidate(None) else {
                            break;
                        };
                        self.evict(&oldest, "cold_build_peak_admission");
                    }
                    release_allocator_memory();
                    if self.pending_roots.len() < MAX_PENDING_ROOTS {
                        self.pending_roots.push_front(PendingRoot {
                            requested: job.spec.root,
                            retry: job.retry + 1,
                        });
                    } else {
                        self.fail_background_root(&job, error.to_string());
                    }
                } else {
                    self.fail_background_root(&job, error.to_string());
                }
            }
        }
    }

    fn record_timing(&mut self, mut timing: RequestTiming) {
        timing.operation = timing.operation.chars().take(64).collect();
        if timing
            .root
            .as_ref()
            .is_some_and(|root| root.as_os_str().len() > 4096)
        {
            timing.root = Some(PathBuf::from("<request path exceeds4096bytes>"));
        }
        self.request_timings.push_back(timing);
        while self.request_timings.len() > 64 {
            self.request_timings.pop_front();
        }
    }

    fn record_client_timing(&mut self, value: Value) -> Result<()> {
        let object = value
            .as_object()
            .ok_or_else(|| invalid("client trace must be an object"))?;
        if serde_json::to_vec(&value)?.len() > 4096 {
            return Err(invalid("client trace exceeds 4 KiB").into());
        }
        validate_trace_id(
            object
                .get("trace_id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("client trace ID is required"))?,
        )?;
        if !matches!(
            object.get("origin").and_then(Value::as_str),
            Some("mcp" | "native")
        ) {
            return Err(invalid("client trace origin must be mcp or native").into());
        }
        let operation = object
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("client trace operation is required"))?;
        if operation.is_empty()
            || operation.len() > 64
            || !operation
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(invalid("invalid client trace operation").into());
        }
        if let Some(root) = object.get("root").filter(|v| !v.is_null())
            && root.as_str().is_none_or(|s| s.len() > 4096)
        {
            return Err(invalid("invalid client trace root").into());
        }
        for (key, entry) in object {
            match key.as_str() {
                "trace_id" | "origin" | "operation" | "root" | "daemon" => {}
                "outcome" if matches!(entry.as_str(),
                    Some("success" | "cache_not_ready" | "policy_denied" | "invalid_request" | "system_error")) => {}
                "host_memory" if value["origin"] == "native"
                    && entry["platform"] == "win32"
                    && entry.as_object().is_some_and(|host|host.len() == 3)
                    && entry["total_bytes"].as_u64().is_some()
                    && entry["available_bytes"].as_u64().is_some() => {}
                "success" | "available" if entry.is_boolean() => {}
                "started_unix_ms" | "completed_unix_ms" if entry.is_u64() || entry.is_null() => {}
                "queue_ms" | "prepare_ms" | "ipc_ms" | "serialize_ms" | "write_ms" | "total_ms"
                    if entry
                        .as_f64()
                        .is_some_and(|n| n.is_finite() && (0.0..=3_600_000.0).contains(&n)) => {}
                _ => return Err(invalid(format!("invalid client trace field: {key}")).into()),
            }
        }
        for key in [
            "queue_ms",
            "prepare_ms",
            "ipc_ms",
            "serialize_ms",
            "write_ms",
            "total_ms",
        ] {
            if !object.contains_key(key) {
                return Err(invalid(format!("client trace field is required: {key}")).into());
            }
        }
        if !object.get("success").is_some_and(Value::is_boolean) {
            return Err(invalid("client trace success must be boolean").into());
        }
        if let Some(daemon) = object.get("daemon").filter(|v| !v.is_null())
            && (daemon["trace_id"] != value["trace_id"]
                || daemon["daemon_request_id"]
                    .as_u64()
                    .is_none_or(|id| id == 0)
                || ["queue_ms", "execute_ms"].iter().any(|key| {
                    daemon[*key]
                        .as_f64()
                        .is_none_or(|n| !n.is_finite() || !(0.0..=3_600_000.0).contains(&n))
                }))
        {
            return Err(invalid("invalid correlated daemon trace").into());
        }
        if let Some(host) = value.get("host_memory")
            && value["origin"] == "native" && host["platform"] == "win32"
        {
            let total = host["total_bytes"].as_u64()
                .and_then(|value|usize::try_from(value).ok())
                .ok_or_else(||invalid("invalid host memory total"))?;
            let available = host["available_bytes"].as_u64()
                .and_then(|value|usize::try_from(value).ok())
                .ok_or_else(||invalid("invalid host memory available"))?;
            self.memory.observe_windows_host(total, available)?;
        }
        self.client_timings.push_back(value);
        if self.client_timings.len() > 64 {
            self.client_timings.pop_front();
        }
        Ok(())
    }

    fn record_call(&mut self, protocol: Protocol, operation: &str) {
        let key = Self::protocol_metric_key(protocol, operation);
        if let Some(count) = self.protocol_calls.get_mut(key) {
            *count += 1;
        } else {
            self.protocol_calls.insert(key.to_owned(), 1);
        }
    }

    fn record_failure(&mut self, protocol: Protocol, operation: &str) {
        let key = Self::protocol_metric_key(protocol, operation);
        if let Some(count) = self.protocol_failures.get_mut(key) {
            *count += 1;
        } else {
            self.protocol_failures.insert(key.to_owned(), 1);
        }
    }

    fn protocol_metric_key(protocol: Protocol, operation: &str) -> &'static str {
        match (protocol, operation) {
            (Protocol::Bincode, "ping") => "bincode:ping",
            (Protocol::Bincode, "code_workspaces") => "bincode:code_workspaces",
            (Protocol::Bincode, "code_files") => "bincode:code_files",
            (Protocol::Bincode, "code_grep") => "bincode:code_grep",
            (Protocol::Bincode, "code_context") => "bincode:code_context",
            (Protocol::Bincode, "code_read") => "bincode:code_read",
            (Protocol::Bincode, "code_status") => "bincode:code_status",
            (Protocol::Bincode, "code_sync") => "bincode:code_sync",
            (Protocol::Bincode, "code_trace") => "bincode:code_trace",
            (Protocol::Bincode, "code_profile") => "bincode:code_profile",
            (Protocol::Bincode, "code_profile_full") => "bincode:code_profile_full",
            (Protocol::Bincode, _) => "bincode:unknown",
            (Protocol::FramedJson, "ping") => "framed_json:ping",
            (Protocol::FramedJson, "code_workspaces") => "framed_json:code_workspaces",
            (Protocol::FramedJson, "code_files") => "framed_json:code_files",
            (Protocol::FramedJson, "code_grep") => "framed_json:code_grep",
            (Protocol::FramedJson, "code_context") => "framed_json:code_context",
            (Protocol::FramedJson, "code_read") => "framed_json:code_read",
            (Protocol::FramedJson, "code_status") => "framed_json:code_status",
            (Protocol::FramedJson, "code_sync") => "framed_json:code_sync",
            (Protocol::FramedJson, "code_trace") => "framed_json:code_trace",
            (Protocol::FramedJson, "code_profile") => "framed_json:code_profile",
            (Protocol::FramedJson, "code_profile_full") => "framed_json:code_profile_full",
            (Protocol::FramedJson, _) => "framed_json:unknown",
            (Protocol::JsonLines, "ping") => "json_lines:ping",
            (Protocol::JsonLines, "code_workspaces") => "json_lines:code_workspaces",
            (Protocol::JsonLines, "code_files") => "json_lines:code_files",
            (Protocol::JsonLines, "code_grep") => "json_lines:code_grep",
            (Protocol::JsonLines, "code_context") => "json_lines:code_context",
            (Protocol::JsonLines, "code_read") => "json_lines:code_read",
            (Protocol::JsonLines, "code_status") => "json_lines:code_status",
            (Protocol::JsonLines, "code_sync") => "json_lines:code_sync",
            (Protocol::JsonLines, "code_trace") => "json_lines:code_trace",
            (Protocol::JsonLines, "code_profile") => "json_lines:code_profile",
            (Protocol::JsonLines, "code_profile_full") => "json_lines:code_profile_full",
            (Protocol::JsonLines, _) => "json_lines:unknown",
        }
    }

    fn eviction_candidate(&self, protected: Option<&PathBuf>) -> Option<PathBuf> {
        self.roots
            .iter()
            .filter(|(path, _)| protected != Some(*path))
            .max_by_key(|(_, entry)| {
                let age = entry.idle_age();
                let stale_worktree_bonus =
                    u8::from(age >= Duration::from_secs(60) && entry.spec.managed_parent.is_some());
                (
                    u8::from(age >= Duration::from_secs(10)),
                    stale_worktree_bonus,
                    age,
                )
            })
            .map(|(path, _)| path.clone())
    }

    fn compact_result(name: &str, mut result: Value) -> Value {
        if matches!(name, "code_grep" | "code_context")
            && let Some(object) = result.as_object_mut()
        {
            object.remove("consistency");
            if name == "code_context"
                && let Some(search) = object.get_mut("search").and_then(Value::as_object_mut)
            {
                search.remove("consistency");
                if let Some(matches) = search.get_mut("matches").and_then(Value::as_array_mut) {
                    for item in matches {
                        if let Some(item) = item.as_object_mut() {
                            item.remove("text");
                        }
                    }
                }
            }
        }
        result
    }

    fn refresh_registry(&mut self) -> Result<()> {
        let path = match &self.registry_path {
            Some(path) => path.clone(),
            None => {
                let path = catalog::state_dir()?.join("workspaces.json");
                self.registry_path = Some(path.clone());
                path
            }
        };
        let stamp = match std::fs::metadata(&path) {
            Ok(metadata) => {
                #[cfg(unix)]
                let ino = metadata.ino();
                #[cfg(not(unix))]
                let ino = 0_u64;
                Some((ino, metadata.len(), metadata.modified()?))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if stamp != self.registry_stamp {
            let registry = catalog::load()?;
            let parents_changed = self.registry.iter().filter(|spec| spec.managed_parent.is_none())
                .ne(registry.iter().filter(|spec| spec.managed_parent.is_none()));
            if parents_changed {
                self.cache_failures.clear();
                self.root_aliases.clear();
            } else {
                self.cache_failures.retain(|root, _| {
                    self.registry.iter().filter(|spec| root.starts_with(&spec.root))
                        .eq(registry.iter().filter(|spec| root.starts_with(&spec.root)))
                });
                self.root_aliases.retain(|_, root| {
                    self.registry.iter().find(|spec| &spec.root == root)
                        == registry.iter().find(|spec| &spec.root == root)
                });
            }
            self.registry = registry;
            self.registry_stamp = stamp;
            self.root_size_hints.retain(|root, (spec, _)| {
                self.registry
                    .iter()
                    .any(|current| &current.root == root && current == spec)
            });
            self.root_loads
                .retain(|root, _| self.registry.iter().any(|spec| &spec.root == root));
        }
        Ok(())
    }

    fn poll_interval(&self,explicit_pending:bool)->Duration {
        if explicit_pending || self.background.is_some() || self.preparation.is_some()
            || !self.pending_roots.is_empty() {
            ACTIVE_POLL_INTERVAL
        } else {
            IDLE_MAINTENANCE_INTERVAL
        }
    }

    fn maintain_idle_roots(&mut self) -> Result<()> {
        self.maintain_idle_roots_inner(false)
    }

    fn maintain_idle_roots_inner(&mut self, already_refreshed: bool) -> Result<()> {
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.check()?;
        }
        if !already_refreshed {
            if let Err(error) = self.refresh_registry() {
                eprintln!("background registry refresh failed: {error}");
                return Ok(());
            }
            if self.evict_idle_roots() {
                release_allocator_memory();
            }
        }
        self.drive_lazy_loads();
        for entry in self.roots.values_mut() {
            if let Err(error) = entry.maintenance_tick() {
                eprintln!("background root reconciliation failed: {error}");
            }
        }
        if self
            .last_pressure_check
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(5))
        {
            self.last_pressure_check = Some(std::time::Instant::now());
            if let Err(error) = self.memory.refresh(self.background.is_some()) {
                self.memory.last_error = Some(error.to_string());
                eprintln!("CCTX_MEMORY_REFRESH_FAILED {error}");
            }
            while self.roots.len() > self.memory.limits.root_limit()
                || self.roots.values().map(Watched::estimated_bytes).sum::<usize>()
                    > self.available_cache_budget()
            {
                let Some(root) = self.eviction_candidate(None) else { break; };
                self.evict(&root, "adaptive_memory_budget");
            }
            self.relieve_memory_pressure();
        }
        Ok(())
    }

    fn evict_idle_roots(&mut self) -> bool {
        let roots_before = self.roots.len();
        let expired: Vec<_> = self
            .roots
            .iter()
            .filter_map(|(root, entry)| {
                if !self
                    .registry
                    .iter()
                    .any(|spec| spec.root == *root && spec == &entry.spec)
                {
                    Some((root.clone(), "registration_changed"))
                } else if entry.idle_for(self.idle_timeout) {
                    Some((root.clone(), "idle_lease_expired"))
                } else {
                    None
                }
            })
            .collect();
        for (root, reason) in expired {
            self.evict(&root, reason);
        }
        self.roots.len() != roots_before
    }

    fn relieve_memory_pressure(&mut self) {
        let mut usage = self.pressure_usage_bytes();
        if usage <= self.memory.limits.high {
            return;
        }
        for entry in self.roots.values_mut() {
            entry.release_caches();
        }
        release_allocator_memory();
        usage = self.pressure_usage_bytes();
        while usage > self.memory.limits.high && !self.roots.is_empty() {
            let candidate = self
                .roots
                .iter()
                .max_by_key(|(_, entry)| entry.estimated_bytes());
            let Some(root) = candidate.map(|(root, _)| root.clone()) else {
                break;
            };
            eprintln!("evicting root under memory pressure: {}", root.display());
            self.evict(&root, "memory_pressure");
            release_allocator_memory();
            usage = self.pressure_usage_bytes();
        }
    }

    #[cfg(windows)]
    fn process_rss_bytes() -> usize {
        crate::memory::win_mem::process_memory().0
    }

    #[cfg(not(windows))]
    fn process_rss_bytes() -> usize {
        let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
            return 0;
        };
        status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0)
            .saturating_mul(1024)
    }

    fn cgroup_value(name: &str) -> Option<String> {
        std::fs::read_to_string(current_cgroup_dir().ok()?.join(name))
            .ok()
            .map(|value| value.trim().to_owned())
    }

    fn cgroup_current_bytes() -> usize {
        Self::cgroup_value("memory.current")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    }

    fn pressure_usage_bytes(&self) -> usize {
        let rss = Self::process_rss_bytes();
        let managed_high = Self::cgroup_value("memory.high")
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|high| high <= self.memory.limits.high);
        if managed_high || self.memory.is_managed() {
            rss.max(Self::cgroup_current_bytes())
        } else {
            rss
        }
    }

    fn process_memory(&self) -> Value {
        json!({
            "rss_bytes":Self::process_rss_bytes(),
            "pressure_usage_bytes":self.pressure_usage_bytes(),
            "pressure_threshold_bytes":self.memory.limits.high,
            "adaptive":self.memory.status(),
            "cgroup":{
                "current":Self::cgroup_value("memory.current"),
                "high":Self::cgroup_value("memory.high"),
                "max":Self::cgroup_value("memory.max"),
                "events":Self::cgroup_value("memory.events")
            },
            "io":{
                "stat":Self::cgroup_value("io.stat"),
                "max":Self::cgroup_value("io.max"),
                "pressure":Self::cgroup_value("io.pressure")
            },
            "configured_memory_high_bytes":self.memory.limits.high,
            "configured_memory_max_bytes":self.memory.limits.max
        })
    }
}

fn release_allocator_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        // All Rust owners have already been dropped; ask glibc to return free arenas.
        libc::malloc_trim(0);
    }
}

fn receive(reader: &mut BufReader<RpcStream>) -> Result<Option<Request>> {
    let mut bytes = Vec::new();
    let count = std::io::Read::take(reader, MAX_WIRE as u64 + 1).read_until(b'\n', &mut bytes)?;
    if count == 0 {
        return Ok(None);
    }
    if bytes.len() > MAX_WIRE || bytes.last() != Some(&b'\n') {
        return Err(invalid("request must be a newline-terminated message <=2MiB").into());
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn parse_json_frame(bytes: &[u8]) -> Result<Request> {
    if bytes.len() > MAX_WIRE || bytes.last() != Some(&b'\n') {
        return Err(invalid("request must be a newline-terminated message <=2MiB").into());
    }
    Ok(serde_json::from_slice(bytes)?)
}

fn receive_binary_buf(
    reader: &mut BufReader<RpcStream>,
    buf: &mut Vec<u8>,
) -> Result<Option<Request>> {
    let mut length = [0_u8; 4];
    match std::io::Read::read_exact(reader, &mut length) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_WIRE {
        return Err(invalid("binary request exceeds 2MiB").into());
    }
    buf.resize(length, 0);
    std::io::Read::read_exact(reader, buf)?;
    let request: BinaryRequestBorrow<'_> = bincode::deserialize(buf)?;
    Ok(Some(Request {
        root: request.root,
        name: request.name,
        arguments: serde_json::from_slice(request.arguments_json)?,
        compact: request.compact,
    }))
}

fn receive_framed_json_buf(
    reader: &mut BufReader<RpcStream>,
    buf: &mut Vec<u8>,
) -> Result<Option<Request>> {
    let mut length = [0_u8; 4];
    match std::io::Read::read_exact(reader, &mut length) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_WIRE {
        return Err(invalid("framed JSON request exceeds 2MiB").into());
    }
    buf.resize(length, 0);
    std::io::Read::read_exact(reader, buf)?;
    Ok(Some(serde_json::from_slice(buf)?))
}

fn respond(stream: &mut RpcStream, result: WireResult) -> Result<()> {
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let response = match result {
        Ok(result) => json!({"result":result}),
        Err(error) => json!({"error":error}),
    };
    serde_json::to_writer(&mut *stream, &response)?;
    stream.write_all(b"\n")?;
    Ok(())
}

#[derive(Serialize)]
struct TraceMeta<'a> {
    trace_id: &'a str,
    daemon_request_id: u64,
    queue_ms: f64,
    execute_ms: f64,
}

#[derive(Serialize)]
struct TracedRef<'a> {
    value: &'a Value,
    _cctx_trace: TraceMeta<'a>,
}

fn respond_protocol(
    stream: &mut RpcStream,
    protocol: Protocol,
    result: &WireResult,
) -> Result<(f64, f64)> {
    let mut frame = Vec::with_capacity(512);
    respond_protocol_buf(stream, protocol, result, None, &mut frame)
}

fn respond_protocol_buf(
    stream: &mut RpcStream,
    protocol: Protocol,
    result: &WireResult,
    trace: Option<TraceMeta<'_>>,
    frame: &mut Vec<u8>,
) -> Result<(f64, f64)> {
    let started = Instant::now();
    frame.clear();
    match protocol {
        Protocol::Bincode => {
            frame.extend_from_slice(&[0_u8; 4]);
            match result {
                Ok(value) => {
                    frame.push(1);
                    frame.extend_from_slice(&[0_u8; 8]);
                    if let Some(_cctx_trace) = trace {
                        serde_json::to_writer(&mut *frame, &TracedRef { value, _cctx_trace })?;
                    } else {
                        serde_json::to_writer(&mut *frame, value)?;
                    }
                    let payload_len = (frame.len() - 13) as u64;
                    frame[5..13].copy_from_slice(&payload_len.to_le_bytes());
                }
                Err(error) => {
                    frame.push(0);
                    if let Some(meta) = trace {
                        use std::io::Write as _;
                        frame.extend_from_slice(&[0_u8; 8]);
                        write!(frame, "{error} [cctx_trace={}]", meta.trace_id)?;
                        let payload_len = (frame.len() - 13) as u64;
                        frame[5..13].copy_from_slice(&payload_len.to_le_bytes());
                    } else {
                        let payload = error.as_bytes();
                        frame.extend_from_slice(&(payload.len() as u64).to_le_bytes());
                        frame.extend_from_slice(payload);
                    }
                }
            }
            let body_len = frame.len() - 4;
            if body_len > MAX_WIRE {
                return Err(invalid("response exceeds2MiB").into());
            }
            let len_bytes = u32::try_from(body_len)?.to_be_bytes();
            frame[..4].copy_from_slice(&len_bytes);
        }
        Protocol::FramedJson => {
            frame.extend_from_slice(&[0_u8; 4]);
            match result {
                Ok(value) => {
                    if let Some(_cctx_trace) = trace {
                        #[derive(Serialize)]
                        struct FramedTracedOk<'a> {
                            result: TracedRef<'a>,
                        }
                        serde_json::to_writer(
                            &mut *frame,
                            &FramedTracedOk {
                                result: TracedRef { value, _cctx_trace },
                            },
                        )?;
                    } else {
                        #[derive(Serialize)]
                        struct FramedOk<'a> {
                            result: &'a Value,
                        }
                        serde_json::to_writer(&mut *frame, &FramedOk { result: value })?;
                    }
                }
                Err(error) => {
                    if let Some(meta) = trace {
                        let msg = format!("{error} [cctx_trace={}]", meta.trace_id);
                        serde_json::to_writer(&mut *frame, &json!({"error": msg}))?;
                    } else {
                        serde_json::to_writer(&mut *frame, &json!({"error": error}))?;
                    }
                }
            }
            let body_len = frame.len() - 4;
            if body_len > MAX_WIRE {
                return Err(invalid("response exceeds2MiB").into());
            }
            let len_bytes = u32::try_from(body_len)?.to_be_bytes();
            frame[..4].copy_from_slice(&len_bytes);
        }
        Protocol::JsonLines => {
            match result {
                Ok(value) => {
                    if let Some(_cctx_trace) = trace {
                        serde_json::to_writer(
                            &mut *frame,
                            &json!({"result": TracedRef { value, _cctx_trace }}),
                        )?;
                    } else {
                        serde_json::to_writer(&mut *frame, &json!({"result":value}))?;
                    }
                }
                Err(error) => {
                    if let Some(meta) = trace {
                        let msg = format!("{error} [cctx_trace={}]", meta.trace_id);
                        serde_json::to_writer(&mut *frame, &json!({"error": msg}))?;
                    } else {
                        serde_json::to_writer(&mut *frame, &json!({"error":error}))?;
                    }
                }
            }
            if frame.len() > MAX_WIRE {
                return Err(invalid("response exceeds2MiB").into());
            }
            frame.push(b'\n');
        }
    }
    let serialize_ms = started.elapsed().as_secs_f64() * 1000.0;
    let write_started = Instant::now();
    stream.write_all(frame)?;
    Ok((serialize_ms, write_started.elapsed().as_secs_f64() * 1000.0))
}

fn handle_connection(
    mut stream: RpcStream,
    tx: std::sync::mpsc::SyncSender<Envelope>,
    active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    timing_tx: std::sync::mpsc::SyncSender<RequestTiming>,
    #[cfg(windows)] expected_secret: std::sync::Arc<String>,
) {
    let _connection = ActiveConnection(active);
    #[cfg(windows)]
    let _ = stream.set_nodelay(true);
    let mut reader = match stream.try_clone() {
        Ok(reader) => BufReader::new(reader),
        Err(error) => {
            let _ = respond(&mut stream, Err(error.to_string()));
            return;
        }
    };
    if let Err(error) = stream.set_read_timeout(Some(Duration::from_secs(2))) {
        let _ = respond(&mut stream, Err(error.to_string()));
        return;
    }
    #[cfg(windows)]
    {
        let mut auth = Vec::new();
        if std::io::Read::take(&mut reader, 256)
            .read_until(b'\n', &mut auth)
            .is_err()
            || auth.trim_ascii() != expected_secret.as_bytes()
        {
            let _ = respond(&mut stream, Err("unauthorized daemon connection".into()));
            return;
        }
    }
    let mut opening = Vec::new();
    let opening_result =
        std::io::Read::take(&mut reader, MAX_WIRE as u64 + 1).read_until(b'\n', &mut opening);
    if let Err(error) = opening_result {
        let _ = respond(&mut stream, Err(error.to_string()));
        return;
    }
    if opening.len() > MAX_WIRE {
        let _ = respond(&mut stream, Err("opening frame exceeds 2MiB".into()));
        return;
    }
    let protocol = if opening == BINARY_MAGIC {
        Protocol::Bincode
    } else if opening == FRAMED_JSON_MAGIC {
        Protocol::FramedJson
    } else {
        Protocol::JsonLines
    };
    let mut pending = if matches!(protocol, Protocol::Bincode | Protocol::FramedJson) {
        None
    } else {
        match parse_json_frame(&opening) {
            Ok(request) => Some(request),
            Err(error) => {
                let _ = respond(&mut stream, Err(error.to_string()));
                return;
            }
        }
    };
    let idle_read_timeout = if matches!(protocol, Protocol::Bincode | Protocol::FramedJson) {
        Duration::from_secs(300)
    } else {
        Duration::from_secs(30)
    };
    if let Err(error) = stream.set_read_timeout(Some(idle_read_timeout)) {
        let _ = respond(&mut stream, Err(error.to_string()));
        return;
    }
    if let Err(error) = stream.set_write_timeout(Some(Duration::from_secs(2))) {
        let _ = respond(&mut stream, Err(error.to_string()));
        return;
    }
    let mut read_buf = Vec::with_capacity(512);
    let mut write_buf = Vec::with_capacity(4096);
    let (reply_tx, response) = std::sync::mpsc::sync_channel(1);
    loop {
        let received = if let Some(request) = pending.take() {
            Ok(Some(request))
        } else {
            match protocol {
                Protocol::Bincode => receive_binary_buf(&mut reader, &mut read_buf),
                Protocol::FramedJson => receive_framed_json_buf(&mut reader, &mut read_buf),
                Protocol::JsonLines => receive(&mut reader),
            }
        };
        let mut request = match received {
            Ok(Some(request)) => request,
            Ok(None) => break,
            Err(error) => {
                if !error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    )
                }) {
                    let _ = respond_protocol(&mut stream, protocol, &Err(error.to_string()));
                }
                break;
            }
        };
        let prev_trace = request
            .arguments
            .as_object_mut()
            .and_then(|args| args.remove(PREV_TRACE_ARGUMENT));
        let trace_id = match request
            .arguments
            .as_object_mut()
            .and_then(|args| args.remove(TRACE_ARGUMENT))
        {
            None => None,
            Some(value) => match value
                .as_str()
                .ok_or_else(|| invalid("trace ID must be a string"))
                .and_then(|id| {
                    validate_trace_id(id)
                        .map_err(|error| invalid(error.to_string()))
                        .map(|()| id.to_owned())
                }) {
                Ok(id) => Some(id),
                Err(error) => {
                    let _ = respond_protocol(&mut stream, protocol, &Err(error.to_string()));
                    continue;
                }
            },
        };
        if tx
            .try_send(Envelope {
                request,
                protocol,
                reply: reply_tx.clone(),
                queued_at: Instant::now(),
                trace_id,
                prev_trace,
                first_dispatch: true,
                id: {
                    static NEXT_ID: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(1);
                    NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                },
            })
            .is_err()
        {
            let _ = respond_protocol(
                &mut stream,
                protocol,
                &Err("daemon request queue full".into()),
            );
            break;
        }
        match response.recv_timeout(Duration::from_secs(35)) {
            Ok(mut reply) => {
                let started = Instant::now();
                let trace_meta =
                    reply
                        .sample
                        .trace_id
                        .as_deref()
                        .map(|trace_id| TraceMeta {
                            trace_id,
                            daemon_request_id: reply.sample.id,
                            queue_ms: reply.sample.queue_ms,
                            execute_ms: reply.sample.execute_ms,
                        });
                let sent = respond_protocol_buf(
                    &mut stream,
                    protocol,
                    &reply.result,
                    trace_meta,
                    &mut write_buf,
                );
                reply.sample.response_ms = started.elapsed().as_secs_f64() * 1000.0;
                if let Ok((encode, write)) = &sent {
                    reply.sample.serialize_ms = *encode;
                    reply.sample.socket_write_ms = *write;
                }
                if timing_tx.try_send(reply.sample).is_err() {
                    eprintln!("RPC timing queue full; request timing sample dropped");
                }
                if sent.is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = respond_protocol(&mut stream, protocol, &Err(error.to_string()));
                break;
            }
        }
    }
}

pub fn serve() -> Result<()> {
    apply_service_io_guard()?;
    let memory_guard = match std::env::var_os("CCTX_MEMORY_GUARD") {
        None => false,
        Some(value) if value == "1" => true,
        _ => return Err(invalid("CCTX_MEMORY_GUARD must be1 when enabled").into()),
    };
    let dir = catalog::state_dir()?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("daemon.lock"))?;
    lock.try_lock_exclusive()
        .map_err(|e| invalid(format!("daemon already active or lock unavailable: {e}")))?;
    #[cfg(unix)]
    lock.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    let memory = crate::memory::Controller::start(current_cgroup_dir()?, memory_guard)?;
    let socket = catalog::socket_path()?;
    #[cfg(unix)]
    let listener = {
        match std::fs::symlink_metadata(&socket) {
            Ok(meta) => {
                use std::os::unix::fs::FileTypeExt;
                if !meta.file_type().is_socket() {
                    return Err(invalid("refusing to replace non-socket runtime path").into());
                }
                std::fs::remove_file(&socket)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        listener
    };
    #[cfg(windows)]
    let (listener, rpc_secret) = {
        if let Ok(meta) = std::fs::symlink_metadata(&socket) {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(invalid("refusing to replace non-file runtime path").into());
            }
            std::fs::remove_file(&socket)?;
        }
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let secret = std::sync::Arc::new(format!("{}-{}", new_trace_id(), new_trace_id()));
        std::fs::write(&socket, format!("{}\n{}\n", listener.local_addr()?, secret))?;
        (listener, secret)
    };
    eprintln!("code-context daemon ready at {}", socket.display());
    let (tx, rx) = std::sync::mpsc::sync_channel::<Envelope>(128);
    let (timing_tx, timing_rx) = std::sync::mpsc::sync_channel::<RequestTiming>(128);
    let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut server = Server {
        memory,
        active_connections: Some(active.clone()),
        runtime:Some(RuntimeIdentity::capture(&dir,&socket,&lock)?),
        ..Server::default()
    };
    let acceptor = std::thread::Builder::new()
        .name("cctx-acceptor".into())
        .stack_size(WORKER_STACK_BYTES)
        .spawn(move || -> Result<()> {
            use std::sync::atomic::Ordering;
            for stream in listener.incoming() {
                let stream = stream?;
                if active.fetch_add(1, Ordering::AcqRel) >= 128 {
                    active.fetch_sub(1, Ordering::AcqRel);
                    let mut stream = stream;
                    let _ = respond(&mut stream, Err("too many RPC connections".into()));
                    continue;
                }
                let active = active.clone();
                let worker_active = active.clone();
                let tx = tx.clone();
                let timing_tx = timing_tx.clone();
                #[cfg(windows)]
                let secret = rpc_secret.clone();
                if let Err(error) = std::thread::Builder::new()
                    .name("cctx-rpc".into())
                    .stack_size(WORKER_STACK_BYTES)
                    .spawn(move || {
                        handle_connection(
                            stream,
                            tx,
                            worker_active,
                            timing_tx,
                            #[cfg(windows)]
                            secret,
                        )
                    })
                {
                    eprintln!("failed to spawn RPC connection worker: {error}");
                    // The closure was dropped without running its RAII decrement.
                    active.fetch_sub(1, Ordering::AcqRel);
                }
            }
            Ok(())
        })?;
    let mut deferred = VecDeque::new();
    loop {
        server.poll_background();
        for timing in timing_rx.try_iter() {
            server.record_timing(timing);
        }
        let pending = deferred.len();
        for _ in 0..pending {
            let envelope = deferred.pop_front().expect("pending envelope");
            dispatch_envelope(&mut server, &mut deferred, envelope);
        }
        server
            .background_results
            .retain(|id, _| deferred.iter().any(|envelope| envelope.id == *id));
        match rx.recv_timeout(server.poll_interval(!deferred.is_empty())) {
            Ok(envelope) => {
                let is_trace = envelope.request.name == "code_trace";
                server.record_call(envelope.protocol, &envelope.request.name);
                dispatch_envelope(&mut server, &mut deferred, envelope);
                if !is_trace
                    && (server.background.is_some()
                        || server.preparation.is_some()
                        || !server.pending_roots.is_empty()
                        || server
                            .last_idle_maintenance
                            .is_none_or(|last| last.elapsed() >= Duration::from_millis(250)))
                {
                    server.last_idle_maintenance = Some(Instant::now());
                    server.maintain_idle_roots_inner(true)?;
                }
            }

            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                server.last_idle_maintenance = Some(Instant::now());
                server.maintain_idle_roots()?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    acceptor
        .join()
        .map_err(|_| invalid("daemon acceptor panicked"))?
}

fn dispatch_envelope(
    server: &mut Server,
    deferred: &mut VecDeque<Envelope>,
    mut envelope: Envelope,
) {
    if let Some(prev) = envelope.prev_trace.take() {
        let items = match prev {
            Value::Array(items) => items,
            single => vec![single],
        };
        for prev_trace in items {
            server.record_call(envelope.protocol, "code_trace");
            let outcome_key = if server.record_client_timing(prev_trace).is_ok() {
                "success"
            } else {
                server.record_failure(envelope.protocol, "code_trace");
                "invalid_request"
            };
            if let Some(count) = server.outcome_counts.get_mut(outcome_key) {
                *count += 1;
            } else {
                server.outcome_counts.insert(outcome_key.to_owned(), 1);
            }
        }
    }
    let prepared = if envelope.queued_at.elapsed() > Duration::from_secs(28) {
        Err(invalid("REQUEST_DEADLINE_EXCEEDED: source initialization/queue exceeded28s; no stale or partial success returned").into())
    } else if server.background_results.contains_key(&envelope.id) {
        Ok(true)
    } else {
        server.prepare_background(&envelope)
    };
    envelope.first_dispatch = false;
    let queue_ms = envelope.queued_at.elapsed().as_secs_f64() * 1000.0;
    let started = Instant::now();
    let operation = envelope.request.name.clone();
    let root = envelope.request.root.clone();
    let result = match prepared {
        Ok(false) => {
            if deferred.len() < 64 {
                deferred.push_back(envelope);
                return;
            }
            Err("daemon deferred request queue full".to_owned())
        }
        Err(error) => Err(error.to_string()),
        Ok(true) => match server.background_results.remove(&envelope.id) {
            Some(result) => result,
            None => server
                .call(envelope.request)
                .map_err(|error| error.to_string()),
        },
    };
    if result
        .as_ref()
        .is_err_and(|error| !error.starts_with("CACHE_MISS "))
    {
        server.record_failure(envelope.protocol, &operation);
    }
    let sample = RequestTiming {
        id: envelope.id,
        trace_id: envelope.trace_id,
        operation,
        root,
        queue_ms,
        execute_ms: started.elapsed().as_secs_f64() * 1000.0,
        response_ms: 0.0,
        serialize_ms: 0.0,
        socket_write_ms: 0.0,
        success: result.is_ok(),
        outcome: crate::outcome::classify(result.as_ref().err().map(String::as_str)),
    };
    if let Some(count) = server.outcome_counts.get_mut(sample.outcome) {
        *count += 1;
    } else {
        server
            .outcome_counts
            .insert(sample.outcome.to_owned(), 1);
    }
    server.abandon_background_reply(envelope.id);
    let _ = envelope.reply.send(TimedReply { result, sample });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::MemoryPolicy;

    #[test]
    fn native_bridge_frame_limits_and_eof_are_explicit() {
        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        assert!(read_native_frame(&mut empty).unwrap().is_none());
        for size in [0, MAX_WIRE as u32 + 1] {
            let mut input = std::io::Cursor::new(size.to_be_bytes());
            assert!(read_native_frame(&mut input).unwrap_err().to_string()
                .contains("frame must contain"));
        }
        let mut header = std::io::Cursor::new(vec![0, 0]);
        assert!(read_native_frame(&mut header).is_err());
        let mut payload = std::io::Cursor::new(vec![0, 0, 0, 3, b'a']);
        assert!(read_native_frame(&mut payload).is_err());
        let mut valid = std::io::Cursor::new(vec![0, 0, 0, 1, b'a']);
        assert_eq!(read_native_frame(&mut valid).unwrap(), Some(vec![b'a']));
    }

    #[test]
    fn hardening_expired_explicit_reply_discards_completed_result() {
        let mut server = Server::default();
        let id = 701;
        server.background_results.insert(id, Ok(json!({"completed":true})));
        let (reply, receiver) = std::sync::mpsc::sync_channel(1);
        let envelope = Envelope {
            request: Request {
                root: Some(PathBuf::from("/test/root")),
                name: "code_sync".into(),
                arguments: json!({}),
                compact: true,
            },
            protocol: Protocol::JsonLines,
            reply,
            queued_at: Instant::now() - Duration::from_secs(29),
            id,
            trace_id: None,
            prev_trace: None,
            first_dispatch: false,
        };
        dispatch_envelope(&mut server, &mut VecDeque::new(), envelope);
        assert!(receiver.recv().unwrap().result.unwrap_err()
            .contains("REQUEST_DEADLINE_EXCEEDED"));
        assert!(server.background_results.is_empty());
    }

    #[test]
    fn hardening_expired_explicit_reply_discards_late_worker_results() {
        let mut server = Server::default();
        let id = 702;
        let mut job = failed_cold_job(test_spec(PathBuf::from("/test/root")), 0, 0);
        job.owner = Some((id, true));
        server.background = Some(job);
        let (reply, receiver) = std::sync::mpsc::sync_channel(1);
        dispatch_envelope(&mut server, &mut VecDeque::new(), Envelope {
            request: Request {
                root: Some(PathBuf::from("/test/root")),
                name: "code_sync".into(),
                arguments: json!({}),
                compact: true,
            },
            protocol: Protocol::JsonLines,
            reply,
            queued_at: Instant::now() - Duration::from_secs(29),
            id,
            trace_id: None,
            prev_trace: None,
            first_dispatch: false,
        });
        assert!(receiver.recv().unwrap().result.is_err());
        let job = server.background.take().unwrap();
        assert!(job.reply_abandoned);
        server.finish_background_reply(&job, Ok(json!({"late":true})));
        server.finish_background_reply(&job, Err("late failure".into()));
        assert!(server.background_results.is_empty());
    }

    #[test]
    fn hardening_repeated_expiry_does_not_accumulate_results() {
        let mut server = Server::default();
        for id in 800..1312 {
            server.background_results.insert(id, Ok(json!({"payload":"x".repeat(4096)})));
            let (reply, receiver) = std::sync::mpsc::sync_channel(1);
            dispatch_envelope(&mut server, &mut VecDeque::new(), Envelope {
                request: Request {
                    root: Some(PathBuf::from("/test/root")),
                    name: "code_sync".into(),
                    arguments: json!({}),
                    compact: true,
                },
                protocol: Protocol::JsonLines,
                reply,
                queued_at: Instant::now() - Duration::from_secs(29),
                id,
                trace_id: None,
                prev_trace: None,
                first_dispatch: false,
            });
            assert!(receiver.recv().unwrap().result.is_err());
            assert!(server.background_results.is_empty());
        }
    }

    #[test]
    fn hardening_background_worker_failure_completes_its_explicit_owner() {
        let mut server = Server::default();
        let mut job = failed_cold_job(
            test_spec(PathBuf::from("/test/root")), GLOBAL_RESIDENT_TARGET, 1,
        );
        job.owner = Some((703, true));
        server.background = Some(job);
        server.poll_background();
        assert!(server.background_results.remove(&703).unwrap()
            .unwrap_err().contains("required_bytes"));
        assert!(server.background.is_none());
    }

    #[test]
    fn hardening_expired_failure_remains_visible_in_nonblocking_retry() {
        let mut server = Server::default();
        let root = PathBuf::from("/test/failed");
        server.record_cache_failure(root.clone(), "registration capacity reached".into());
        server.cache_failures.get_mut(&root).unwrap().0 =
            Instant::now() - CACHE_FAILURE_COOLDOWN - Duration::from_secs(1);
        let miss = server.request_cache(root).unwrap();
        assert_eq!(miss.state, "queued");
        assert!(miss.scheduled);
        assert!(miss.to_string().contains("registration capacity reached"));
        assert_eq!(server.pending_roots.len(), 1);
    }

    #[test]
    fn hardening_failure_storage_is_bounded() {
        let mut server = Server::default();
        for number in 0..MAX_CACHE_FAILURES * 2 {
            server.record_cache_failure(
                PathBuf::from(format!("/test/failed-{number}")),
                "bounded failure".into(),
            );
        }
        assert_eq!(server.cache_failures.len(), MAX_CACHE_FAILURES);
    }

    #[test]
    fn idle_loop_sleeps_without_delaying_active_work() {
        let mut server=Server::default();
        assert_eq!(server.poll_interval(false),Duration::from_secs(1));
        assert_eq!(server.poll_interval(true),Duration::from_millis(5));
        server.pending_roots.push_back(PendingRoot {requested:PathBuf::from("/test/root"),retry:0});
        assert_eq!(server.poll_interval(false),Duration::from_millis(5));
    }

    #[test]
    fn service_io_guard_preserves_stricter_limits_and_rejects_invalid_values() {
        assert_eq!(service_io_guard_rule("8:48","").unwrap(),"8:48 rbps=32000000 wbps=1000000");
        let existing="8:48 rbps=16000000 wbps=500000 riops=80 wiops=40\n";
        assert_eq!(service_io_guard_rule("8:48",existing).unwrap(),"8:48 rbps=16000000 wbps=500000");
        assert!(service_io_guard_rule("8:48","8:48 rbps=invalid wbps=max").is_err());
    }

    #[test]
    fn runtime_identity_rejects_a_removed_socket_or_replaced_lock() {
        let directory=tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(),std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket=directory.path().join("daemon.sock");
        let _listener=UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket,std::fs::Permissions::from_mode(0o600)).unwrap();
        let lock=std::fs::File::create(directory.path().join("daemon.lock")).unwrap();
        lock.set_permissions(std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut identity=RuntimeIdentity::capture(directory.path(),&socket,&lock).unwrap();
        std::fs::remove_file(directory.path().join("daemon.lock")).unwrap();
        let _replacement=std::fs::File::create(directory.path().join("daemon.lock")).unwrap();
        identity.last_check=Instant::now()-IDLE_MAINTENANCE_INTERVAL;
        assert!(identity.check().unwrap_err().to_string().contains("RUNTIME_REPLACED_OR_UNSAFE"));
        std::fs::remove_file(&socket).unwrap();
        assert!(identity.check().unwrap_err().to_string().contains("RUNTIME_UNAVAILABLE"));
    }

    fn test_spec(root: PathBuf) -> Spec {
        Spec {
            root,
            scopes: vec![".".into()],
            globs: vec!["*.rs".into()],
            excludes: Vec::new(),
            default_excludes: true,
            prewarm: false,
            policy: MemoryPolicy::default(),
            allow_worktrees: false,
            managed_parent: None,
        }
    }

    fn cached_test_roots(count: usize) -> (tempfile::TempDir, Server) {
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::default();
        for number in 0..count {
            let root = directory.path().join(format!("root-{number}"));
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("source.rs"), "fn source() {}\n").unwrap();
            let mut spec = test_spec(root.clone());
            spec.prewarm = number == count - 1;
            server.registry.push(spec.clone());
            server.roots.insert(root, Watched::new(spec).unwrap());
        }
        (directory, server)
    }

    fn failed_cold_job(spec: Spec, budget: usize, required: usize) -> BackgroundRoot {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        sender
            .send(Err(crate::resident::ResidentAdmissionError::new(
                "test build peak admission",
                required,
            )
            .into()))
            .unwrap();
        BackgroundRoot {
            spec,
            receiver,
            started: Instant::now(),
            budget,
            owner: None,
            reply_abandoned: false,
            retry: 0,
        }
    }

    #[test]
    fn cold_peak_failure_evicts_only_enough_lru_roots_and_keeps_recent_roots() {
        let (directory, mut server) = cached_test_roots(6);
        let root = directory.path().join("incoming");
        let spec = test_spec(root.clone());
        let used = server
            .roots
            .values()
            .map(Watched::estimated_bytes)
            .sum::<usize>();
        let headroom = Watched::budget_for_resident(0);
        let resident_required = GLOBAL_RESIDENT_TARGET - used - headroom + 1;
        server.background = Some(failed_cold_job(
            spec.clone(),
            GLOBAL_RESIDENT_TARGET - used - spec.policy.max_staged_bytes,
            resident_required,
        ));
        server.poll_background();
        assert_eq!(server.roots.len(), 5);
        assert_eq!(server.evictions.len(), 1);
        assert!(server.roots.values().any(|entry| entry.spec.prewarm));
        let retry = server.cold_load_budget(
            &spec,
            server.roots.values().map(Watched::estimated_bytes).sum(),
        );
        assert!(
            server
                .roots
                .values()
                .map(Watched::estimated_bytes)
                .sum::<usize>()
                + retry
                <= GLOBAL_RESIDENT_TARGET
        );
        assert_eq!(server.evictions[0]["reason"], "cold_build_peak_admission");
        assert_eq!(
            server.background_timings[0]["required_bytes"],
            Watched::budget_for_resident(resident_required)
        );
        assert!(
            server.background_timings[0]["error"]
                .as_str()
                .unwrap()
                .contains("required_bytes")
        );
        assert!(!server.cache_failures.contains_key(&root));
    }

    #[test]
    fn impossible_cold_build_does_not_purge_any_working_root() {
        let (directory, mut server) = cached_test_roots(3);
        let spec = test_spec(directory.path().join("too-large"));
        let root = spec.root.clone();
        server.background = Some(failed_cold_job(
            spec,
            GLOBAL_RESIDENT_TARGET,
            GLOBAL_RESIDENT_TARGET,
        ));
        server.poll_background();
        assert_eq!(server.roots.len(), 3);
        assert!(server.evictions.is_empty());
        assert!(server.cache_failures[&root].1.contains("required_bytes"));
    }

    #[test]
    fn later_admission_phase_can_retry_after_two_failures_even_without_other_roots() {
        let mut server = Server::default();
        let spec = test_spec(PathBuf::from("/test/incoming"));
        let mut job = failed_cold_job(spec.clone(), 456_306_878, 288_588_732);
        job.retry = 2;
        server.background = Some(job);
        server.poll_background();
        assert_eq!(server.pending_roots.len(), 1);
        assert_eq!(server.pending_roots[0].retry, 3);
        assert!(!server.cache_failures.contains_key(&spec.root));
        let budget = server.cold_load_budget(&spec, 0);
        assert!(budget > 456_306_878);
        assert!(budget <= GLOBAL_RESIDENT_TARGET);
    }

    #[test]
    fn cold_retry_budget_progresses_to_the_global_ceiling_and_stops() {
        let mut spec = test_spec(PathBuf::from("/test/bounded"));
        spec.policy.max_staged_bytes = 1;
        let mut budget = 0;
        for retry in 0..MAX_COLD_LOAD_RETRIES {
            let next = Server::cold_retry_budget(&spec, budget, budget + 1, retry).unwrap();
            assert!(next > budget);
            assert!(next <= GLOBAL_RESIDENT_TARGET);
            budget = next;
        }
        assert_eq!(budget, GLOBAL_RESIDENT_TARGET);
        assert!(Server::cold_retry_budget(&spec, budget, 1, 0).is_none());
        assert!(Server::cold_retry_budget(&spec, 1, GLOBAL_RESIDENT_TARGET + 1, 0).is_none());
        assert!(Server::cold_retry_budget(&spec, 1, 2, MAX_COLD_LOAD_RETRIES).is_none());
        assert!(Server::cold_retry_budget(&spec, usize::MAX, usize::MAX, 0).is_none());
    }

    #[test]
    fn cold_worktrees_use_only_actual_root_or_parent_size_hints() {
        let parent = test_spec(PathBuf::from("/test/parent"));
        let mut child = test_spec(PathBuf::from("/test/worktree"));
        child.managed_parent = Some(parent.root.clone());
        let mut server = Server::default();
        let hint = 512 * 1024 * 1024;
        server
            .root_size_hints
            .insert(parent.root.clone(), (parent, hint));
        let used = 768 * 1024 * 1024;
        assert_eq!(
            server.cold_load_budget(&child, used),
            hint + child.policy.max_staged_bytes
        );
        server.root_size_hints.clear();
        assert_eq!(
            server.cold_load_budget(&child, used),
            GLOBAL_RESIDENT_TARGET - used - 128 * 1024 * 1024
        );
        server
            .root_size_hints
            .insert(child.root.clone(), (child.clone(), hint * 2));
        assert_eq!(
            server.cold_load_budget(&child, used),
            hint * 2 + child.policy.max_staged_bytes
        );
    }

    #[test]
    fn preparation_policy_snapshot_distinguishes_authorization_from_managed_churn() {
        let parent = test_spec(PathBuf::from("/test/parent"));
        let mut linked = test_spec(PathBuf::from("/test/linked"));
        linked.managed_parent = Some(parent.root.clone());
        let (_, receiver) = std::sync::mpsc::sync_channel(1);
        let job = RootPreparation {
            requested: linked.root.clone(), receiver, retry: 0,
            policy_snapshot: vec![parent.clone()],
        };
        assert!(!job.policy_changed(&[parent.clone(), linked]));
        let mut authorized = parent;
        authorized.allow_worktrees = true;
        assert!(job.policy_changed(&[authorized]));
    }

    #[test]
    fn native_outcome_and_windows_host_reports_are_validated_end_to_end() {
        let mut server = Server::default();
        let sample = json!({
            "trace_id":"native-host-1","origin":"native","operation":"code_read","root":null,
            "queue_ms":0.0,"prepare_ms":0.0,"ipc_ms":2.0,"serialize_ms":0.1,
            "write_ms":0.1,"total_ms":3.0,"success":true,"outcome":"success",
            "host_memory":{"platform":"win32","total_bytes":64_u64*1024*1024*1024,
                "available_bytes":48_u64*1024*1024*1024},
        });
        server.record_client_timing(sample.clone()).unwrap();
        assert_eq!(server.memory.status()["windows_host_fresh"], true);
        for (key, value) in [
            ("outcome", json!("unclassified")),
            ("host_memory", json!({"platform":"win32","total_bytes":0,"available_bytes":1})),
            ("host_memory", json!({"platform":"win32","total_bytes":10,"available_bytes":11})),
            ("host_memory", json!({"platform":"win32","total_bytes":10,"available_bytes":1,"extra":true})),
        ] {
            let mut bad = sample.clone();
            bad[key] = value;
            assert!(server.record_client_timing(bad).is_err());
        }
        assert_eq!(server.client_timings.len(), 1);
    }

    #[test]
    fn client_trace_history_is_bounded_and_rejects_invalid_or_uncorrelated_metrics() {
        let mut server = Server::default();
        let sample = json!({
            "trace_id":"test-1","origin":"mcp","operation":"code_read","root":null,
            "queue_ms":0.0,"prepare_ms":0.0,"ipc_ms":2.0,"serialize_ms":0.1,
            "write_ms":0.1,"total_ms":3.0,"success":true,"daemon":{
                "trace_id":"test-1","daemon_request_id":1,"queue_ms":0.1,"execute_ms":1.0
            }
        });
        for _ in 0..70 {
            server.record_client_timing(sample.clone()).unwrap();
        }
        assert_eq!(server.client_timings.len(), 64);
        for (key, value) in [
            ("total_ms", json!(-1)),
            ("trace_id", json!("bad\nid")),
            ("root", json!(42)),
            ("unknown", json!(true)),
            ("started_unix_ms", json!("not a timestamp")),
            ("completed_unix_ms", json!(-1)),
            (
                "daemon",
                json!({"trace_id":"different","daemon_request_id":1,"queue_ms":0.0,"execute_ms":1.0}),
            ),
        ] {
            let mut bad = sample.clone();
            bad[key] = value;
            assert!(server.record_client_timing(bad).is_err(), "{key}");
        }
        let mut missing = sample;
        missing.as_object_mut().unwrap().remove("ipc_ms");
        assert!(server.record_client_timing(missing).is_err());
        assert_eq!(server.client_timings.len(), 64);
    }

    #[test]
    fn trace_unwrap_preserves_array_and_null_values_and_rejects_mismatches() {
        for value in [json!([{"path":"a.rs","lines":["x"]}]), Value::Null] {
            let (result, trace) = unwrap_trace(
                json!({"value":value,"_cctx_trace":{"trace_id":"test-1"}}),
                "test-1",
            )
            .unwrap();
            assert_eq!(result, value);
            assert_eq!(trace["trace_id"], "test-1");
        }
        assert!(
            unwrap_trace(
                json!({"value":1,"_cctx_trace":{"trace_id":"other"}}),
                "test-1"
            )
            .is_err()
        );
        assert!(unwrap_trace(json!({"_cctx_trace":{"trace_id":"test-1"}}), "test-1").is_err());
    }

    #[test]
    fn framed_json_errors_remain_length_prefixed() {
        #[cfg(unix)]
        let (mut server, client) = UnixStream::pair().unwrap();
        #[cfg(windows)]
        let (mut server, client) = {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let addr = listener.local_addr().unwrap();
            let client = std::net::TcpStream::connect(addr).unwrap();
            let (server, _) = listener.accept().unwrap();
            (server, client)
        };
        let stats =
            respond_protocol(&mut server, Protocol::FramedJson, &Err("queue full".into())).unwrap();
        assert!(stats.0 >= 0.0 && stats.1 >= 0.0);
        let mut reader = BufReader::new(client);
        let mut frame = Vec::new();
        assert!(read_native_frame_buf(&mut reader, &mut frame).unwrap());
        assert_eq!(
            serde_json::from_slice::<Value>(&frame).unwrap()["error"],
            "queue full"
        );
    }

    #[test]
    fn binary_application_errors_are_typed_and_not_transport_retries() {
        let error: Box<dyn std::error::Error> = Box::new(ApplicationError("invalid range".into()));
        assert!(is_application_error(error.as_ref()));
        assert!(!is_application_error(&invalid("connection reset")));
        assert_eq!(IDLE_ROOT_TIMEOUT.as_secs(), 900);
        assert_eq!(catalog::MAX_REGISTERED_ROOTS, 256);
    }

    #[test]
    fn cache_misses_coalesce_and_bound_the_root_queue() {
        let mut server = Server::default();
        for _ in 0..4 {
            assert_eq!(
                server
                    .request_cache(PathBuf::from("/test/root"))
                    .unwrap()
                    .state,
                "queued"
            );
        }
        assert_eq!(server.pending_roots.len(), 1);
        for n in 1..MAX_PENDING_ROOTS {
            server
                .request_cache(PathBuf::from(format!("/test/{n}")))
                .unwrap();
        }
        let miss = server
            .request_cache(PathBuf::from("/test/overflow"))
            .unwrap();
        assert_eq!(miss.state, "queue_full");
        assert!(!miss.scheduled);
        assert_eq!(server.pending_roots.len(), MAX_PENDING_ROOTS);
    }

    #[test]
    fn failed_loads_surface_the_cause_and_wait_for_a_new_demand_after_cooldown() {
        let mut server = Server::default();
        let root = PathBuf::from("/test/failed");
        server.record_cache_failure(root.clone(), "excluded by registration policy".into());
        let error = server.request_cache(root.clone()).unwrap_err().to_string();
        assert!(error.starts_with("CACHE_LOAD_FAILED "));
        assert!(error.contains("excluded by registration policy"));
        assert!(server.pending_roots.is_empty());
        server.cache_failures.get_mut(&root).unwrap().0 =
            Instant::now() - CACHE_FAILURE_COOLDOWN - Duration::from_secs(1);
        assert_eq!(server.request_cache(root).unwrap().state, "queued");
        assert_eq!(server.pending_roots.len(), 1);
    }

    #[test]
    fn even_legacy_prewarm_roots_expire_and_a_later_miss_queues_reload() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("source.rs"), "source").unwrap();
        let spec = Spec {
            root: root.path().to_path_buf(),
            scopes: vec![".".into()],
            globs: vec!["*.rs".into()],
            excludes: Vec::new(),
            default_excludes: true,
            prewarm: true,
            policy: MemoryPolicy::default(),
            allow_worktrees: false,
            managed_parent: None,
        };
        let watched = Watched::new(spec.clone()).unwrap();
        let canonical = catalog::canonicalize(root.path()).unwrap();
        let mut server = Server {
            registry: vec![spec],
            idle_timeout: Duration::ZERO,
            ..Server::default()
        };
        server.roots.insert(canonical.clone(), watched);
        assert!(server.registry[0].prewarm);
        std::thread::sleep(Duration::from_millis(1));
        assert!(server.evict_idle_roots());
        assert!(server.roots.is_empty());
        assert_eq!(
            server.evictions.back().unwrap()["reason"],
            "idle_lease_expired"
        );
        let miss = server.request_cache(canonical.clone()).unwrap();
        assert_eq!(miss.state, "queued");
        assert_eq!(server.pending_roots[0].requested, canonical);
    }
}
