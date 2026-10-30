use clap::{Parser, Subcommand};
use code_context::context::{ContextRequest, ReadRange};
use code_context::engine::{Backend, Engine};
use code_context::policy::MemoryPolicy;
use code_context::search::Search;
use code_context::workspace::Workspace;
use code_context::{Result, catalog, daemon, experiment, mcp, resident_experiment, shared_mcp};
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Read-only Rust code context efficacy experiment")]
struct Args {
    #[arg(long, default_value = ".")]
    root: PathBuf,
    #[arg(long)]
    scope: Vec<String>,
    #[arg(long)]
    glob: Vec<String>,
    #[arg(long)]
    exclude: Vec<String>,
    #[arg(long)]
    no_default_excludes: bool,
    #[arg(long, value_enum, default_value_t = Backend::Shared)]
    backend: Backend,
    #[arg(
        long,
        help = "Legacy stored preference; shared cache no longer prewarms or pins roots"
    )]
    prewarm: bool,
    #[arg(long)]
    allow_root_scope: bool,
    #[arg(long)]
    preserve_registration_settings: bool,
    #[arg(long, default_value_t = 100_000)]
    max_files: usize,
    #[arg(long, default_value_t = 256)]
    max_source_mib: usize,
    #[arg(long, default_value_t = 1)]
    max_file_mib: usize,
    #[arg(long, default_value_t = 10_000_000)]
    max_total_lines: usize,
    #[arg(long, default_value_t = 16)]
    max_path_mib: usize,
    #[arg(long, default_value_t = 50_000)]
    max_watch_dirs: usize,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    Daemon,
    Ping,
    Register,
    Plan,
    Unregister,
    Workspaces,
    SharedMcp,
    NativeBridge,
    Sync,
    Status,
    Doctor,
    Coverage,
    Prepare,
    Migrate {
        #[arg(long)]
        add_scope: Vec<String>,
        #[arg(long)]
        add_exclude: Vec<String>,
        #[arg(long)]
        apply: bool,
    },
    AuthorizeWorktrees {
        #[arg(long)]
        revoke: bool,
    },
    PruneWorktrees {
        #[arg(long)]
        apply: bool,
    },
    Files {
        glob: String,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        #[arg(long)]
        path: Vec<String>,
    },
    Grep {
        pattern: String,
        #[arg(long)]
        regex: bool,
        #[arg(long)]
        ignore_case: bool,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        #[arg(long)]
        path: Vec<String>,
        #[arg(long)]
        filter: Option<String>,
    },
    Context {
        pattern: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = 3)]
        radius: usize,
        #[arg(long)]
        regex: bool,
        #[arg(long)]
        ignore_case: bool,
        #[arg(long)]
        path: Vec<String>,
        #[arg(long)]
        filter: Option<String>,
    },
    Read {
        path: String,
        start: usize,
        end: usize,
        #[arg(long)]
        page: bool,
    },
    Mcp,
    Bench {
        #[arg(long)]
        queries: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 30)]
        rounds: usize,
    },
    BenchResident {
        #[arg(long)]
        queries: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 30)]
        rounds: usize,
    },
}

fn run() -> Result<()> {
    let args = Args::parse();
    match &args.command {
        Action::Daemon => return daemon::serve(),
        Action::SharedMcp => return shared_mcp::serve(io::stdin().lock(), io::stdout().lock()),
        Action::NativeBridge => return daemon::native_bridge(io::stdin().lock(), io::stdout().lock()),
        Action::Doctor => return print(&code_context::diagnostics::doctor(&args.root)?),
        Action::Coverage => return print(&code_context::diagnostics::coverage()?),
        Action::Prepare => {
            return print(&serde_json::json!({
                "root":code_context::registration::prepare_root(&args.root)?
            }));
        }
        Action::Migrate {
            add_scope,
            add_exclude,
            apply,
        } => {
            return print(&code_context::registration::migrate(
                &args.root,
                add_scope,
                add_exclude,
                *apply,
            )?);
        }
        Action::AuthorizeWorktrees { revoke } => {
            return print(&code_context::registration::authorize(&args.root, !revoke)?);
        }
        Action::PruneWorktrees { apply } => {
            return print(&code_context::registration::prune(*apply)?);
        }
        Action::Register | Action::Plan => {
            let planning = matches!(&args.command, Action::Plan);
            let mut policy = MemoryPolicy {
                max_files: args.max_files,
                max_source_bytes: args
                    .max_source_mib
                    .checked_mul(1024 * 1024)
                    .ok_or_else(|| code_context::invalid("max_source_mib overflow"))?,
                max_file_bytes: args
                    .max_file_mib
                    .checked_mul(1024 * 1024)
                    .ok_or_else(|| code_context::invalid("max_file_mib overflow"))?,
                max_total_lines: args.max_total_lines,
                max_total_path_bytes: args
                    .max_path_mib
                    .checked_mul(1024 * 1024)
                    .ok_or_else(|| code_context::invalid("max_path_mib overflow"))?,
                max_watch_dirs: args.max_watch_dirs,
                ..MemoryPolicy::default()
            };
            policy.max_staged_bytes = policy.max_staged_bytes.min(policy.max_source_bytes);
            policy.validate().map_err(code_context::invalid)?;
            let requested_spec = catalog::Spec {
                root: args.root,
                scopes: args.scope,
                globs: args.glob,
                excludes: args.exclude,
                default_excludes: !args.no_default_excludes,
                prewarm: args.prewarm,
                policy,
                allow_worktrees: false,
                managed_parent: None,
            };
            let spec = if args.preserve_registration_settings {
                let root = catalog::canonicalize(&requested_spec.root)?;
                let existing = catalog::load()?
                    .into_iter()
                    .find(|spec| spec.root == root)
                    .ok_or_else(|| {
                        code_context::invalid(
                            "--preserve-registration-settings requires an existing exact root",
                        )
                    })?;
                catalog::Spec {
                    root,
                    scopes: requested_spec.scopes,
                    globs: requested_spec.globs,
                    ..existing
                }
            } else {
                requested_spec
            };
            let workspace = spec.workspace()?;
            if !planning
                && spec.prewarm
                && workspace.scopes.iter().any(|scope| scope == ".")
                && !args.allow_root_scope
            {
                return Err(code_context::invalid(
                    "prewarming scope '.' requires --allow-root-scope after reviewing cctx plan",
                )
                .into());
            }
            let inventory = workspace.files_limited(&spec.policy)?;
            let watch_directories = workspace.watch_directories_for(
                inventory.files.iter().map(PathBuf::as_path),
                &spec.policy,
            )?;
            let exclusion_summary = workspace.exclusion_summary();
            let effective_excludes: Vec<_> = workspace
                .excludes
                .iter()
                .cloned()
                .chain(exclusion_summary.patterns.iter().cloned())
                .collect();
            let plan = serde_json::json!({
                "root":workspace.root,
                "scopes":workspace.scopes,
                "globs":workspace.globs,
                "excludes":workspace.excludes,
                "default_excludes":workspace.default_excludes,
                "detected_profiles":exclusion_summary.profiles,
                "effective_excludes":effective_excludes,
                "prewarm":spec.prewarm,
                "files":inventory.files.len(),
                "watch_directories":watch_directories.len(),
                "path_bytes":inventory.total_path_bytes,
                "metadata_source_bytes":inventory.metadata_source_bytes,
                "policy":spec.policy
            });
            if planning {
                return print(&plan);
            }
            catalog::register(spec)?;
            return print(&serde_json::json!({"registered":true,"plan":plan}));
        }
        Action::Unregister => {
            catalog::unregister(&args.root)?;
            return print(&serde_json::json!({"unregistered":true}));
        }
        Action::Ping | Action::Workspaces => {
            let name = if matches!(args.command, Action::Ping) {
                "ping"
            } else {
                "code_workspaces"
            };
            return print(&daemon::request(&daemon::Request {
                root: None,
                name: name.into(),
                arguments: serde_json::json!({}),
                compact: false,
            })?);
        }
        _ => {}
    }
    let benchmark = matches!(
        args.command,
        Action::Bench { .. } | Action::BenchResident { .. }
    );
    if args.backend == Backend::Shared && !benchmark {
        let (name, arguments) = match &args.command {
            Action::Files { glob, limit, path } => (
                "code_files",
                serde_json::json!({"glob":glob,"limit":limit,"paths":path}),
            ),
            Action::Grep {
                pattern,
                regex,
                ignore_case,
                limit,
                path,
                filter,
            } => (
                "code_grep",
                serde_json::json!({
                "pattern":pattern,"regex":regex,"ignore_case":ignore_case,"limit":limit,"paths":path,"glob":filter}),
            ),
            Action::Context {
                pattern,
                regex,
                ignore_case,
                limit,
                radius,
                path,
                filter,
            } => (
                "code_context",
                serde_json::json!({
                "pattern":pattern,"regex":regex,"ignore_case":ignore_case,"limit":limit,"radius":radius,"paths":path,"glob":filter}),
            ),
            Action::Read {
                path,
                start,
                end,
                page,
            } => (
                "code_read",
                serde_json::json!({"ranges":[{"path":path,"start":start,"end":end}],"page":page}),
            ),
            Action::Sync => ("code_sync", serde_json::json!({})),
            Action::Status => ("code_status", serde_json::json!({})),
            Action::Mcp => return shared_mcp::serve(io::stdin().lock(), io::stdout().lock()),
            _ => return Err(code_context::invalid("unsupported shared command").into()),
        };
        return print(&daemon::request(&daemon::Request {
            root: Some(catalog::canonicalize(&args.root)?),
            name: name.into(),
            arguments,
            compact: false,
        })?);
    }
    let workspace = Workspace::new(&args.root, args.scope, args.glob)?;
    if let Action::BenchResident {
        queries,
        output,
        rounds,
    } = &args.command
    {
        return resident_experiment::run(&workspace, queries, output, *rounds);
    }
    if let Action::Bench {
        queries,
        output,
        rounds,
    } = &args.command
    {
        return experiment::run(&workspace, queries, output, *rounds);
    }
    let mut engine = Engine::new(workspace, args.backend)?;
    let value = match args.command {
        Action::Files { glob, limit, path } => {
            serde_json::to_value(engine.files(&code_context::files::FileQuery {
                glob,
                paths: path,
                limit,
            })?)?
        }
        Action::Grep {
            pattern,
            regex,
            ignore_case,
            limit,
            path,
            filter,
        } => serde_json::to_value(
            engine
                .search(&Search {
                    // Path filtering is provided by the shared backend.
                    pattern,
                    regex,
                    ignore_case,
                    limit,
                })
                .and_then(|value| {
                    if path.is_empty() && filter.is_none() {
                        Ok(value)
                    } else {
                        Err(code_context::invalid("path/filter requires shared backend").into())
                    }
                })?,
        )?,
        Action::Context {
            pattern,
            limit,
            radius,
            regex,
            ignore_case,
            path,
            filter,
        } => serde_json::to_value(
            engine
                .context(&ContextRequest {
                    search: Search {
                        pattern,
                        regex,
                        ignore_case,
                        limit,
                    },
                    radius,
                })
                .and_then(|value| {
                    if path.is_empty() && filter.is_none() {
                        Ok(value)
                    } else {
                        Err(code_context::invalid("path/filter requires shared backend").into())
                    }
                })?,
        )?,
        Action::Read {
            path,
            start,
            end,
            page,
        } => {
            if page {
                return Err(code_context::invalid("--page requires the shared backend").into());
            }
            serde_json::to_value(engine.read_many(&[ReadRange { path, start, end }])?)?
        }
        Action::Mcp => {
            return mcp::serve_engine(&mut engine, io::stdin().lock(), io::stdout().lock());
        }
        _ => return Err(code_context::invalid("command requires shared backend").into()),
    };
    print(&value)
}

fn print(value: &serde_json::Value) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, &value)?;
    out.write_all(b"\n")?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("cctx: {error}");
        std::process::exit(2);
    }
}
