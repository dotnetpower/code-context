use crate::policy::MemoryPolicy;
use crate::workspace::Workspace;
use crate::{Result, invalid};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const MAX_REGISTERED_ROOTS: usize = 256;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub root: PathBuf,
    pub scopes: Vec<String>,
    pub globs: Vec<String>,
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default = "default_true")]
    pub default_excludes: bool,
    #[serde(default)]
    pub prewarm: bool,
    #[serde(default)]
    pub policy: MemoryPolicy,
    #[serde(default)]
    pub allow_worktrees: bool,
    #[serde(default)]
    pub managed_parent: Option<PathBuf>,
}

fn default_true() -> bool {
    true
}

impl Spec {
    pub fn workspace(&self) -> Result<Workspace> {
        Workspace::new_with_excludes(
            &self.root,
            self.scopes.clone(),
            self.globs.clone(),
            self.excludes.clone(),
            self.default_excludes,
        )
    }
}

pub fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    #[cfg(windows)]
    {
        if let Some(text) = canonical.to_str()
            && let Some(stripped) = text.strip_prefix(r"\\?\")
            && stripped.as_bytes().get(1) == Some(&b':')
        {
            return Ok(PathBuf::from(stripped));
        }
    }
    Ok(canonical)
}

#[cfg(unix)]
pub fn private_dir(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                return Err(invalid(format!(
                    "directory must be owned, non-symlink and mode 0700: {}",
                    path.display()
                ))
                .into());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(windows)]
mod win_acl {
    use std::collections::HashSet;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::{Mutex, OnceLock};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_USER_CLASS: u32 = 1;

    #[repr(C)]
    struct SidAndAttributes {
        sid: *mut core::ffi::c_void,
        attributes: u32,
    }

    #[repr(C)]
    struct TokenUser {
        user: SidAndAttributes,
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(
            process_handle: *mut core::ffi::c_void,
            desired_access: u32,
            token_handle: *mut *mut core::ffi::c_void,
        ) -> i32;
        fn GetTokenInformation(
            token_handle: *mut core::ffi::c_void,
            token_information_class: u32,
            token_information: *mut core::ffi::c_void,
            token_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
        fn ConvertSidToStringSidW(
            sid: *mut core::ffi::c_void,
            string_sid: *mut *mut u16,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
        fn LocalFree(mem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    }

    fn current_user_sid() -> Option<&'static str> {
        static SID: OnceLock<Option<String>> = OnceLock::new();
        SID.get_or_init(|| unsafe {
            let mut token: *mut core::ffi::c_void = core::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let mut needed = 0u32;
            let _ = GetTokenInformation(token, TOKEN_USER_CLASS, core::ptr::null_mut(), 0, &mut needed);
            if needed == 0 || needed > 4096 {
                let _ = CloseHandle(token);
                return None;
            }
            let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
            if GetTokenInformation(
                token,
                TOKEN_USER_CLASS,
                buf.as_mut_ptr().cast::<core::ffi::c_void>(),
                needed,
                &mut needed,
            ) == 0
            {
                let _ = CloseHandle(token);
                return None;
            }
            let tu = &*(buf.as_ptr().cast::<TokenUser>());
            let mut wide_ptr: *mut u16 = core::ptr::null_mut();
            let ok = ConvertSidToStringSidW(tu.user.sid, &mut wide_ptr);
            let _ = CloseHandle(token);
            if ok == 0 || wide_ptr.is_null() {
                return None;
            }
            let mut len = 0usize;
            while len < 256 && *wide_ptr.add(len) != 0 {
                len += 1;
            }
            let slice = core::slice::from_raw_parts(wide_ptr, len);
            let sid_str = String::from_utf16(slice).ok();
            let _ = LocalFree(wide_ptr.cast::<core::ffi::c_void>());
            sid_str.filter(|s| s.starts_with("S-1-"))
        })
        .as_deref()
    }

    pub fn apply(path: &Path, is_dir: bool, force: bool) {
        static APPLIED_DIRS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
        if is_dir && !force {
            let cache = APPLIED_DIRS.get_or_init(|| Mutex::new(HashSet::new()));
            if let Ok(guard) = cache.lock()
                && guard.contains(path)
            {
                return;
            }
        }
        let Some(sid) = current_user_sid() else {
            return;
        };
        let user_ace = if is_dir {
            format!("*{sid}:(OI)(CI)F")
        } else {
            format!("*{sid}:F")
        };
        let sys_ace = if is_dir {
            "*S-1-5-18:(OI)(CI)F"
        } else {
            "*S-1-5-18:F"
        };
        let status = Command::new("icacls.exe")
            .arg(path)
            .args(["/inheritance:r", "/grant:r", &user_ace, sys_ace])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if is_dir && status.is_ok_and(|s| s.success()) {
            let cache = APPLIED_DIRS.get_or_init(|| Mutex::new(HashSet::new()));
            if let Ok(mut guard) = cache.lock() {
                guard.insert(path.to_path_buf());
            }
        }
    }
}

#[cfg(windows)]
pub fn restrict_windows_file_acl(path: &Path) {
    win_acl::apply(path, false, true);
}

#[cfg(windows)]
pub fn private_dir(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(invalid(format!(
                    "directory must be owned, non-symlink and mode 0700: {}",
                    path.display()
                ))
                .into());
            }
            win_acl::apply(path, true, false);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
            win_acl::apply(path, true, true);
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub fn state_dir() -> Result<PathBuf> {
    let base = std::env::var_os("CCTX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            #[cfg(windows)]
            {
                std::env::var_os("LOCALAPPDATA")
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("USERPROFILE")
                            .map(|home| PathBuf::from(home).join("AppData").join("Local"))
                    })
                    .or_else(|| {
                        std::env::var_os("HOME")
                            .map(|home| PathBuf::from(home).join(".local").join("state"))
                    })
                    .unwrap_or_default()
                    .join("code-context")
            }
            #[cfg(not(windows))]
            {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                    .join(".local/state/code-context")
            }
        });
    if !base.is_absolute() {
        return Err(invalid("CCTX_HOME/HOME must resolve to an absolute path").into());
    }
    private_dir(&base)?;
    Ok(base)
}

pub fn socket_path() -> Result<PathBuf> {
    let path = state_dir()?.join("daemon.sock");
    #[cfg(unix)]
    if path.as_os_str().len() >= 104 {
        return Err(invalid("Unix socket path too long; use a shorter CCTX_HOME").into());
    }
    Ok(path)
}

pub fn load() -> Result<Vec<Spec>> {
    let path = state_dir()?.join("workspaces.json");
    match std::fs::File::open(&path) {
        Ok(file) => {
            let cap = file
                .metadata()
                .map(|m| (m.len() as usize).min(1024 * 1024 + 1))
                .unwrap_or(512);
            let mut bytes = Vec::with_capacity(cap);
            file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 1024 * 1024 {
                return Err(invalid("registry exceeds 1 MiB").into());
            }
            let specs: Vec<Spec> = serde_json::from_slice(&bytes)?;
            if specs.len() > MAX_REGISTERED_ROOTS {
                return Err(invalid(format!(
                    "at most {MAX_REGISTERED_ROOTS} roots can be registered"
                )).into());
            }
            Ok(specs)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub fn register(spec: Spec) -> Result<()> {
    let workspace = spec.workspace()?;
    let spec = Spec {
        root: workspace.root,
        scopes: workspace.scopes,
        globs: workspace.globs,
        excludes: workspace.excludes,
        default_excludes: workspace.default_excludes,
        prewarm: spec.prewarm,
        policy: spec.policy,
        allow_worktrees: spec.allow_worktrees,
        managed_parent: spec.managed_parent,
    };
    mutate(|specs| {
        specs.retain(|s| s.root != spec.root);
        ensure_capacity(specs)?;

        specs.push(spec);
        specs.sort_by(|a, b| a.root.cmp(&b.root));
        Ok(())
    })
}

pub fn register_if_absent(spec: Spec) -> Result<bool> {
    let workspace = spec.workspace()?;
    let spec = Spec {
        root: workspace.root,
        scopes: workspace.scopes,
        globs: workspace.globs,
        excludes: workspace.excludes,
        default_excludes: workspace.default_excludes,
        prewarm: spec.prewarm,
        policy: spec.policy,
        allow_worktrees: spec.allow_worktrees,
        managed_parent: spec.managed_parent,
    };
    let mut inserted = false;
    mutate(|specs| {
        if specs.iter().any(|s| s.root == spec.root) {
            inserted = false;
            return Ok(());
        }
        if specs
            .iter()
            .any(|s| spec.root.starts_with(&s.root) || s.root.starts_with(&spec.root))
        {
            return Err(invalid(
                "cannot automatically initialize an overlapping root beneath or above an existing registration",
            )
            .into());
        }
        ensure_capacity(specs)?;
        specs.push(spec);
        specs.sort_by(|a, b| a.root.cmp(&b.root));
        inserted = true;
        Ok(())
    })?;
    Ok(inserted)
}

pub(crate) fn missing_managed_roots(specs: &[Spec]) -> Result<Vec<PathBuf>> {
    let mut missing = Vec::new();
    for spec in specs.iter().filter(|spec| spec.managed_parent.is_some()) {
        match std::fs::symlink_metadata(&spec.root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(spec.root.clone());
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
    }
    Ok(missing)
}

pub(crate) fn ensure_capacity(specs: &mut Vec<Spec>) -> Result<()> {
    if specs.len() < MAX_REGISTERED_ROOTS {
        return Ok(());
    }
    let missing = missing_managed_roots(specs)?;
    specs.retain(|spec| !missing.contains(&spec.root));
    if specs.len() >= MAX_REGISTERED_ROOTS {
        return Err(invalid(format!(
            "{MAX_REGISTERED_ROOTS} registered roots reached; review cctx prune-worktrees; existing and manual roots are never evicted"
        )).into());
    }
    Ok(())
}

pub fn unregister(root: &Path) -> Result<()> {
    let root = match canonicalize(root) {
        Ok(root) => root,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && root.is_absolute()
                && !root
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)) =>
        {
            root.to_path_buf()
        }
        Err(error) => return Err(error.into()),
    };
    mutate(|specs| {
        let count = specs.len();
        specs.retain(|s| s.root != root);
        if count == specs.len() {
            return Err(invalid("root is not registered").into());
        }

        Ok(())
    })
}

pub fn resolve_root(path: &Path) -> Result<PathBuf> {
    let path = canonicalize(path)?;
    let specs = load()?;
    if let Some(root) = specs
        .iter()
        .filter(|s| path.starts_with(&s.root))
        .max_by_key(|s| s.root.components().count())
        .map(|s| s.root.clone())
    {
        return Ok(root);
    }
    let listed: Vec<String> = specs
        .iter()
        .take(5)
        .map(|s| s.root.display().to_string())
        .collect();
    let suffix = if listed.is_empty() {
        String::new()
    } else {
        format!(" (registered roots: {})", listed.join(", "))
    };
    Err(invalid(format!(
        "no registered workspace contains this directory; use cctx register{suffix}"
    ))
    .into())
}

pub(crate) fn mutate(change: impl FnOnce(&mut Vec<Spec>) -> Result<()>) -> Result<()> {
    let dir = state_dir()?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("registry.lock"))?;
    lock.lock_exclusive()?;
    let mut specs = load()?;
    let before = specs.clone();
    change(&mut specs)?;
    if specs == before {
        return Ok(());
    }
    let serialized = serde_json::to_vec_pretty(&specs)?;
    if serialized.len() > 1024 * 1024 {
        return Err(invalid("updated registry would exceed 1 MiB").into());
    }
    let mut temporary = tempfile::NamedTempFile::new_in(&dir)?;
    #[cfg(unix)]
    std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o600))?;
    std::io::Write::write_all(temporary.as_file_mut(), &serialized)?;
    temporary.as_file().sync_all()?;
    temporary.persist(dir.join("workspaces.json"))?;
    #[cfg(unix)]
    std::fs::File::open(&dir)?.sync_all()?;
    let removed_managed = before.iter().filter(|spec| {
        spec.managed_parent.is_some() && !specs.iter().any(|current| current.root == spec.root)
    }).count();
    if removed_managed > 0 {
        eprintln!("CCTX_REGISTRY_RECLAIM missing_managed_roots={removed_managed}");
    }
    Ok(())
}
