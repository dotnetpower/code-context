use std::collections::BTreeSet;
use std::path::Path;

pub const UNIVERSAL_EXCLUDED_DIRS: &[&str] = &[
    "node_modules",
    "bower_components",
    "jspm_packages",
    "web_modules",
    "lib-cov",
    "target",
    "dist",
    "build",
    "out",
    "coverage",
    "htmlcov",
    "__pycache__",
    "__pypackages__",
    "develop-eggs",
    "cython_debug",
    "venv",
    "env",
    ".venv",
    ".tox",
    ".nox",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".ipynb_checkpoints",
    ".quarto",
    ".vitepress",
    "_site",
    "_book",
    ".next",
    ".nuxt",
    ".vite",
    ".turbo",
    ".parcel-cache",
    ".svelte-kit",
    ".astro",
    ".output",
    ".vercel",
    ".netlify",
    ".wrangler",
    ".docusaurus",
    "storybook-static",
    "playwright-report",
    "test-results",
    "dist-ssr",
    "dev-dist",
    ".gradle",
    ".dart_tool",
    "CMakeFiles",
    "vcpkg_installed",
    "_deps",
    "_build",
    "_checkouts",
    "dist-newstyle",
    "zig-out",
    "zig-pkg",
    "nimcache",
    "nimblecache",
    "htmldocs",
    "TestResults",
    "BenchmarkDotNet.Artifacts",
    "AppPackages",
    "FakesAssemblies",
    "DerivedDataCache",
    "MemoryCaptures",
    "Recordings",
    "Addressables_Temp",
    "ExportedObj",
    "ServerData",
    "build_isolated",
    "devel_isolated",
    "msg_gen",
    "srv_gen",
    "Pods",
    "DerivedData",
    ".cursor",
    ".claude",
    ".continue",
    ".windsurf",
    ".aider.tags.cache.v3",
    ".nx",
    ".angular",
    ".expo",
    ".sst",
    ".serverless",
    ".terraform",
    ".terragrunt-cache",
    ".direnv",
    ".devenv",
    ".pixi",
    "cdk.out",
    "buck-out",
];

const UNIVERSAL_GLOBS: &[&str] = &[
    "**/secrets.*",
    "**/credentials.*",
    "**/*.pem",
    "**/*.key",
    "**/*.p12",
    "**/*.pfx",
    "**/build-*/**",
    "**/cmake-build-*/**",
    "**/bazel-*/**",
    "**/mutants.out*",
    "**/mutants.out*/**",
    "**/*.min.js",
    "**/*.min.mjs",
    "**/*.min.cjs",
    "**/*.min.css",
    "**/*.bundle.js",
    "**/*.chunk.js",
    "**/*.map",
    "**/*.js.map",
    "**/*.css.map",
    "**/*.d.ts.map",
    "**/~$*",
    "**/.~lock.*#",
    "**/*.aux",
    "**/*.bbl",
    "**/*.blg",
    "**/*.fdb_latexmk",
    "**/*.fls",
    "**/*.lof",
    "**/*.lot",
    "**/*.nav",
    "**/*.snm",
    "**/*.synctex.gz",
    "**/*.toc",
    "**/*.xdv",
];

const NODE: &[&str] = &[
    "**/logs",
    "**/logs/**",
    "**/pids",
    "**/pids/**",
    "**/package-lock.json",
    "**/npm-shrinkwrap.json",
    "**/pnpm-lock.yaml",
    "**/yarn.lock",
    "**/bun.lock",
    "**/vite.config.*.timestamp-*",
    "**/__generated__/**",
    "**/routeTree.gen.ts",
];
const VITE: &[&str] = &[
    "**/dist-ssr/**",
    "**/dev-dist/**",
    "**/storybook-static/**",
    "**/playwright-report/**",
    "**/test-results/**",
    "**/vite.config.*.timestamp-*",
    "**/__generated__/**",
    "**/routeTree.gen.ts",
];
const PYTHON: &[&str] = &[
    "**/downloads/**",
    "**/eggs/**",
    "**/parts/**",
    "**/sdist/**",
    "**/wheels/**",
    "**/cover/**",
    "**/instance/**",
    "**/profile_default/**",
    "**/mnesia/**",
    "**/rabbitmq/**",
    "**/rabbitmq-data/**",
    "**/activemq-data/**",
    "**/share/python-wheels/**",
    "**/docs/_build/**",
];
const DOTNET: &[&str] = &[
    "**/[Bb]in/**",
    "**/[Oo]bj/**",
    "**/[Aa]rtifacts/**",
    "**/[Pp]ublish/**",
    "**/Generated_Code/**",
];
const SWIFT: &[&str] = &[
    "Carthage/Build/**",
    "Carthage/Checkouts/**",
    "**/xcuserdata/**",
];
const BEAM: &[&str] = &[
    "**/deps/**",
    "**/cover/**",
    "**/doc/**",
    "**/_build/**",
    "**/_checkouts/**",
];
const HASKELL: &[&str] = &["**/dist-*", "**/dist-*/**", "**/cabal-dev/**"];
const RUBY: &[&str] = &[
    "**/vendor/bundle/**",
    "**/tmp/**",
    "**/log/**",
    "**/pkg/**",
    "**/spec/reports/**",
    "**/public/system/**",
    "**/public/packs/**",
    "**/public/packs-test/**",
    "**/public/assets/**",
];
const PHP: &[&str] = &["**/vendor/**"];
const GO: &[&str] = &["**/vendor/**"];
const PERL: &[&str] = &["**/blib/**", "**/_eumm/**", "**/local/**", "**/cover_db/**"];
const UNITY: &[&str] = &[
    "[Ll]ibrary/**",
    "[Tt]emp/**",
    "[Oo]bj/**",
    "[Bb]uild/**",
    "[Bb]uilds/**",
    "[Ll]ogs/**",
    "[Uu]ser[Ss]ettings/**",
    "[Rr]ecordings/**",
    "[Ee]xported[Oo]bj/**",
];
const UNREAL: &[&str] = &[
    "Binaries/**",
    "Plugins/**/Binaries/**",
    "Build/**",
    "Intermediate/**",
    "Plugins/**/Intermediate/**",
    "Saved/**",
    "DerivedDataCache/**",
];
const DART: &[&str] = &["**/doc/api/**"];
const JULIA: &[&str] = &[
    "deps/downloads/**",
    "deps/usr/**",
    "deps/src/**",
    "docs/build/**",
    "docs/site/**",
];
const R_PROJECT: &[&str] = &[
    "**/*_cache/**",
    "**/cache/**",
    "**/docs/**",
    "**/rsconnect/**",
];
const ANDROID: &[&str] = &["**/captures/**"];
const ROS: &[&str] = &[
    "**/devel/**",
    "**/logs/**",
    "**/bin/**",
    "**/lib/**",
    "**/msg_gen/**",
    "**/srv_gen/**",
    "**/build_isolated/**",
    "**/devel_isolated/**",
];
const OCAML: &[&str] = &["**/_opam/**"];
const CLOJURE: &[&str] = &["**/classes/**", "**/checkouts/**", "**/lib/**"];
const MDBOOK: &[&str] = &["**/book/**"];
const MKDOCS: &[&str] = &["**/site/**"];
const QUARTO: &[&str] = &["**/_site/**", "**/_freeze/**", "**/*_files/**"];

struct ProfileRule {
    name: &'static str,
    files: &'static [&'static str],
    extensions: &'static [&'static str],
    patterns: &'static [&'static str],
}

const PROFILE_RULES: &[ProfileRule] = &[
    ProfileRule {
        name: "node",
        files: &[
            "package.json",
            "bun.lock",
            "bunfig.toml",
            "deno.json",
            "deno.jsonc",
            "pnpm-workspace.yaml",
            "turbo.json",
            "nx.json",
        ],
        extensions: &[],
        patterns: NODE,
    },
    ProfileRule {
        name: "vite",
        files: &[
            "vite.config.ts",
            "vite.config.js",
            "vite.config.mts",
            "vite.config.mjs",
            "vitest.config.ts",
            "vitest.config.js",
            "vitest.config.mts",
            "next.config.js",
            "next.config.mjs",
            "next.config.ts",
            "nuxt.config.ts",
            "nuxt.config.js",
            "svelte.config.js",
            "astro.config.mjs",
            "astro.config.ts",
            "remix.config.js",
            "react-router.config.ts",
        ],
        extensions: &[],
        patterns: VITE,
    },
    ProfileRule {
        name: "python",
        files: &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "uv.lock",
            "Pipfile",
            "pixi.toml",
        ],
        extensions: &["py"],
        patterns: PYTHON,
    },
    ProfileRule {
        name: "cmake",
        files: &["CMakeLists.txt"],
        extensions: &[],
        patterns: &[],
    },
    ProfileRule {
        name: "dotnet",
        files: &[],
        extensions: &["sln", "csproj", "fsproj", "vbproj"],
        patterns: DOTNET,
    },
    ProfileRule {
        name: "swift",
        files: &["Package.swift"],
        extensions: &[],
        patterns: SWIFT,
    },
    ProfileRule {
        name: "beam",
        files: &["mix.exs", "rebar.config"],
        extensions: &[],
        patterns: BEAM,
    },
    ProfileRule {
        name: "haskell",
        files: &["cabal.project", "stack.yaml"],
        extensions: &["cabal"],
        patterns: HASKELL,
    },
    ProfileRule {
        name: "ruby",
        files: &["Gemfile"],
        extensions: &[],
        patterns: RUBY,
    },
    ProfileRule {
        name: "php",
        files: &["composer.json"],
        extensions: &[],
        patterns: PHP,
    },
    ProfileRule {
        name: "go",
        files: &["go.mod"],
        extensions: &[],
        patterns: GO,
    },
    ProfileRule {
        name: "perl",
        files: &["Makefile.PL", "Build.PL", "cpanfile"],
        extensions: &[],
        patterns: PERL,
    },
    ProfileRule {
        name: "unity",
        files: &["ProjectSettings/ProjectVersion.txt"],
        extensions: &[],
        patterns: UNITY,
    },
    ProfileRule {
        name: "unreal",
        files: &[],
        extensions: &["uproject"],
        patterns: UNREAL,
    },
    ProfileRule {
        name: "dart",
        files: &["pubspec.yaml"],
        extensions: &[],
        patterns: DART,
    },
    ProfileRule {
        name: "julia",
        files: &["Project.toml"],
        extensions: &[],
        patterns: JULIA,
    },
    ProfileRule {
        name: "android",
        files: &["AndroidManifest.xml"],
        extensions: &[],
        patterns: ANDROID,
    },
    ProfileRule {
        name: "ros",
        files: &["package.xml"],
        extensions: &[],
        patterns: ROS,
    },
    ProfileRule {
        name: "ocaml",
        files: &["dune-project"],
        extensions: &["opam"],
        patterns: OCAML,
    },
    ProfileRule {
        name: "clojure",
        files: &["project.clj", "deps.edn"],
        extensions: &[],
        patterns: CLOJURE,
    },
    ProfileRule {
        name: "rust",
        files: &["Cargo.toml"],
        extensions: &[],
        patterns: &[],
    },
    ProfileRule {
        name: "jvm",
        files: &[
            "pom.xml",
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
        ],
        extensions: &[],
        patterns: &[],
    },
    ProfileRule {
        name: "scala",
        files: &["build.sbt"],
        extensions: &[],
        patterns: &[],
    },
    ProfileRule {
        name: "godot",
        files: &["project.godot"],
        extensions: &[],
        patterns: &[],
    },
    ProfileRule {
        name: "terraform",
        files: &[],
        extensions: &["tf"],
        patterns: &[],
    },
    ProfileRule {
        name: "zig",
        files: &["build.zig"],
        extensions: &[],
        patterns: &[],
    },
    ProfileRule {
        name: "nim",
        files: &[],
        extensions: &["nimble"],
        patterns: &[],
    },
    ProfileRule {
        name: "mdbook",
        files: &["book.toml"],
        extensions: &[],
        patterns: MDBOOK,
    },
    ProfileRule {
        name: "mkdocs",
        files: &["mkdocs.yml", "mkdocs.yaml"],
        extensions: &[],
        patterns: MKDOCS,
    },
    ProfileRule {
        name: "quarto",
        files: &["_quarto.yml", "_quarto.yaml"],
        extensions: &[],
        patterns: QUARTO,
    },
];

#[derive(Clone, Debug)]
pub struct EffectiveExclusions {
    pub profiles: Vec<&'static str>,
    pub patterns: Vec<String>,
}

struct RootSnapshot {
    files: std::collections::HashSet<String>,
    extensions: std::collections::HashSet<String>,
    has_project_settings: bool,
    has_packages: bool,
}

impl RootSnapshot {
    fn scan(root: &Path) -> Self {
        let mut files = std::collections::HashSet::new();
        let mut extensions = std::collections::HashSet::new();
        let mut has_project_settings = false;
        let mut has_packages = false;
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.filter_map(std::result::Result::ok) {
                let name_os = entry.file_name();
                let Some(name) = name_os.to_str() else {
                    continue;
                };
                let is_file = match entry.file_type() {
                    Ok(ft) if ft.is_file() => true,
                    Ok(ft) if ft.is_dir() => {
                        if name == "ProjectSettings" {
                            has_project_settings = true;
                        } else if name == "packages" {
                            has_packages = true;
                        }
                        false
                    }
                    _ => {
                        let path = entry.path();
                        if path.is_dir() {
                            if name == "ProjectSettings" {
                                has_project_settings = true;
                            } else if name == "packages" {
                                has_packages = true;
                            }
                            false
                        } else {
                            path.is_file()
                        }
                    }
                };
                if is_file {
                    files.insert(name.to_owned());
                    if let Some(ext) = Path::new(name).extension().and_then(|e| e.to_str()) {
                        extensions.insert(ext.to_owned());
                    }
                }
            }
        }
        Self {
            files,
            extensions,
            has_project_settings,
            has_packages,
        }
    }

    fn matches(&self, root: &Path, rule: &ProfileRule) -> bool {
        rule.files.iter().any(|&name| {
            if name == "ProjectSettings/ProjectVersion.txt" {
                self.has_project_settings && root.join(name).is_file()
            } else {
                self.files.contains(name)
            }
        }) || rule
            .extensions
            .iter()
            .any(|&ext| self.extensions.contains(ext))
    }
}

fn base_universal_patterns() -> &'static BTreeSet<String> {
    static BASE: std::sync::OnceLock<BTreeSet<String>> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        let mut patterns = BTreeSet::new();
        for directory in UNIVERSAL_EXCLUDED_DIRS {
            patterns.insert(format!("**/{directory}"));
            patterns.insert(format!("**/{directory}/**"));
        }
        patterns.extend(UNIVERSAL_GLOBS.iter().map(|pattern| (*pattern).to_owned()));
        patterns
    })
}

pub fn detect(root: &Path, scopes: &[String]) -> EffectiveExclusions {
    let mut roots = vec![(String::new(), root.to_path_buf())];
    roots.extend(
        scopes
            .iter()
            .filter(|scope| scope.as_str() != ".")
            .map(|scope| (scope.clone(), root.join(scope))),
    );
    let mut profiles = BTreeSet::new();
    let mut patterns = base_universal_patterns().clone();

    for (prefix, project_root) in roots {
        let snapshot = RootSnapshot::scan(project_root.as_path());
        for rule in PROFILE_RULES {
            apply_scoped(
                snapshot.matches(project_root.as_path(), rule),
                prefix.as_str(),
                rule.name,
                rule.patterns,
                &mut profiles,
                &mut patterns,
            );
        }
        apply_scoped(
            snapshot.files.contains("DESCRIPTION")
                && (snapshot.files.contains("NAMESPACE")
                    || snapshot.extensions.contains("R")
                    || snapshot.extensions.contains("r")),
            prefix.as_str(),
            "r",
            R_PROJECT,
            &mut profiles,
            &mut patterns,
        );
        if snapshot.has_packages
            && project_root.join("packages/repositories.config").is_file()
        {
            patterns.insert(scoped_pattern(prefix.as_str(), "packages/**"));
        }
    }

    EffectiveExclusions {
        profiles: profiles.into_iter().collect(),
        patterns: patterns.into_iter().collect(),
    }
}

pub fn is_profile_marker(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if matches!(
        name,
        "package.json"
            | "vite.config.ts"
            | "vite.config.js"
            | "vite.config.mts"
            | "vite.config.mjs"
            | "vitest.config.ts"
            | "vitest.config.js"
            | "vitest.config.mts"
            | "next.config.js"
            | "next.config.mjs"
            | "next.config.ts"
            | "svelte.config.js"
            | "astro.config.mjs"
            | "astro.config.ts"
            | "pyproject.toml"
            | "setup.py"
            | "setup.cfg"
            | "CMakeLists.txt"
            | "Package.swift"
            | "mix.exs"
            | "rebar.config"
            | "cabal.project"
            | "stack.yaml"
            | "Gemfile"
            | "composer.json"
            | "go.mod"
            | "Makefile.PL"
            | "Build.PL"
            | "cpanfile"
            | "ProjectVersion.txt"
            | "pubspec.yaml"
            | "Project.toml"
            | "AndroidManifest.xml"
            | "package.xml"
            | "dune-project"
            | "project.clj"
            | "deps.edn"
            | "Cargo.toml"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "settings.gradle"
            | "build.sbt"
            | "project.godot"
            | "build.zig"
            | "DESCRIPTION"
            | "NAMESPACE"
            | "repositories.config"
            | "book.toml"
            | "mkdocs.yml"
            | "mkdocs.yaml"
            | "_quarto.yml"
            | "_quarto.yaml"
    ) {
        return true;
    }
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|ext| {
            matches!(
                ext,
                "py" | "sln"
                    | "csproj"
                    | "fsproj"
                    | "vbproj"
                    | "cabal"
                    | "uproject"
                    | "opam"
                    | "tf"
                    | "nimble"
            )
        })
}

fn apply_scoped(
    enabled: bool,
    prefix: &str,
    profile: &'static str,
    profile_patterns: &[&str],
    profiles: &mut BTreeSet<&'static str>,
    patterns: &mut BTreeSet<String>,
) {
    if enabled {
        profiles.insert(profile);
        patterns.extend(
            profile_patterns
                .iter()
                .map(|pattern| scoped_pattern(prefix, pattern)),
        );
    }
}

fn scoped_pattern(prefix: &str, pattern: &str) -> String {
    if prefix.is_empty() {
        pattern.to_owned()
    } else {
        format!("{prefix}/{pattern}")
    }
}
