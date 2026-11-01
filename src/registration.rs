use crate::catalog::{self, Spec};
use crate::workspace::capture_with_timeout;
use crate::{Result, invalid};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const TEXT_GLOBS: &[&str] = &[
    "*.md",
    "*.mdx",
    "*.markdown",
    "*.mdown",
    "*.mkd",
    "*.qmd",
    "*.rmd",
    "*.rst",
    "*.adoc",
    "*.org",
    "*.typ",
    "*.tex",
    "*.bib",
    "*.txt",
    "*.csv",
    "*.tsv",
    "*.jsonl",
    "*.ndjson",
    "*.jsonlines",
    "*.docx",
    "*.pptx",
    "*.xlsx",
    "*.odt",
    "*.odp",
    "*.ods",
    "*.epub",
    "*.pdf",
    "*.rtf",
    "*.ipynb",
    "*.vtt",
    "*.srt",
    "*.xlf",
    "*.xliff",
    "*.json",
    "*.jsonc",
    "*.json5",
    "*.yaml",
    "*.yml",
    "*.toml",
    "*.ini",
    "*.cfg",
    "*.xml",
    "*.html",
    "*.svg",
    "*.css",
    "*.scss",
    "*.less",
    "*.sass",
    "*.pcss",
    "*.postcss",
    "*.styl",
    "*.vue",
    "*.svelte",
    "*.astro",
    "*.hbs",
    "*.ejs",
    "*.glsl",
    "*.wgsl",
    "*.hlsl",
    "*.metal",
    "Makefile",
    "GNUmakefile",
    "Dockerfile",
    "Containerfile",
    "CMakeLists.txt",
    "Justfile",
    "Vagrantfile",
    "Procfile",
    "Brewfile",
    "Gemfile",
    "Rakefile",
    "Podfile",
    "Earthfile",
    "Tiltfile",
    "Taskfile.yml",
    "compose.yaml",
    "compose.yml",
    "docker-compose.yml",
    "docker-compose.yaml",
    "BUILD",
    "WORKSPACE",
    "*.sh",
    "*.bash",
    "*.zsh",
    "*.ps1",
    "*.psm1",
    "*.psd1",
    "*.tf",
    "*.tftpl",
    "*.hcl",
    "*.bicep",
    "*.sql",
    "*.proto",
    "*.graphql",
    "*.gql",
    "*.prisma",
    "*.cmake",
    "*.mk",
    "*.nix",
    "*.cue",
    "*.dhall",
    "*.pkl",
    "*.rego",
    "*.wit",
    "*.fbs",
    "*.capnp",
    "*.smithy",
    "*.avsc",
    "*.graphqls",
    "*.mmd",
    "*.mermaid",
    "*.puml",
    "*.plantuml",
];

pub const DEFAULT_SOURCE_GLOBS: &[&str] = &[
    "*.C",
    "*.R",
    "*.adoc",
    "*.astro",
    "*.bash",
    "*.bib",
    "*.bicep",
    "*.c",
    "*.cc",
    "*.cfg",
    "*.cjs",
    "*.clj",
    "*.cljs",
    "*.cmake",
    "*.cpp",
    "*.cs",
    "*.css",
    "*.csv",
    "*.cts",
    "*.cxx",
    "*.dart",
    "*.docx",
    "*.ejs",
    "*.epub",
    "*.erl",
    "*.ex",
    "*.exs",
    "*.fs",
    "*.gleam",
    "*.glsl",
    "*.go",
    "*.gql",
    "*.gradle",
    "*.graphql",
    "*.groovy",
    "*.h",
    "*.hbs",
    "*.hcl",
    "*.hh",
    "*.hpp",
    "*.hrl",
    "*.hs",
    "*.html",
    "*.ini",
    "*.ipynb",
    "*.java",
    "*.jl",
    "*.js",
    "*.json",
    "*.jsonc",
    "*.jsonl",
    "*.jsx",
    "*.kt",
    "*.kts",
    "*.less",
    "*.lua",
    "*.markdown",
    "*.md",
    "*.mdx",
    "*.mjs",
    "*.mk",
    "*.ml",
    "*.mli",
    "*.mts",
    "*.nim",
    "*.nix",
    "*.odp",
    "*.ods",
    "*.odt",
    "*.org",
    "*.pcss",
    "*.pdf",
    "*.php",
    "*.postcss",
    "*.pptx",
    "*.prisma",
    "*.proto",
    "*.ps1",
    "*.psm1",
    "*.py",
    "*.pyi",
    "*.qmd",
    "*.r",
    "*.rb",
    "*.rs",
    "*.rst",
    "*.rtf",
    "*.sass",
    "*.scala",
    "*.scss",
    "*.sh",
    "*.sql",
    "*.srt",
    "*.styl",
    "*.svelte",
    "*.svg",
    "*.swift",
    "*.tex",
    "*.tf",
    "*.tftpl",
    "*.toml",
    "*.ts",
    "*.tsv",
    "*.tsx",
    "*.txt",
    "*.typ",
    "*.vb",
    "*.vtt",
    "*.vue",
    "*.wgsl",
    "*.xliff",
    "*.xlsx",
    "*.xml",
    "*.yaml",
    "*.yml",
    "*.zig",
    "*.zsh",
    "BUILD",
    "Brewfile",
    "CMakeLists.txt",
    "Containerfile",
    "Dockerfile",
    "GNUmakefile",
    "Justfile",
    "Makefile",
    "Procfile",
    "Vagrantfile",
    "WORKSPACE",
];

pub fn plan(spec: &Spec) -> Result<Value> {
    let workspace = spec.workspace()?;
    let inventory = workspace.files_limited(&spec.policy)?;
    let watches = workspace
        .watch_directories_for(inventory.files.iter().map(PathBuf::as_path), &spec.policy)?;
    Ok(json!({
        "root":workspace.root, "scopes":workspace.scopes, "globs":workspace.globs,
        "excludes":workspace.excludes, "default_excludes":workspace.default_excludes,
        "prewarm":spec.prewarm, "policy":spec.policy, "allow_worktrees":spec.allow_worktrees,
        "managed_parent":spec.managed_parent, "files":inventory.files.len(),
        "metadata_source_bytes":inventory.metadata_source_bytes,
        "path_bytes":inventory.total_path_bytes, "watch_directories":watches.len(),
        "detected_profiles":workspace.exclusion_summary().profiles,
    }))
}

pub fn migrate(
    root: &Path,
    add_scopes: &[String],
    add_excludes: &[String],
    apply: bool,
) -> Result<Value> {
    let root = catalog::canonicalize(root)?;
    let before = catalog::load()?
        .into_iter()
        .find(|s| s.root == root)
        .ok_or_else(|| invalid("migration requires an existing exact registered root"))?;
    if before.managed_parent.is_some() {
        return Err(invalid("migrate the authorized parent, not a managed worktree").into());
    }
    let mut after = before.clone();
    after.scopes.extend_from_slice(add_scopes);
    after.excludes.extend_from_slice(add_excludes);
    after.excludes.sort();
    after.excludes.dedup();
    after.globs.extend(DEFAULT_SOURCE_GLOBS.iter().map(|s| (*s).into()));
    after.globs.sort();
    after.globs.dedup();
    let workspace = after.workspace()?;
    after.scopes = workspace.scopes;
    let result = json!({"applied":apply,"before":plan(&before)?,"after":plan(&after)?,
        "source_verified":apply,
        "scope_change_explicit":!add_scopes.is_empty(),"explicit_added_excludes":add_excludes});
    if apply {
        validate_source(&after)?;
        catalog::mutate(|specs| {
            let entry = specs
                .iter_mut()
                .find(|s| s.root == root)
                .ok_or_else(|| invalid("registration was removed during migration"))?;
            if entry != &before {
                return Err(
                    invalid("registration changed during migration; review a new plan").into(),
                );
            }
            *entry = after;
            Ok(())
        })?;
    }
    Ok(result)
}

fn validate_source(spec: &Spec) -> Result<()> {
    let workspace = spec.workspace()?;
    let inventory = workspace.files_limited(&spec.policy)?;
    let mut source_bytes = 0usize;
    let mut lines = 0usize;
    let mut errors = Vec::new();
    for path in &inventory.files {
        let bytes = match workspace.read_known_with_limit(path, spec.policy.max_file_bytes) {
            Ok(bytes) => bytes,
            Err(error) => {
                errors.push(format!("{}: {error}", path.display()));
                if errors.len() >= 16 {
                    break;
                }
                continue;
            }
        };
        if let Err(error) = std::str::from_utf8(&bytes) {
            errors.push(format!(
                "non-UTF8 source is unsupported: {}: {error}",
                path.display()
            ));
            if errors.len() >= 16 {
                break;
            }
            continue;
        }
        source_bytes = source_bytes.saturating_add(bytes.len());
        lines = lines.saturating_add(
            bytes.iter().filter(|byte| **byte == b'\n').count()
                + usize::from(!bytes.is_empty() && bytes.last() != Some(&b'\n')),
        );
        if source_bytes > spec.policy.max_source_bytes || lines > spec.policy.max_total_lines {
            return Err(
                invalid("live source bytes or lines exceed configured migration budget").into(),
            );
        }
    }
    if !errors.is_empty() {
        return Err(invalid(format!(
            "migration source validation failed (up to 16 errors, registration unchanged):\n{}",
            errors.join("\n")
        ))
        .into());
    }
    Ok(())
}

pub fn authorize(root: &Path, allow: bool) -> Result<Value> {
    let root = catalog::canonicalize(root)?;
    // This is a local CLI-only trust decision, never an MCP argument.
    if allow {
        worktrees(&root)?;
    }
    catalog::mutate(|specs| {
        let spec = specs
            .iter_mut()
            .find(|s| s.root == root)
            .ok_or_else(|| invalid("authorize an exact registered repository first"))?;
        if spec.managed_parent.is_some() {
            return Err(invalid("managed worktrees cannot authorize other roots").into());
        }
        spec.allow_worktrees = allow;
        Ok(())
    })?;
    Ok(json!({"root":root,"allow_worktrees":allow}))
}

fn worktrees(parent: &Path) -> Result<Vec<PathBuf>> {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.quotePath=false", "-C"])
        .arg(parent)
        .args(["worktree", "list", "--porcelain"]);
    let bytes = capture_with_timeout(&mut command, Duration::from_secs(10))?;
    let output = std::str::from_utf8(&bytes)?;
    let mut roots = Vec::new();
    for line in output.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            let path = PathBuf::from(path);
            if !path.is_absolute() || path.to_string_lossy().contains(['\r', '\n', '"']) {
                return Err(invalid("unsupported Git worktree path encoding").into());
            }

            roots.push(path);
        }
    }
    if roots.is_empty() {
        return Err(invalid("Git returned no worktrees for the trusted repository").into());
    }
    Ok(roots)
}

pub(crate) fn preparation_candidates(specs: &[Spec]) -> Value {
    let mut roots = Vec::new();
    let mut errors = Vec::new();
    let mut truncated = false;
    for parent in specs
        .iter()
        .filter(|s| s.allow_worktrees && s.managed_parent.is_none())
    {
        match worktrees(&parent.root) {
            Ok(listed) => {
                for root in listed {
                    if specs.iter().any(|s| s.root == root) {
                        continue;
                    }
                    if roots.len() >= 128 {
                        truncated = true;
                        break;
                    }
                    roots.push(json!({"root":root,"parent":parent.root,
                        "state":"Git-listed; preparation/admission and canonical-path verification still required"}));
                }
            }
            Err(error) => errors.push(json!({"parent":parent.root,"error":error.to_string()})),
        }
    }
    json!({"roots":roots,"errors":errors,"truncated":truncated})
}

fn verify(parent: &Spec, target: &Path) -> Result<PathBuf> {
    fn git_paths(directory: &Path, options: &[&str]) -> Result<Vec<PathBuf>> {
        let mut command = Command::new("git");
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_NAMESPACE",
        ] {
            command.env_remove(key);
        }
        command
            .args(["-c", "core.quotePath=false", "-C"])
            .arg(directory)
            .args(["rev-parse", "--path-format=absolute"])
            .args(options);
        let bytes = capture_with_timeout(&mut command, Duration::from_secs(10))?;
        let paths: Vec<_> = std::str::from_utf8(&bytes)?
            .lines()
            .map(PathBuf::from)
            .collect();
        if paths.len() != options.len()
            || paths.iter().any(|path| {
                !path.is_absolute() || path.to_string_lossy().contains(['\r', '\n', '"'])
            })
        {
            return Err(invalid("unsupported Git worktree path encoding").into());
        }
        Ok(paths)
    }
    let parent_paths = git_paths(&parent.root, &["--git-common-dir"])?;
    let target_paths = git_paths(
        target,
        &["--show-toplevel", "--git-common-dir", "--absolute-git-dir"],
    )?;
    let root = target_paths[0].clone();
    if catalog::canonicalize(&target_paths[1])? != catalog::canonicalize(&parent_paths[0])?
        || !target.starts_with(&root)
    {
        return Err(invalid("target is not a Git worktree of the authorized parent").into());
    }
    if catalog::canonicalize(&root)? != root {
        return Err(invalid("symlinked worktree roots are not authorized").into());
    }
    let git_dir = catalog::canonicalize(&target_paths[2])?;
    let common = catalog::canonicalize(&parent_paths[0])?;
    if git_dir != common {
        if git_dir.parent() != Some(common.join("worktrees").as_path()) {
            return Err(
                invalid("worktree Git directory is outside the authorized repository").into(),
            );
        }
        let backlink = git_dir.join("gitdir");
        let metadata = std::fs::symlink_metadata(&backlink)?;
        if !metadata.is_file() || metadata.len() > 4096 {
            return Err(invalid("invalid worktree Git backlink").into());
        }
        let linked =
            PathBuf::from(std::fs::read_to_string(backlink)?.trim_end_matches(['\r', '\n']));
        if catalog::canonicalize(&linked)? != catalog::canonicalize(&root.join(".git"))? {
            return Err(invalid("worktree Git backlink does not match the requested root").into());
        }
    }
    Ok(catalog::canonicalize(&root)?)
}

pub fn prepare_root(target: &Path) -> Result<PathBuf> {
    let target = catalog::canonicalize(target)?;
    if let Ok(specs) = catalog::load()
        && let Some(existing) = specs
            .iter()
            .filter(|s| target.starts_with(&s.root))
            .max_by_key(|s| s.root.components().count())
        && existing.managed_parent.is_none()
        && (target == existing.root || !existing.allow_worktrees)
    {
        return Ok(existing.root.clone());
    }
    let mut prepared = None;
    catalog::mutate(|specs| {
        if let Some(existing) = specs
            .iter()
            .filter(|s| target.starts_with(&s.root))
            .max_by_key(|s| s.root.components().count())
            .cloned()
        {
            if existing.managed_parent.is_some() {
                let replacement = effective_spec(&existing, specs)?;
                if replacement != existing {
                    plan(&replacement)?;
                    let entry = specs
                        .iter_mut()
                        .find(|s| s.root == existing.root)
                        .ok_or_else(|| invalid("managed registration disappeared"))?;
                    *entry = replacement;
                }
            }
            if existing.managed_parent.is_none()
                && existing.allow_worktrees
                && target != existing.root
            {
                let root = verify(&existing, &target)?;
                if root != existing.root {
                    prepared = Some(admit(specs, inherited(&existing, &root))?);
                    return Ok(());
                }
            }
            prepared = Some(existing.root);
            return Ok(());
        }
        let mut verification_errors = Vec::new();
        for parent in specs
            .iter()
            .filter(|s| s.allow_worktrees && s.managed_parent.is_none())
        {
            match verify(parent, &target) {
                Ok(root) => {
                    let spec = inherited(parent, &root);
                    prepared = Some(admit(specs, spec)?);
                    return Ok(());
                }
                Err(error) => verification_errors.push(error.to_string()),
            }
        }
        Err(invalid(format!(
            "root is not explicitly registered or authorized as a worktree; use local cctx authorize-worktrees on its registered parent, or register this root. {}",
            verification_errors.join("; ")
        )).into())
    })?;
    prepared.ok_or_else(|| invalid("worktree preparation did not resolve a root").into())
}

pub(crate) fn effective_spec(spec: &Spec, specs: &[Spec]) -> Result<Spec> {
    let policy = registered_policy(spec, specs)?;
    if let Some(parent_root) = &spec.managed_parent {
        let parent = specs
            .iter()
            .find(|s| &s.root == parent_root && s.allow_worktrees)
            .ok_or_else(|| {
                invalid("worktree authorization was revoked; explicitly reauthorize its parent")
            })?;
        if verify(parent, &spec.root)? != spec.root {
            return Err(
                invalid("registered worktree no longer belongs to its authorized parent").into(),
            );
        }
    }
    Ok(policy)
}

pub(crate) fn registered_policy(spec: &Spec, specs: &[Spec]) -> Result<Spec> {
    match &spec.managed_parent {
        Some(parent_root) => {
            let parent = specs
                .iter()
                .find(|parent| &parent.root == parent_root && parent.allow_worktrees)
                .ok_or_else(|| {
                    invalid("worktree authorization was revoked; explicitly reauthorize its parent")
                })?;
            Ok(inherited(parent, &spec.root))
        }
        None => Ok(spec.clone()),
    }
}

fn admit(specs: &mut Vec<Spec>, spec: Spec) -> Result<PathBuf> {
    catalog::ensure_capacity(specs)?;
    plan(&spec)?;
    let root = spec.root.clone();
    specs.push(spec);
    specs.sort_by(|a, b| a.root.cmp(&b.root));
    Ok(root)
}

fn inherited(parent: &Spec, root: &Path) -> Spec {
    Spec {
        root: root.into(),
        prewarm: false,
        allow_worktrees: false,
        managed_parent: Some(parent.root.clone()),
        ..parent.clone()
    }
}

pub fn prune(apply: bool) -> Result<Value> {
    let mut removed = Vec::new();
    catalog::mutate(|specs| {
        removed = catalog::missing_managed_roots(specs)?;
        if apply {
            specs.retain(|s| !removed.contains(&s.root));
        }
        Ok(())
    })?;
    Ok(json!({"applied":apply,"missing_managed_roots":removed,
        "policy":"only nonexistent managed roots; existing, active and manual roots are retained"}))
}
