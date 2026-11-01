use code_context::catalog::Spec;
use code_context::exclusion_policy::UNIVERSAL_EXCLUDED_DIRS;
use code_context::policy::MemoryPolicy;
use code_context::watched::Watched;
use code_context::workspace::Workspace;
use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};

fn files(workspace: &Workspace) -> Vec<String> {
    let mut files: Vec<_> = workspace
        .files_limited(&MemoryPolicy::default())
        .unwrap()
        .files
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    files.sort();
    files
}

#[test]
fn universal_exact_and_glob_outputs_are_excluded() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/source.rs"), "source").unwrap();
    for directory in UNIVERSAL_EXCLUDED_DIRS {
        let path = root.path().join(directory);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("generated.rs"), "generated").unwrap();
    }
    for directory in [
        "build-debug",
        "cmake-build-release",
        "bazel-workspace",
        "mutants.out123",
    ] {
        let path = root.path().join(directory);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("generated.rs"), "generated").unwrap();
    }
    let workspace = Workspace::new(root.path(), vec![".".into()], vec!["*.rs".into()]).unwrap();
    assert_eq!(files(&workspace), ["src/source.rs"]);
    assert_eq!(
        workspace
            .watch_directories(&MemoryPolicy::default())
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn build_prefixed_source_files_are_not_generated_directories() {
    let root = tempfile::tempdir().unwrap();
    let scripts = root.path().join("scripts/deployment/release");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::write(scripts.join("build-deployment-bundle.py"), "build_marker = 1\n").unwrap();
    std::fs::write(scripts.join("build-standalone-deployment-kit.sh"), "echo build_marker\n").unwrap();
    for prefix in ["build-debug", "cmake-build-release", "bazel-output"] {
        let generated = root.path().join(prefix);
        std::fs::create_dir(&generated).unwrap();
        std::fs::write(generated.join("generated.py"), "build_marker = 2\n").unwrap();
    }
    let spec = Spec {
        root: root.path().to_path_buf(), scopes: vec![".".into()],
        globs: vec!["*.py".into(), "*.sh".into()], excludes: vec![],
        default_excludes: true, prewarm: false, policy: MemoryPolicy::default(),
        allow_worktrees: false, managed_parent: None,
    };
    let mut watched = Watched::new(spec).unwrap();
    let search = watched.call("code_context", json!({"pattern":"build_marker","radius":0})).unwrap();
    assert_eq!(search["search"]["total_matches"], 2);
    let denied = watched.call("code_read", json!({"ranges":[{"path":"build-debug/generated.py","start":1,"end":1}]}))
        .unwrap_err().to_string();
    assert!(denied.contains("reason=excluded_by_rule"), "{denied}");
    assert!(denied.contains("Do not retry"), "{denied}");
}

#[test]
fn explicit_target_diagnostics_separate_rules_globs_scope_and_ignore() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("ignored.rs"), "ignored source\n").unwrap();
    std::fs::write(root.path().join("denied.rs"), "denied source\n").unwrap();
    std::fs::write(root.path().join(".ignore"), "ignored.rs\n").unwrap();
    let workspace = Workspace::new_with_excludes(
        root.path(), vec![".".into()], vec!["*.rs".into()], vec!["denied.rs".into()], true,
    ).unwrap();
    let spec = Spec {
        root: workspace.root.clone(), scopes: workspace.scopes.clone(),
        globs: workspace.globs.clone(), excludes: workspace.excludes.clone(),
        default_excludes: true, prewarm: false, policy: MemoryPolicy::default(),
        allow_worktrees: false, managed_parent: None,
    };
    let mut watched = Watched::new(spec).unwrap();
    for (path, reason) in [("denied.rs", "excluded_by_rule"), ("ignored.rs", "ignored_by_discovery")] {
        let error = watched.call("code_context", json!({"pattern":"source","paths":[path]}))
            .unwrap_err().to_string();
        assert!(error.contains(&format!("reason={reason}")), "{error}");
        assert!(error.contains("Do not retry"), "{error}");
    }
    for (path, reason) in [("other.txt", "unsupported_glob"), ("denied.rs", "excluded_by_rule")] {
        let error = watched.call("code_read", json!({"ranges":[{"path":path,"start":1,"end":1}]}))
            .unwrap_err().to_string();
        assert!(error.contains(&format!("reason={reason}")), "{error}");
    }
    assert_eq!(watched.call("code_status", json!({})).unwrap()["io_profile"]["full_source_verifications"], 0);
}

#[test]
fn ecosystem_markers_activate_all_profiles() {
    let cases: &[(&str, &str, &str)] = &[
        ("node", "package.json", "logs/generated.rs"),
        ("python", "pyproject.toml", "wheels/generated.rs"),
        ("cmake", "CMakeLists.txt", "CMakeFiles/generated.rs"),
        ("dotnet", "app.sln", "bin/generated.rs"),
        ("swift", "Package.swift", "Carthage/Build/generated.rs"),
        ("beam", "mix.exs", "deps/generated.rs"),
        ("haskell", "stack.yaml", "dist-ghc/generated.rs"),
        ("ruby", "Gemfile", "vendor/bundle/generated.rs"),
        ("php", "composer.json", "vendor/generated.rs"),
        ("go", "go.mod", "vendor/generated.rs"),
        ("perl", "Makefile.PL", "blib/generated.rs"),
        (
            "unity",
            "ProjectSettings/ProjectVersion.txt",
            "Library/generated.rs",
        ),
        ("unreal", "Game.uproject", "Intermediate/generated.rs"),
        ("dart", "pubspec.yaml", "doc/api/generated.rs"),
        ("julia", "Project.toml", "deps/usr/generated.rs"),
        ("r", "DESCRIPTION", "rsconnect/generated.rs"),
        ("android", "AndroidManifest.xml", "captures/generated.rs"),
        ("ros", "package.xml", "devel/generated.rs"),
        ("ocaml", "dune-project", "_opam/generated.rs"),
        ("clojure", "deps.edn", "classes/generated.rs"),
    ];
    for (profile, marker, excluded) in cases {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/source.rs"), "source").unwrap();
        let marker = root.path().join(marker);
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(marker, "").unwrap();
        if *profile == "r" {
            std::fs::write(root.path().join("NAMESPACE"), "").unwrap();
        }

        let excluded = root.path().join(excluded);
        std::fs::create_dir_all(excluded.parent().unwrap()).unwrap();
        std::fs::write(excluded, "generated").unwrap();
        let workspace = Workspace::new(root.path(), vec![".".into()], vec!["*.rs".into()]).unwrap();
        assert!(
            workspace.detected_profiles().contains(profile),
            "missing profile {profile}"
        );
        assert_eq!(files(&workspace), ["src/source.rs"], "{profile}");
    }
}

#[test]
fn profiles_with_universal_or_hidden_outputs_are_still_reported() {
    let cases = [
        ("rust", "Cargo.toml"),
        ("jvm", "pom.xml"),
        ("scala", "build.sbt"),
        ("godot", "project.godot"),
        ("terraform", "main.tf"),
        ("zig", "build.zig"),
        ("nim", "package.nimble"),
    ];
    for (profile, marker) in cases {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(marker), "").unwrap();
        let workspace = Workspace::new(root.path(), vec![".".into()], vec!["*.rs".into()]).unwrap();
        assert!(
            workspace.detected_profiles().contains(&profile),
            "{profile}"
        );
    }
}

#[test]
fn markers_inside_registered_scopes_are_detected() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("frontend/logs")).unwrap();
    std::fs::write(root.path().join("frontend/package.json"), "{}").unwrap();
    std::fs::write(root.path().join("frontend/source.ts"), "source").unwrap();
    std::fs::write(root.path().join("frontend/logs/generated.ts"), "generated").unwrap();
    let workspace =
        Workspace::new(root.path(), vec!["frontend".into()], vec!["*.ts".into()]).unwrap();
    assert!(workspace.detected_profiles().contains(&"node"));
    assert_eq!(files(&workspace), ["frontend/source.ts"]);
}

#[test]
fn scoped_profile_does_not_hide_same_named_source_elsewhere() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src/logs")).unwrap();
    std::fs::create_dir_all(root.path().join("frontend/logs")).unwrap();
    std::fs::write(root.path().join("src/logs/source.ts"), "source").unwrap();
    std::fs::write(root.path().join("frontend/package.json"), "{}").unwrap();
    std::fs::write(root.path().join("frontend/source.ts"), "source").unwrap();
    std::fs::write(root.path().join("frontend/logs/generated.ts"), "generated").unwrap();
    let workspace = Workspace::new(
        root.path(),
        vec!["src".into(), "frontend".into()],
        vec!["*.ts".into()],
    )
    .unwrap();
    assert_eq!(
        files(&workspace),
        ["frontend/source.ts", "src/logs/source.ts"]
    );
}

#[test]
fn ambiguous_directories_remain_searchable_without_markers() {
    let root = tempfile::tempdir().unwrap();
    for directory in [
        "vendor",
        "deps",
        "bin",
        "lib",
        "packages",
        "pkg",
        "docs",
        "site",
        "generated",
        "artifacts",
        "tmp",
        "logs",
        "debug",
        "release",
        "cache",
    ] {
        let path = root.path().join(directory);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("source.rs"), "source").unwrap();
    }
    let workspace = Workspace::new(root.path(), vec![".".into()], vec!["*.rs".into()]).unwrap();
    assert_eq!(files(&workspace).len(), 15);
    assert!(workspace.detected_profiles().is_empty());
}

#[test]
fn nuget_packages_require_the_nuget_marker() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("app.sln"), "").unwrap();
    std::fs::create_dir(root.path().join("packages")).unwrap();
    std::fs::write(root.path().join("packages/source.rs"), "source").unwrap();
    let workspace = Workspace::new(root.path(), vec![".".into()], vec!["*.rs".into()]).unwrap();
    assert_eq!(files(&workspace), ["packages/source.rs"]);

    std::fs::write(root.path().join("packages/repositories.config"), "").unwrap();
    assert!(files(&workspace).is_empty());
}

#[test]
fn opt_out_disables_universal_and_profile_exclusions() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("composer.json"), "{}").unwrap();
    std::fs::create_dir_all(root.path().join("vendor/pkg")).unwrap();
    std::fs::write(root.path().join("vendor/pkg/source.rs"), "source").unwrap();
    std::fs::create_dir(root.path().join("node_modules")).unwrap();
    std::fs::write(root.path().join("node_modules/source.rs"), "source").unwrap();
    let workspace = Workspace::new_with_excludes(
        root.path(),
        vec![".".into()],
        vec!["*.rs".into()],
        Vec::new(),
        false,
    )
    .unwrap();
    assert_eq!(files(&workspace).len(), 2);
    assert!(workspace.detected_profiles().is_empty());
}

#[test]
fn live_profile_marker_changes_update_membership() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::create_dir(root.path().join("vendor")).unwrap();
    std::fs::write(root.path().join("src/source.rs"), "source").unwrap();
    std::fs::write(root.path().join("vendor/dependency.rs"), "profile_marker").unwrap();
    let mut watched = Watched::new(Spec {
        root: root.path().to_path_buf(),
        scopes: vec![".".into()],
        globs: vec!["*.rs".into()],
        excludes: Vec::new(),
        default_excludes: true,
        prewarm: false,
        policy: MemoryPolicy::default(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();
    assert_eq!(
        watched
            .call("code_grep", json!({"pattern":"profile_marker"}))
            .unwrap()["total_matches"],
        1
    );

    std::fs::write(root.path().join("composer.json"), "{}").unwrap();
    wait_for_count(&mut watched, 0);
    std::fs::remove_file(root.path().join("composer.json")).unwrap();
    wait_for_count(&mut watched, 1);
}

fn wait_for_count(watched: &mut Watched, expected: u64) {
    let started = Instant::now();
    loop {
        let count = watched
            .call("code_grep", json!({"pattern":"profile_marker"}))
            .unwrap()["total_matches"]
            .as_u64()
            .unwrap();
        if count == expected {
            return;
        }
        assert!(started.elapsed() < Duration::from_secs(4));
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn profile_marker_recognizes_extensions_and_nested_markers() {
    for path in [
        "app.sln",
        "app.csproj",
        "Game.uproject",
        "library.opam",
        "go.mod",
        "Cargo.toml",
        "main.tf",
        "package.nimble",
        "ProjectSettings/ProjectVersion.txt",
        "vite.config.ts",
        "vitest.config.ts",
        "next.config.ts",
        "svelte.config.js",
        "astro.config.mjs",
    ] {
        assert!(code_context::exclusion_policy::is_profile_marker(
            Path::new(path)
        ));
    }
}

#[test]
fn frontend_and_vite_profiles_exclude_lockfiles_bundles_sourcemaps_and_generated_outputs() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src/components")).unwrap();
    std::fs::create_dir_all(root.path().join("src/__generated__")).unwrap();
    std::fs::create_dir_all(root.path().join("storybook-static")).unwrap();
    std::fs::create_dir_all(root.path().join("playwright-report")).unwrap();
    std::fs::create_dir_all(root.path().join("test-results")).unwrap();
    std::fs::create_dir_all(root.path().join("dist-ssr")).unwrap();
    std::fs::create_dir_all(root.path().join("dev-dist")).unwrap();

    std::fs::write(root.path().join("package.json"), "{\"name\":\"app\"}\n").unwrap();
    std::fs::write(root.path().join("vite.config.ts"), "export default {}\n").unwrap();
    std::fs::write(
        root.path().join("src/components/Button.tsx"),
        "export const Button = () => <button />;\n",
    )
    .unwrap();

    // Lockfiles, minified bundles, sourcemaps, and generated Vite/Relay/TanStack outputs
    for noisy in [
        "package-lock.json",
        "npm-shrinkwrap.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "bun.lock",
        "vite.config.ts.timestamp-1730000000000-abc123.mjs",
        "src/routeTree.gen.ts",
        "src/__generated__/graphql.ts",
        "src/vendor.min.js",
        "src/vendor.min.mjs",
        "src/styles.min.css",
        "src/app.bundle.js",
        "src/app.chunk.js",
        "src/App.tsx.map",
        "src/styles.css.map",
        "src/types.d.ts.map",
        "storybook-static/iframe.js",
        "playwright-report/index.html",
        "test-results/report.json",
        "dist-ssr/entry-server.js",
        "dev-dist/sw.js",
    ] {
        std::fs::write(root.path().join(noisy), "noisy_artifact\n").unwrap();
    }

    let workspace = Workspace::new(
        root.path(),
        vec![".".into()],
        vec![
            "*.json".into(),
            "*.yaml".into(),
            "*.ts".into(),
            "*.tsx".into(),
            "*.js".into(),
            "*.mjs".into(),
            "*.css".into(),
            "*.html".into(),
        ],
    )
    .unwrap();

    let profiles = workspace.detected_profiles();
    assert!(profiles.contains(&"node"), "{profiles:?}");
    assert!(profiles.contains(&"vite"), "{profiles:?}");
    assert_eq!(
        files(&workspace),
        [
            "package.json",
            "src/components/Button.tsx",
            "vite.config.ts"
        ]
    );
}
