use crate::exclusion_policy;
use crate::policy::{IO_WORKER_STACK_BYTES, MemoryPolicy};
use crate::{Result, invalid};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::collections::{BTreeSet, HashSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const MAX_CAPTURE: usize = 8 * 1024 * 1024;
pub const MAX_FILE: u64 = 2 * 1024 * 1024;
pub const MAX_RESPONSE: usize = 256 * 1024;
const MAX_SCOPES: usize = 32;
const MAX_GLOBS: usize = 128;
const MAX_SCOPE_BYTES: usize = 4096;
const MAX_GLOB_BYTES: usize = 1024;
const MAX_CONFIG_BYTES: usize = 64 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
mod candidate_policy_tests {
    use super::*;

    fn reference(workspace:&Workspace,name:&str)->Result<bool> {
        let path=Path::new(name);
        safe_relative(path)?;
        let path=normalize(path);
        if !workspace.scopes.iter().any(|scope|scope=="."||path.starts_with(scope)) {return Ok(false);}
        let included=workspace.globs.iter().map(|pattern|Glob::new(pattern))
            .collect::<std::result::Result<Vec<_>,_>>()?.iter().any(|glob|{
                let matcher=glob.compile_matcher();
                matcher.is_match(&path)||path.file_name().is_some_and(|name|matcher.is_match(name))
            });
        if !included {return Ok(false);}
        for pattern in workspace.effective_excludes() {
            let matcher=Glob::new(&pattern)?.compile_matcher();
            if path.ancestors().filter(|p|!p.as_os_str().is_empty()).any(|ancestor|
                matcher.is_match(ancestor)||ancestor.file_name().is_some_and(|name|matcher.is_match(name))) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    #[test]
    fn compiled_policy_matches_existing_scope_glob_exclusion_and_hidden_rules() {
        let root=tempfile::tempdir().unwrap();
        let workspace=Workspace::new_with_excludes(root.path(),vec![".".into()],
            vec!["*.rs".into(),"*.ts".into(),"docs/**/*.md".into(),"Makefile".into()],
            vec!["**/private/**".into(),"**/manual.rs".into()],true).unwrap();
        let policy=CandidatePolicy::compile(workspace.candidate_policy_key()).unwrap();
        for directory in ["","src/","src/private/","target/","src/target/","node_modules/","docs/a/"] {
            for file in ["lib.rs","app.ts","manual.rs","a.md","Makefile","readme.txt"] {
                let name=format!("{directory}{file}");
                assert_eq!(policy.candidate_path(&name).unwrap(),reference(&workspace,&name).unwrap(),"{name}");
            }
        }
        for name in [".hidden.rs",".github/a.rs","../escape.rs","/absolute.rs"] {
            assert!(policy.candidate_path(name).is_err(),"{name}");
        }
    }
}

pub struct FileInventory {
    pub files: HashSet<PathBuf>,
    pub(crate) stamps: std::collections::BTreeMap<String, crate::incremental::Stamp>,
    pub total_path_bytes: usize,
    pub metadata_source_bytes: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CandidatePolicyKey {
    scopes: Vec<String>,
    globs: Vec<String>,
    excludes: Vec<String>,
}

pub(crate) struct CandidatePolicy {
    pub(crate) key: CandidatePolicyKey,
    includes: GlobSet,
    excludes: GlobSet,
}

impl CandidatePolicy {
    pub(crate) fn compile(key: CandidatePolicyKey) -> Result<Self> {
        let mut includes = GlobSetBuilder::new();
        for pattern in &key.globs {
            includes.add(Glob::new(pattern)?);
        }
        let mut excludes = GlobSetBuilder::new();
        for pattern in &key.excludes {
            excludes.add(Glob::new(pattern)?);
        }
        Ok(Self { key, includes:includes.build()?, excludes:excludes.build()? })
    }

    pub(crate) fn candidate_path(&self, path: &str) -> Result<bool> {
        Ok(self.rejection_reason(path)?.is_none())
    }

    pub(crate) fn rejection_reason(&self, path: &str) -> Result<Option<&'static str>> {
        let path=Path::new(path);
        safe_relative(path)?;
        let path=normalize(path);
        if !self.key.scopes.iter().any(|scope|scope=="." || path.starts_with(scope)) {
            return Ok(Some("outside_scope"));
        }
        if path.ancestors().filter(|ancestor|!ancestor.as_os_str().is_empty())
            .any(|ancestor|self.excludes.is_match(ancestor) ||
                ancestor.file_name().is_some_and(|name|self.excludes.is_match(name))) {
            return Ok(Some("excluded_by_rule"));
        }
        if !self.includes.is_match(&path)
            && !path.file_name().is_some_and(|name|self.includes.is_match(name)) {
            return Ok(Some("unsupported_glob"));
        }
        Ok(None)
    }
}

#[derive(Clone, Debug)]
pub struct Workspace {
    pub root: PathBuf,
    pub scopes: Vec<String>,
    pub globs: Vec<String>,
    pub excludes: Vec<String>,
    pub default_excludes: bool,
}

impl Workspace {
    pub fn new(root: &Path, scopes: Vec<String>, globs: Vec<String>) -> Result<Self> {
        Self::new_with_excludes(root, scopes, globs, Vec::new(), true)
    }

    pub fn new_with_excludes(
        root: &Path,
        scopes: Vec<String>,
        globs: Vec<String>,
        excludes: Vec<String>,
        default_excludes: bool,
    ) -> Result<Self> {
        let root = crate::catalog::canonicalize(root)?;
        if !root.is_dir() || scopes.is_empty() || globs.is_empty() {
            return Err(
                invalid("root must be a directory; explicit scopes and globs required").into(),
            );
        }
        if scopes.len() > MAX_SCOPES || globs.len() > MAX_GLOBS || excludes.len() > MAX_GLOBS {
            return Err(
                invalid("at most 32 scopes and 128 include/exclude globs are allowed").into(),
            );
        }
        if scopes.iter().any(|scope| scope.len() > MAX_SCOPE_BYTES)
            || globs
                .iter()
                .chain(excludes.iter())
                .any(|glob| glob.len() > MAX_GLOB_BYTES)
            || scopes
                .iter()
                .chain(globs.iter())
                .chain(excludes.iter())
                .map(String::len)
                .sum::<usize>()
                > MAX_CONFIG_BYTES
        {
            return Err(invalid("workspace scope/glob configuration is too large").into());
        }
        if globs
            .iter()
            .chain(excludes.iter())
            .any(|glob| glob.is_empty() || glob.starts_with('!') || glob.contains(','))
        {
            return Err(
                invalid("source globs must be nonempty positive patterns without commas").into(),
            );
        }
        let mut normalized_scopes = Vec::new();
        for scope in &scopes {
            safe_relative(Path::new(scope))?;
            let path = root.join(scope);
            reject_symlinks(&root, Path::new(scope))?;
            if !path.is_dir() || !crate::catalog::canonicalize(&path)?.starts_with(&root) {
                return Err(invalid("scopes must be directories inside root").into());
            }
            let canonical = crate::catalog::canonicalize(&path)?;
            let relative = normalize(canonical.strip_prefix(&root)?);
            let name = if relative.as_os_str().is_empty() {
                ".".into()
            } else {
                relative.to_string_lossy().into_owned()
            };
            normalized_scopes.push(name);
        }
        normalized_scopes.sort_by_key(|scope| {
            (
                if scope == "." {
                    0
                } else {
                    Path::new(scope).components().count()
                },
                scope.clone(),
            )
        });
        normalized_scopes.dedup();
        let mut minimal_scopes: Vec<String> = Vec::new();
        for scope in normalized_scopes {
            if !minimal_scopes
                .iter()
                .any(|parent| parent == "." || Path::new(&scope).starts_with(Path::new(parent)))
            {
                minimal_scopes.push(scope);
            }
        }
        let mut globs = globs;
        globs.sort();
        globs.dedup();
        let mut excludes = excludes;
        excludes.sort();
        excludes.dedup();
        Ok(Self {
            root,
            scopes: minimal_scopes,
            globs,
            excludes,
            default_excludes,
        })
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(rg_executable());
        command.current_dir(&self.root).arg("--no-config");
        #[cfg(windows)]
        command.arg("--path-separator=/");
        // Explicit search roots do not reliably inherit cwd ignore rules outside
        // Git repositories. Apply root policy explicitly for every discovery.
        for name in [".gitignore", ".ignore"] {
            let path = self.root.join(name);
            if path.is_file() {
                command.arg("--ignore-file").arg(path);
            }
        }
        command.args(["--no-hidden", "--glob", "!.*"]);
        if self.default_excludes {
            for pattern in &exclusion_policy::detect(&self.root, &self.scopes).patterns {
                command.arg("--glob").arg(format!("!{pattern}"));
            }
        }
        for glob in &self.globs {
            command.arg("--type-add").arg(format!("cctx:{glob}"));
        }
        command.args(["--type", "cctx"]);
        for exclude in &self.excludes {
            command.arg("--glob").arg(format!("!{exclude}"));
        }
        command
    }

    pub fn effective_excludes(&self) -> Vec<String> {
        let mut excludes = self.excludes.clone();
        excludes.extend(self.exclusion_summary().patterns);
        excludes
    }

    pub fn detected_profiles(&self) -> Vec<&'static str> {
        self.exclusion_summary().profiles
    }

    pub fn exclusion_summary(&self) -> exclusion_policy::EffectiveExclusions {
        if self.default_excludes {
            exclusion_policy::detect(&self.root, &self.scopes)
        } else {
            exclusion_policy::EffectiveExclusions {
                profiles: Vec::new(),
                patterns: Vec::new(),
            }
        }
    }

    pub fn watch_directories(&self, policy: &MemoryPolicy) -> Result<BTreeSet<PathBuf>> {
        let inventory = self.files_limited(policy)?;
        self.watch_directories_for(inventory.files.iter().map(PathBuf::as_path), policy)
    }

    pub fn watch_directories_for<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a Path>,
        policy: &MemoryPolicy,
    ) -> Result<BTreeSet<PathBuf>> {
        let mut directories = BTreeSet::new();
        let mut path_bytes = 0usize;
        let mut insert = |path: &Path| -> Result<()> {
            if directories.insert(path.to_path_buf()) {
                path_bytes = path_bytes.saturating_add(path.as_os_str().len());
                if directories.len() > policy.max_watch_dirs {
                    return Err(invalid(format!(
                        "workspace exceeds max_watch_dirs ({})",
                        policy.max_watch_dirs
                    ))
                    .into());
                }
                if path_bytes > policy.max_total_path_bytes {
                    return Err(invalid(format!(
                        "watch directories exceed max_total_path_bytes ({})",
                        policy.max_total_path_bytes
                    ))
                    .into());
                }
            }
            Ok(())
        };
        insert(Path::new(""))?;
        for scope in &self.scopes {
            let mut directory = normalize(Path::new(scope));
            loop {
                insert(&directory)?;
                if !directory.pop() {
                    break;
                }
            }
        }
        for path in paths {
            let mut directory = path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
            loop {
                insert(&directory)?;
                if !directory.pop() {
                    break;
                }
            }
        }
        Ok(directories)
    }

    pub fn files(&self) -> Result<HashSet<PathBuf>> {
        Ok(self
            .files_limited(&MemoryPolicy {
                max_file_bytes: MAX_FILE as usize,
                ..MemoryPolicy::default()
            })?
            .files)
    }

    pub fn files_limited(&self, policy: &MemoryPolicy) -> Result<FileInventory> {
        self.files_limited_at(policy, &self.scopes, false)
    }

    pub(crate) fn files_limited_at(
        &self,
        policy: &MemoryPolicy,
        targets: &[String],
        shallow: bool,
    ) -> Result<FileInventory> {
        policy.validate().map_err(invalid)?;
        let mut command = self.command();
        if shallow {
            command.args(["--max-depth", "1"]);
        }
        command.args(["--files", "--null", "--"]).args(targets);
        let output = capture(&mut command)?;
        let mut paths = HashSet::new();
        let mut stamps = std::collections::BTreeMap::new();
        let mut checked_dirs: HashSet<PathBuf> = HashSet::new();
        let mut total_path_bytes = 0usize;
        let mut metadata_source_bytes = 0u64;
        for path in output.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            let text = std::str::from_utf8(path)?;
            let path = Path::new(text);
            safe_relative(path)?;
            let metadata = reject_symlinks_metadata_cached(&self.root, path, &mut checked_dirs)?;
            let normalized = normalize(path);
            if paths.insert(normalized.clone()) {
                total_path_bytes = total_path_bytes.saturating_add(path.as_os_str().len());
                if paths.len() > policy.max_files {
                    return Err(invalid(format!(
                        "workspace exceeds max_files ({})",
                        policy.max_files
                    ))
                    .into());
                }
                if total_path_bytes > policy.max_total_path_bytes {
                    return Err(invalid(format!(
                        "workspace exceeds max_total_path_bytes ({})",
                        policy.max_total_path_bytes
                    ))
                    .into());
                }
                if metadata.len() > policy.max_file_bytes as u64 {
                    return Err(invalid(format!(
                        "file exceeds configured max_file_bytes: {}",
                        normalized.display()
                    ))
                    .into());
                }
                metadata_source_bytes = metadata_source_bytes.saturating_add(metadata.len());
                if metadata_source_bytes > policy.max_source_bytes as u64 {
                    return Err(invalid(format!(
                        "workspace metadata exceeds max_source_bytes ({})",
                        policy.max_source_bytes
                    ))
                    .into());
                }
                let normalized_name = normalized
                    .to_str()
                    .ok_or_else(|| invalid("non-UTF8 path"))?
                    .to_owned();
                stamps.insert(
                    normalized_name,
                    crate::incremental::stamp_from_metadata(&metadata)?,
                );
            }
        }
        Ok(FileInventory {
            files: paths,
            stamps,
            total_path_bytes,
            metadata_source_bytes,
        })
    }

    pub(crate) fn candidate_policy_key(&self) -> CandidatePolicyKey {
        CandidatePolicyKey {
            scopes:self.scopes.clone(),globs:self.globs.clone(),
            excludes:self.effective_excludes(),
        }
    }

    pub(crate) fn unindexed_reason(&self, path: &str, policy: &MemoryPolicy) -> Result<&'static str> {
        let matcher = CandidatePolicy::compile(self.candidate_policy_key())?;
        if let Some(reason) = matcher.rejection_reason(path)? { return Ok(reason); }
        let relative = normalize(Path::new(path));
        match std::fs::symlink_metadata(self.root.join(&relative)) {
            Ok(meta) if meta.file_type().is_symlink() => return Ok("symlink_not_allowed"),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("missing_on_disk"),
            Err(error) => return Err(error.into()),
        }
        reject_symlinks(&self.root, &relative)?;
        let parent = relative.parent().unwrap_or(Path::new(""));
        let target = if parent.as_os_str().is_empty() { ".".to_owned() }
            else { parent.to_string_lossy().into_owned() };
        let inventory = self.files_limited_at(policy, &[target], true)?;
        Ok(if inventory.files.contains(&relative) { "not_yet_indexed" } else { "ignored_by_discovery" })
    }

    pub fn read_allowed(&self, path: &str, allowed: &HashSet<PathBuf>) -> Result<Vec<u8>> {
        self.read_allowed_with_limit(path, allowed, MAX_FILE as usize)
    }

    pub fn read_allowed_with_limit(
        &self,
        path: &str,
        allowed: &HashSet<PathBuf>,
        max_file_bytes: usize,
    ) -> Result<Vec<u8>> {
        let path = Path::new(path);
        safe_relative(path)?;
        let normalized = normalize(path);
        if !allowed.contains(&normalized) {
            return Err(
                invalid("file is outside configured scope, ignored, or excluded by glob").into(),
            );
        }
        self.read_known_with_limit(path, max_file_bytes)
    }

    pub fn read_known_with_limit(&self, path: &Path, max_file_bytes: usize) -> Result<Vec<u8>> {
        safe_relative(path)?;
        let metadata = reject_symlinks_metadata(&self.root, path)?;
        if metadata.len() > max_file_bytes as u64 {
            return Err(invalid(format!(
                "binary file or file larger than configured limit is unsupported: {}",
                path.display()
            ))
            .into());
        }
        let absolute = self.root.join(path);
        let capacity = (metadata.len() as usize).min(max_file_bytes).saturating_add(1);
        let mut bytes = Vec::with_capacity(capacity);
        std::fs::File::open(absolute)?
            .take(max_file_bytes as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > max_file_bytes || memchr::memchr(0, &bytes).is_some() {
            return Err(invalid(format!(
                "binary file or file larger than configured limit is unsupported: {}",
                path.display()
            ))
            .into());
        }
        Ok(bytes)
    }
}

fn rg_executable() -> &'static std::ffi::OsStr {
    static RG: std::sync::OnceLock<std::ffi::OsString> = std::sync::OnceLock::new();
    RG.get_or_init(|| {
        if let Some(configured) = std::env::var_os("CCTX_RG")
            && !configured.is_empty()
        {
            return configured;
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            if Command::new("rg")
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .status()
                .is_ok_and(|s| s.success())
            {
                return "rg".into();
            }
            for env_key in ["LOCALAPPDATA", "ProgramFiles"] {
                if let Some(base) = std::env::var_os(env_key) {
                    let vscode_dir = PathBuf::from(base).join("Programs").join("Microsoft VS Code");
                    let alt_dir = PathBuf::from(std::env::var_os(env_key).unwrap_or_default())
                        .join("Microsoft VS Code");
                    for root in [vscode_dir, alt_dir] {
                        if !root.is_dir() {
                            continue;
                        }
                        for rel in [
                            "resources/app/node_modules.asar.unpacked/@vscode/ripgrep/bin/rg.exe",
                            "resources/app/node_modules.asar.unpacked/@vscode/ripgrep-universal/bin/win32-x64/rg.exe",
                            "resources/app/node_modules/@vscode/ripgrep/bin/rg.exe",
                        ] {
                            let candidate = root.join(rel);
                            if candidate.is_file() {
                                return candidate.into_os_string();
                            }
                        }
                        if let Ok(entries) = std::fs::read_dir(&root) {
                            for entry in entries.flatten() {
                                for rel in [
                                    "resources/app/node_modules.asar.unpacked/@vscode/ripgrep/bin/rg.exe",
                                    "resources/app/node_modules.asar.unpacked/@vscode/ripgrep-universal/bin/win32-x64/rg.exe",
                                    "resources/app/node_modules/@vscode/ripgrep/bin/rg.exe",
                                ] {
                                    let candidate = entry.path().join(rel);
                                    if candidate.is_file() {
                                        return candidate.into_os_string();
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        "rg".into()
    })
    .as_os_str()
}

pub fn normalize(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(raw) = path.to_str()
            && !raw.is_empty()
            && !raw.contains('\\')
            && !raw.contains("//")
            && !raw.starts_with("./")
            && !raw.contains("/./")
            && !raw.ends_with("/.")
            && raw != "."
        {
            return path.to_path_buf();
        }
        let mut out = std::ffi::OsString::new();
        for part in path.components().filter(|part| *part != Component::CurDir) {
            if !out.is_empty() {
                out.push("/");
            }
            out.push(part.as_os_str());
        }
        PathBuf::from(out)
    }
    #[cfg(not(windows))]
    {
        if let Some(raw) = path.to_str()
            && !raw.is_empty()
            && !raw.contains("//")
            && !raw.starts_with("./")
            && !raw.contains("/./")
            && !raw.ends_with("/.")
            && raw != "."
        {
            return path.to_path_buf();
        }
        path.components()
            .filter(|part| *part != Component::CurDir)
            .collect()
    }
}

pub fn safe_relative(path: &Path) -> Result<()> {
    if let Some(raw) = path.to_str()
        && !raw.is_empty()
        && !raw.starts_with(['/', '\\', '.'])
        && !raw.contains(['\\', ':'])
        && !raw.contains("/.")
        && !raw.contains("//")
    {
        return Ok(());
    }
    if path.as_os_str().is_empty()
        || path.components().any(|c| match c {
            Component::Normal(name) => name.to_string_lossy().starts_with('.'),
            Component::CurDir => false,
            _ => true,
        })
    {
        return Err(invalid(format!(
            "only non-hidden, relative paths without traversal are allowed: {} (hidden paths like .github/.vscode are excluded by policy; use a permitted host read/search tool directly without retrying CCTX)",
            path.display()
        ))
        .into());
    }
    Ok(())
}

pub fn reject_symlinks(root: &Path, path: &Path) -> Result<()> {
    let _ = reject_symlinks_metadata(root, path)?;
    Ok(())
}

pub(crate) fn reject_symlinks_metadata(root: &Path, path: &Path) -> Result<std::fs::Metadata> {
    let mut current = root.to_path_buf();
    let mut last_meta = None;
    for component in path.components() {
        current.push(component);
        let meta = std::fs::symlink_metadata(&current)?;
        if meta.file_type().is_symlink() {
            return Err(invalid("symlinks are unsupported in this local experiment").into());
        }
        last_meta = Some(meta);
    }
    match last_meta {
        Some(meta) => Ok(meta),
        None => Ok(std::fs::symlink_metadata(&current)?),
    }
}

fn reject_symlinks_metadata_cached(
    root: &Path,
    path: &Path,
    checked_dirs: &mut HashSet<PathBuf>,
) -> Result<std::fs::Metadata> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        for ancestor in parent.ancestors() {
            if ancestor.as_os_str().is_empty() || checked_dirs.contains(ancestor) {
                break;
            }
            let meta = std::fs::symlink_metadata(root.join(ancestor))?;
            if meta.file_type().is_symlink() {
                return Err(invalid("symlinks are unsupported in this local experiment").into());
            }
            checked_dirs.insert(ancestor.to_path_buf());
        }
    }
    let meta = std::fs::symlink_metadata(root.join(path))?;
    if meta.file_type().is_symlink() {
        return Err(invalid("symlinks are unsupported in this local experiment").into());
    }
    Ok(meta)
}

pub fn capture(command: &mut Command) -> Result<Vec<u8>> {
    capture_with_timeout(command, COMMAND_TIMEOUT)
}

pub fn capture_with_timeout(command: &mut Command, timeout: Duration) -> Result<Vec<u8>> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(invalid("missing stdout").into());
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(invalid("missing stderr").into());
        }
    };
    let stdout_reader = match std::thread::Builder::new()
        .name("cctx-rg-stdout".into())
        .stack_size(IO_WORKER_STACK_BYTES)
        .spawn(move || {
            let mut bytes = Vec::new();
            stdout
                .take(MAX_CAPTURE as u64 + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        }) {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
    };
    let stderr_reader = match std::thread::Builder::new()
        .name("cctx-rg-stderr".into())
        .stack_size(IO_WORKER_STACK_BYTES)
        .spawn(move || {
            let mut bytes = Vec::new();
            stderr
                .take(64 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        }) {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            return Err(error.into());
        }
    };
    let started = Instant::now();
    let mut poll_error = None;
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status, false),
            Ok(None) => {}
            Err(error) => {
                poll_error = Some(error);
                let _ = child.kill();
                break (child.wait()?, false);
            }
        }
        if started.elapsed() >= timeout {
            if let Err(error) = child.kill()
                && child.try_wait()?.is_none()
            {
                return Err(error.into());
            }
            break (child.wait()?, true);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| invalid("stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| invalid("stderr reader panicked"))??;
    if let Some(error) = poll_error {
        return Err(error.into());
    }
    if timed_out {
        return Err(invalid(format!(
            "command timed out after {} milliseconds",
            timeout.as_millis()
        ))
        .into());
    }
    if stdout.len() > MAX_CAPTURE {
        return Err(invalid("command output exceeds 8 MiB; narrow the search").into());
    }
    if !status.success() && status.code() != Some(1) {
        return Err(invalid(format!(
            "command failed ({status}): {}",
            String::from_utf8_lossy(&stderr)
        ))
        .into());
    }
    if !stderr.is_empty() {
        return Err(invalid(format!(
            "command reported an error: {}",
            String::from_utf8_lossy(&stderr)
        ))
        .into());
    }
    Ok(stdout)
}
