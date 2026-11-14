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
pub const MAX_DISCOVERY_CAPTURE: usize = 32 * 1024 * 1024;
pub const MAX_FILE: u64 = 2 * 1024 * 1024;
pub const MAX_RESPONSE: usize = 256 * 1024;
const MAX_SCOPES: usize = 32;
const MAX_GLOBS: usize = 160;
const MAX_SCOPE_BYTES: usize = 4096;
const MAX_GLOB_BYTES: usize = 1024;
const MAX_CONFIG_BYTES: usize = 64 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
mod candidate_policy_tests {
    use super::*;

    fn reference_with_compiled(
        workspace: &Workspace,
        include_matchers: &[globset::GlobMatcher],
        exclude_matchers: &[globset::GlobMatcher],
        name: &str,
    ) -> Result<bool> {
        let path = Path::new(name);
        safe_relative(path)?;
        let path = normalize(path);
        if !workspace
            .scopes
            .iter()
            .any(|scope| scope == "." || path.starts_with(scope))
        {
            return Ok(false);
        }
        let included = include_matchers.iter().any(|matcher| {
            matcher.is_match(&path) || path.file_name().is_some_and(|name| matcher.is_match(name))
        });
        if !included {
            return Ok(false);
        }
        for matcher in exclude_matchers {
            if path
                .ancestors()
                .filter(|p| !p.as_os_str().is_empty())
                .any(|ancestor| {
                    matcher.is_match(ancestor)
                        || ancestor
                            .file_name()
                            .is_some_and(|name| matcher.is_match(name))
                })
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    #[test]
    fn compiled_policy_matches_existing_scope_glob_exclusion_and_hidden_rules() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::new_with_excludes(
            root.path(),
            vec![".".into()],
            vec![
                "*.rs".into(),
                "*.ts".into(),
                "docs/**/*.md".into(),
                "Makefile".into(),
            ],
            vec!["**/private/**".into(), "**/manual.rs".into()],
            true,
        )
        .unwrap();
        let include_matchers: Vec<_> = workspace
            .globs
            .iter()
            .map(|p| Glob::new(p).unwrap().compile_matcher())
            .collect();
        let exclude_matchers: Vec<_> = workspace
            .effective_excludes()
            .into_iter()
            .map(|p| Glob::new(&p).unwrap().compile_matcher())
            .collect();
        let policy = CandidatePolicy::compile(workspace.candidate_policy_key()).unwrap();
        for directory in [
            "",
            "src/",
            "src/private/",
            "target/",
            "src/target/",
            "node_modules/",
            "docs/a/",
        ] {
            for file in [
                "lib.rs",
                "app.ts",
                "manual.rs",
                "a.md",
                "Makefile",
                "readme.txt",
            ] {
                let name = format!("{directory}{file}");
                assert_eq!(
                    policy.candidate_path(&name).unwrap(),
                    reference_with_compiled(
                        &workspace,
                        &include_matchers,
                        &exclude_matchers,
                        &name
                    )
                    .unwrap(),
                    "{name}"
                );
            }
        }
        for name in [".hidden.rs", ".github/a.rs", "../escape.rs", "/absolute.rs"] {
            assert!(policy.candidate_path(name).is_err(), "{name}");
        }
    }
}

pub struct FileInventory {
    pub files: HashSet<PathBuf>,
    pub(crate) stamps: std::collections::BTreeMap<String, crate::incremental::Stamp>,
    pub(crate) extra_files: std::collections::BTreeMap<String, crate::incremental::Stamp>,
    pub capped_count: usize,
    pub oversized_count: usize,
    pub total_path_bytes: usize,
    pub metadata_source_bytes: u64,
}

impl FileInventory {
    pub(crate) fn trim_to_source_budget(&mut self, max_source_bytes: u64) -> usize {
        if self.metadata_source_bytes <= max_source_bytes {
            return 0;
        }
        let mut removable: Vec<(u8, bool, u64, String)> = self
            .stamps
            .iter()
            .filter_map(|(name, stamp)| {
                let (tier, secondary, len) = inventory_priority_key(Path::new(name), stamp.length);
                (tier > 0).then(|| (tier, secondary, len, name.clone()))
            })
            .collect();
        // Drop lowest-priority (highest tier, secondary subtree, largest bytes) first.
        removable.sort_unstable_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| b.3.cmp(&a.3))
        });
        let mut trimmed = 0usize;
        for (_, _, _, name) in removable {
            if self.metadata_source_bytes <= max_source_bytes {
                break;
            }
            if let Some(stamp) = self.stamps.remove(&name) {
                self.files.remove(Path::new(&name));
                self.metadata_source_bytes =
                    self.metadata_source_bytes.saturating_sub(stamp.length);
                self.total_path_bytes = self.total_path_bytes.saturating_sub(name.len());
                self.extra_files.insert(name, stamp);
                self.capped_count = self.capped_count.saturating_add(1);
                trimmed += 1;
            }
        }
        trimmed
    }
}

pub(crate) fn inventory_priority_key(path: &Path, len: u64) -> (u8, bool, u64) {
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let tier = match ext.as_str() {
        "rs" | "py" | "pyi" | "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs"
        | "go" | "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "cs" | "java" | "kt"
        | "kts" | "scala" | "swift" | "rb" | "php" | "lua" | "zig" | "nim" | "dart" | "ex"
        | "exs" | "erl" | "hrl" | "hs" | "ml" | "mli" | "clj" | "cljs" | "r" | "jl" | "sh"
        | "bash" | "zsh" | "ps1" | "psm1" | "psd1" | "sql" | "proto" | "graphql" | "gql"
        | "graphqls" | "prisma" | "tf" | "tftpl" | "hcl" | "bicep" | "nix" | "cmake" | "mk"
        | "vue" | "svelte" | "astro" | "dog" | "wit" | "rego" | "cue" | "dhall" | "pkl"
        | "glsl" | "wgsl" | "hlsl" | "metal" => 0,
        "" if matches!(
            file_name,
            "Makefile"
                | "GNUmakefile"
                | "Dockerfile"
                | "Containerfile"
                | "Justfile"
                | "BUILD"
                | "WORKSPACE"
                | "Gemfile"
                | "Rakefile"
                | "Brewfile"
                | "Procfile"
                | "Vagrantfile"
                | "Tiltfile"
                | "Earthfile"
                | "Podfile"
        ) =>
        {
            0
        }
        "txt" if file_name == "CMakeLists.txt" => 0,
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "css" | "scss" | "less" | "sass" | "pcss"
        | "postcss" | "styl" | "hbs" | "ejs" | "md" | "mdx" | "markdown" | "mdown" | "mkd"
        | "qmd" | "rmd" | "rst" | "adoc" | "org" | "typ" | "tex" | "bib" | "mmd" | "mermaid"
        | "puml" | "plantuml" => 1,
        "json" | "jsonc"
            if matches!(
                file_name,
                "package.json"
                    | "tsconfig.json"
                    | "jsconfig.json"
                    | "deno.json"
                    | "deno.jsonc"
                    | "biome.json"
                    | "composer.json"
                    | "devcontainer.json"
                    | "context7.json"
            ) =>
        {
            1
        }
        _ => 2,
    };
    let secondary = path.components().any(|comp| {
        comp.as_os_str().to_str().is_some_and(|seg| {
            matches!(
                seg,
                "archive"
                    | "features_change"
                    | "collected"
                    | "fixtures"
                    | "testdata"
                    | "benchmarks"
            )
        })
    });
    (tier, secondary, len)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CandidatePolicyKey {
    pub(crate) scopes: Vec<String>,
    pub(crate) globs: Vec<String>,
    pub(crate) excludes: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct CandidatePolicy {
    pub(crate) key: CandidatePolicyKey,
    includes: GlobSet,
    excludes: GlobSet,
}

fn companion_globs(pattern: &str) -> &'static [&'static str] {
    match pattern {
        "Dockerfile" => &["*.Dockerfile", "*.dockerfile", "Dockerfile.*"],
        "Containerfile" => &["*.Containerfile", "*.containerfile", "Containerfile.*"],
        "Makefile" => &["Caddyfile", "Jenkinsfile", "Rakefile", "Gemfile"],
        "*.cfg" => &["*.service", "*.socket", "*.timer", "*.conf"],
        "*.h" => &["*.H", "*.hxx"],
        "*.clj" => &["*.cljc"],
        "*.ps1" => &["*.psd1"],
        _ => &[],
    }
}

impl CandidatePolicy {
    pub(crate) fn compile(key: CandidatePolicyKey) -> Result<Self> {
        let mut includes = GlobSetBuilder::new();
        for pattern in &key.globs {
            includes.add(Glob::new(pattern)?);
            for extra in companion_globs(pattern) {
                includes.add(Glob::new(extra)?);
            }
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
                invalid("at most 32 scopes and 160 include/exclude globs are allowed").into(),
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
        command
            .current_dir(&self.root)
            .args(["--no-config", "--threads", "2"]);
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
            for extra in companion_globs(glob) {
                command.arg("--type-add").arg(format!("cctx:{extra}"));
            }
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
                if self.root.join(&directory).is_dir() {
                    insert(&directory)?;
                }
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
        self.files_limited_inner(policy, false)
    }

    pub fn files_limited_skip_oversized(
        &self,
        policy: &MemoryPolicy,
    ) -> Result<FileInventory> {
        self.files_limited_inner(policy, true)
    }

    fn files_limited_inner(
        &self,
        policy: &MemoryPolicy,
        skip_oversized: bool,
    ) -> Result<FileInventory> {
        for scope in &self.scopes {
            let rel = if scope == "." {
                Path::new("")
            } else {
                Path::new(scope)
            };
            if !self.root.join(rel).is_dir() {
                return Err(invalid("scopes must be directories inside root").into());
            }
        }
        self.files_limited_at_inner(policy, &self.scopes, false, skip_oversized)
    }

    pub(crate) fn files_limited_at_skip_oversized(
        &self,
        policy: &MemoryPolicy,
        targets: &[String],
        shallow: bool,
    ) -> Result<FileInventory> {
        self.files_limited_at_inner(policy, targets, shallow, true)
    }

    fn files_limited_at_inner(
        &self,
        policy: &MemoryPolicy,
        targets: &[String],
        shallow: bool,
        skip_oversized: bool,
    ) -> Result<FileInventory> {
        policy.validate().map_err(invalid)?;
        let mut active_targets = Vec::with_capacity(targets.len());
        for target in targets {
            let rel = Path::new(target);
            if !rel.as_os_str().is_empty() && target != "." {
                match reject_symlinks_metadata(&self.root, rel) {
                    Ok(meta) if meta.is_dir() => active_targets.push(target.as_str()),
                    Ok(_) => {}
                    Err(error)
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|io_err| io_err.kind() == std::io::ErrorKind::NotFound) => {}
                    Err(error) => return Err(error),
                }
            } else if self.root.is_dir() {
                active_targets.push(target.as_str());
            }
        }
        if active_targets.is_empty() {
            return Ok(FileInventory {
                files: HashSet::new(),
                stamps: std::collections::BTreeMap::new(),
                extra_files: std::collections::BTreeMap::new(),
                capped_count: 0,
                oversized_count: 0,
                total_path_bytes: 0,
                metadata_source_bytes: 0,
            });
        }
        let mut command = self.command();
        if shallow {
            command.args(["--max-depth", "1", "--threads", "1"]);
        }
        command.args(["--files", "--null", "--"]).args(&active_targets);
        let output = if skip_oversized {
            capture_discovery(&mut command)?
        } else {
            capture(&mut command)?
        };
        let estimated_count = bytecount_zero(&output).min(policy.max_files);
        let mut paths = HashSet::with_capacity(estimated_count);
        let mut stamps = std::collections::BTreeMap::new();
        let mut extra_files = std::collections::BTreeMap::new();
        let mut checked_dirs: HashSet<PathBuf> = HashSet::with_capacity((estimated_count / 4).max(16));
        let mut total_path_bytes = 0usize;
        let mut metadata_source_bytes = 0u64;
        let mut oversized_count = 0usize;
        if !skip_oversized {
            for path in output.split(|b| *b == 0).filter(|p| !p.is_empty()) {
                let text = std::str::from_utf8(path)?;
                let path = Path::new(text);
                safe_relative(path)?;
                let metadata =
                    match reject_symlinks_metadata_cached(&self.root, path, &mut checked_dirs) {
                        Ok(metadata) => metadata,
                        Err(error)
                            if error
                                .downcast_ref::<std::io::Error>()
                                .is_some_and(|io_err| {
                                    io_err.kind() == std::io::ErrorKind::NotFound
                                }) =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
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
            return Ok(FileInventory {
                files: paths,
                stamps,
                extra_files,
                capped_count: 0,
                oversized_count: 0,
                total_path_bytes,
                metadata_source_bytes,
            });
        }

        let mut seen_names: HashSet<String> = HashSet::with_capacity(estimated_count);
        let mut candidates: Vec<(PathBuf, String, crate::incremental::Stamp, usize)> =
            Vec::with_capacity(estimated_count);
        let mut extra_path_bytes = 0usize;
        for path in output.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            let Ok(text) = std::str::from_utf8(path) else {
                continue;
            };
            let path = Path::new(text);
            if safe_relative(path).is_err() {
                continue;
            }
            let metadata = match reject_symlinks_metadata_cached(&self.root, path, &mut checked_dirs)
            {
                Ok(metadata) => metadata,
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io_err| io_err.kind() == std::io::ErrorKind::NotFound)
                        || error
                            .to_string()
                            .contains("symlinks are unsupported in this local experiment") =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            let normalized = normalize(path);
            let Some(normalized_str) = normalized.to_str() else {
                continue;
            };
            let normalized_name = normalized_str.to_owned();
            if !seen_names.insert(normalized_name.clone()) {
                continue;
            }
            let stamp = crate::incremental::stamp_from_metadata(&metadata)?;
            let raw_path_len = path.as_os_str().len();
            if metadata.len() > policy.max_file_bytes as u64 {
                oversized_count = oversized_count.saturating_add(1);
                if extra_files.len() < policy.max_files
                    && extra_path_bytes.saturating_add(raw_path_len) <= policy.max_total_path_bytes
                {
                    extra_path_bytes = extra_path_bytes.saturating_add(raw_path_len);
                    extra_files.insert(normalized_name, stamp);
                }
                continue;
            }
            total_path_bytes = total_path_bytes.saturating_add(raw_path_len);
            metadata_source_bytes = metadata_source_bytes.saturating_add(metadata.len());
            candidates.push((normalized, normalized_name, stamp, raw_path_len));
        }

        if candidates.len() <= policy.max_files
            && total_path_bytes <= policy.max_total_path_bytes
            && metadata_source_bytes <= policy.max_source_bytes as u64
        {
            for (normalized, normalized_name, stamp, _) in candidates {
                paths.insert(normalized);
                stamps.insert(normalized_name, stamp);
            }
            return Ok(FileInventory {
                files: paths,
                stamps,
                extra_files,
                capped_count: 0,
                oversized_count,
                total_path_bytes,
                metadata_source_bytes,
            });
        }

        // Graceful 3-tier priority capping when aggregate inventory exceeds static policy limits.
        candidates.sort_unstable_by(|a, b| {
            inventory_priority_key(&a.0, a.2.length)
                .cmp(&inventory_priority_key(&b.0, b.2.length))
                .then_with(|| a.1.cmp(&b.1))
        });
        total_path_bytes = 0;
        metadata_source_bytes = 0;
        let mut capped_count = 0usize;
        for (normalized, normalized_name, stamp, raw_path_len) in candidates {
            if paths.len() < policy.max_files
                && total_path_bytes.saturating_add(raw_path_len) <= policy.max_total_path_bytes
                && metadata_source_bytes.saturating_add(stamp.length)
                    <= policy.max_source_bytes as u64
            {
                total_path_bytes = total_path_bytes.saturating_add(raw_path_len);
                metadata_source_bytes = metadata_source_bytes.saturating_add(stamp.length);
                paths.insert(normalized);
                stamps.insert(normalized_name, stamp);
            } else {
                capped_count = capped_count.saturating_add(1);
                if extra_files.len() < policy.max_files
                    && extra_path_bytes.saturating_add(raw_path_len) <= policy.max_total_path_bytes
                {
                    extra_path_bytes = extra_path_bytes.saturating_add(raw_path_len);
                    extra_files.insert(normalized_name, stamp);
                }
            }
        }
        Ok(FileInventory {
            files: paths,
            stamps,
            extra_files,
            capped_count,
            oversized_count,
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
            Ok(meta) if meta.len() > policy.max_file_bytes as u64 => {
                return Ok("exceeds_max_file_bytes");
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("missing_on_disk"),
            Err(error) => return Err(error.into()),
        }
        reject_symlinks(&self.root, &relative)?;
        let parent = relative.parent().unwrap_or(Path::new(""));
        let target = if parent.as_os_str().is_empty() { ".".to_owned() }
            else { parent.to_string_lossy().into_owned() };
        let inventory = self.files_limited_at_skip_oversized(policy, &[target], true)?;
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
        self.read_verified_file_bytes(path, &metadata, max_file_bytes)
    }

    pub(crate) fn check_parent_symlinks_cached(
        &self,
        path: &Path,
        checked_dirs: &mut HashSet<PathBuf>,
    ) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            for ancestor in parent.ancestors() {
                if ancestor.as_os_str().is_empty() || checked_dirs.contains(ancestor) {
                    break;
                }
                let meta = std::fs::symlink_metadata(self.root.join(ancestor))?;
                if meta.file_type().is_symlink() {
                    return Err(invalid("symlinks are unsupported in this local experiment").into());
                }
                checked_dirs.insert(ancestor.to_path_buf());
            }
        }
        Ok(())
    }

    pub(crate) fn read_known_nofollow_after_dir_check(
        &self,
        path: &Path,
        max_file_bytes: usize,
    ) -> Result<Vec<u8>> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let absolute = self.root.join(path);
            let file = match std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&absolute)
            {
                Ok(file) => file,
                Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                    return Err(invalid("symlinks are unsupported in this local experiment").into());
                }
                Err(error) => return Err(error.into()),
            };
            let metadata = file.metadata()?;
            if metadata.file_type().is_symlink() {
                return Err(invalid("symlinks are unsupported in this local experiment").into());
            }
            if metadata.len() > max_file_bytes as u64 {
                return Err(invalid(format!(
                    "file larger than configured max_file_bytes is unsupported: {}",
                    path.display()
                ))
                .into());
            }
            let capacity = (metadata.len() as usize).min(max_file_bytes).saturating_add(1);
            let mut bytes = Vec::with_capacity(capacity);
            file.take(max_file_bytes as u64 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > max_file_bytes {
                return Err(invalid(format!(
                    "file larger than configured max_file_bytes is unsupported: {}",
                    path.display()
                ))
                .into());
            }
            if let Some(extracted) = crate::document::extract_document_bytes(path, &bytes)
                .map_err(|err| invalid(format!("document extraction failed for {}: {err}", path.display())))?
            {
                return Ok(extracted.into_bytes());
            }
            if memchr::memchr(0, &bytes).is_some() {
                return Err(invalid(format!(
                    "binary file or file larger than configured limit is unsupported: {}",
                    path.display()
                ))
                .into());
            }
            Ok(bytes)
        }
        #[cfg(not(unix))]
        {
            let metadata = std::fs::symlink_metadata(self.root.join(path))?;
            if metadata.file_type().is_symlink() {
                return Err(invalid("symlinks are unsupported in this local experiment").into());
            }
            self.read_verified_file_bytes(path, &metadata, max_file_bytes)
        }
    }

    pub(crate) fn read_known_with_limit_cached(
        &self,
        path: &Path,
        max_file_bytes: usize,
        checked_dirs: &mut HashSet<PathBuf>,
    ) -> Result<Vec<u8>> {
        safe_relative(path)?;
        self.check_parent_symlinks_cached(path, checked_dirs)?;
        self.read_known_nofollow_after_dir_check(path, max_file_bytes)
    }

    fn read_verified_file_bytes(
        &self,
        path: &Path,
        metadata: &std::fs::Metadata,
        max_file_bytes: usize,
    ) -> Result<Vec<u8>> {
        if metadata.len() > max_file_bytes as u64 {
            return Err(invalid(format!(
                "file larger than configured max_file_bytes is unsupported: {}",
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
        if bytes.len() > max_file_bytes {
            return Err(invalid(format!(
                "file larger than configured max_file_bytes is unsupported: {}",
                path.display()
            ))
            .into());
        }
        if let Some(extracted) = crate::document::extract_document_bytes(path, &bytes)
            .map_err(|err| invalid(format!("document extraction failed for {}: {err}", path.display())))?
        {
            return Ok(extracted.into_bytes());
        }
        if memchr::memchr(0, &bytes).is_some() {
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

#[inline]
fn bytecount_zero(bytes: &[u8]) -> usize {
    memchr::memchr_iter(0, bytes).count()
}

pub fn capture(command: &mut Command) -> Result<Vec<u8>> {
    capture_with_timeout(command, COMMAND_TIMEOUT)
}

pub(crate) fn capture_discovery(command: &mut Command) -> Result<Vec<u8>> {
    capture_inner(command, COMMAND_TIMEOUT, MAX_DISCOVERY_CAPTURE, true)
}

pub fn capture_with_timeout(command: &mut Command, timeout: Duration) -> Result<Vec<u8>> {
    capture_inner(command, timeout, MAX_CAPTURE, false)
}

fn is_transient_missing_stderr(stderr: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(stderr) else {
        return false;
    };
    let mut saw_line = false;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        saw_line = true;
        let missing = line.contains("No such file or directory")
            || line.contains("The system cannot find the");
        if !missing {
            return false;
        }
    }
    saw_line
}

fn capture_inner(
    command: &mut Command,
    timeout: Duration,
    max_capture: usize,
    ignore_transient_missing: bool,
) -> Result<Vec<u8>> {
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
            let mut bytes = Vec::with_capacity(8192);
            stdout
                .take(max_capture as u64 + 1)
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
    let mut sleep_us = 200_u64;
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
        std::thread::sleep(Duration::from_micros(sleep_us));
        sleep_us = (sleep_us * 2).min(5_000);
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
    if stdout.len() > max_capture {
        return Err(invalid(format!(
            "command output exceeds {} MiB; narrow the search",
            max_capture / (1024 * 1024)
        ))
        .into());
    }
    if ignore_transient_missing
        && status.code() == Some(2)
        && is_transient_missing_stderr(&stderr)
    {
        return Ok(stdout);
    }
    if !status.success() && status.code() != Some(1) {
        return Err(invalid(format!(
            "command failed ({status}): {}",
            String::from_utf8_lossy(&stderr)
        ))
        .into());
    }
    if !stderr.is_empty() && (!ignore_transient_missing || !is_transient_missing_stderr(&stderr)) {
        return Err(invalid(format!(
            "command reported an error: {}",
            String::from_utf8_lossy(&stderr)
        ))
        .into());
    }
    Ok(stdout)
}

#[cfg(test)]
mod transient_discovery_tests {
    use super::is_transient_missing_stderr;

    #[test]
    fn transient_missing_stderr_filters_only_vanished_paths() {
        assert!(is_transient_missing_stderr(
            b"data/autonomy/rsi_staging/rustcyV0GMn: No such file or directory (os error 2)\n"
        ));
        assert!(!is_transient_missing_stderr(b""));
        assert!(!is_transient_missing_stderr(
            b"rg: regex parse error: unclosed group\n"
        ));
        assert!(!is_transient_missing_stderr(
            b"data/a: No such file or directory (os error 2)\nsecret: Permission denied (os error 13)\n"
        ));
    }
}
