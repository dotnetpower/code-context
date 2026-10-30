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
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
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
            let mut bytes = Vec::new();
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
    load()?
        .into_iter()
        .filter(|s| path.starts_with(&s.root))
        .max_by_key(|s| s.root.components().count())
        .map(|s| s.root)
        .ok_or_else(|| {
            invalid("no registered workspace contains this directory; use cctx register").into()
        })
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
