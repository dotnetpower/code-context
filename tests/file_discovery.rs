use code_context::catalog::Spec;
use code_context::engine::{Backend, Engine};
use code_context::files::FileQuery;
use code_context::watched::Watched;
use code_context::workspace::Workspace;
use serde_json::json;

fn fixture() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    for name in ["src", "docs", ".github", "node_modules"] {
        std::fs::create_dir(dir.path().join(name)).unwrap();
    }
    for (path, text) in [
        ("src/nativeTools.ts", "const registered = true;\n"),
        ("src/other.ts", "nativeTools.ts appears only here\n"),
        ("README.md", "documentation marker\n"),
        ("docs/guide.md", "documentation marker\n"),
        ("package.json", "{\"name\":\"fixture\"}\n"),
        ("config.yaml", "enabled: true\n"),
        ("Cargo.toml", "[package]\n"),
        ("secrets.json", "{\"token\":\"must remain excluded\"}\n"),
        ("credentials.yaml", "token: excluded\n"),
        (".github/private.md", "hidden\n"),
        ("node_modules/vendor.ts", "generated\n"),
        ("ignored.md", "ignored\n"),
        (".gitignore", "ignored.md\n"),
    ] {
        std::fs::write(dir.path().join(path), text).unwrap();
    }
    let workspace = Workspace::new(
        dir.path(),
        vec![".".into()],
        ["*.ts", "*.md", "*.json", "*.yaml", "*.toml"]
            .into_iter()
            .map(String::from)
            .collect(),
    )
    .unwrap();
    (dir, workspace)
}

fn query(glob: &str, paths: &[&str], limit: usize) -> FileQuery {
    FileQuery {
        glob: glob.into(),
        paths: paths.iter().map(|p| (*p).into()).collect(),
        limit,
    }
}

#[test]
fn filename_discovery_is_not_content_search_and_matches_all_backends() {
    let (_dir, workspace) = fixture();
    for backend in [Backend::Live, Backend::Ram, Backend::Index] {
        let engine = Engine::new(workspace.clone(), backend).unwrap();
        let result = engine.files(&query("nativeTools.ts", &[], 200)).unwrap();
        assert_eq!(result.paths, ["src/nativeTools.ts"]);
        assert_eq!(result.total_matches, 1);
        assert!(!result.truncated);
        let docs = engine.files(&query("*.md", &[], 1)).unwrap();
        assert_eq!(docs.paths, ["README.md"]);
        assert_eq!(docs.total_matches, 2);
        assert!(docs.truncated);
        assert_eq!(
            engine.files(&query("*.md", &["docs"], 20)).unwrap().paths,
            ["docs/guide.md"]
        );
        assert!(
            engine
                .files(&query("*.md", &["doc"], 20))
                .unwrap()
                .paths
                .is_empty()
        );
    }
}

#[test]
fn watched_files_read_and_context_include_docs_configs_but_keep_exclusions() {
    let (dir, workspace) = fixture();
    let mut watched = Watched::new(Spec {
        root: workspace.root,
        scopes: workspace.scopes,
        globs: workspace.globs,
        excludes: vec![],
        default_excludes: true,
        prewarm: false,
        policy: Default::default(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();
    let all = watched.call("code_files", json!({"glob":"*"})).unwrap();
    assert_eq!(
        all["paths"],
        json!([
            "Cargo.toml",
            "README.md",
            "config.yaml",
            "docs/guide.md",
            "package.json",
            "src/nativeTools.ts",
            "src/other.ts",
        ])
    );
    let read = watched
        .call(
            "code_read",
            json!({"ranges":[
                {"path":"README.md","start":1,"end":1},
                {"path":"package.json","start":1,"end":1},
            ]}),
        )
        .unwrap();
    assert_eq!(read[0]["lines"], json!(["documentation marker"]));
    let context = watched
        .call(
            "code_context",
            json!({"pattern":"documentation marker","radius":40}),
        )
        .unwrap();
    assert_eq!(context["search"]["total_matches"], 2);
    assert!(
        watched
            .call("code_context", json!({"pattern":"marker","radius":101}))
            .is_err()
    );
    std::fs::write(dir.path().join("docs/new.md"), "new\n").unwrap();
    watched.call("code_sync", json!({})).unwrap();
    assert_eq!(
        watched
            .call("code_files", json!({"glob":"new.md"}))
            .unwrap()["paths"],
        json!(["docs/new.md"])
    );
    std::fs::remove_file(dir.path().join("docs/new.md")).unwrap();
    watched.call("code_sync", json!({})).unwrap();
    assert_eq!(
        watched
            .call("code_files", json!({"glob":"new.md"}))
            .unwrap()["total_matches"],
        0
    );
}

#[test]
fn file_discovery_rejects_invalid_filters_and_has_explicit_empty_results() {
    let (_dir, workspace) = fixture();
    let engine = Engine::new(workspace, Backend::Index).unwrap();
    for request in [
        query("", &[], 1),
        query("[", &[], 1),
        query("*", &[], 0),
        query("*", &[], 1001),
        query("*", &["../escape"], 1),
        query("*", &[".github"], 1),
    ] {
        assert!(engine.files(&request).is_err(), "{request:?}");
    }
    assert_eq!(
        engine
            .files(&query("missing.*", &[], 1))
            .unwrap()
            .total_matches,
        0
    );
}

#[test]
fn simple_extension_glob_requires_dot_boundary_and_supports_single_char_extensions() {
    let candidates = ["src/bars", "src/a.rs", "src/main.c", "src/lib.h"];
    let rs = code_context::files::find(
        candidates,
        &FileQuery {
            glob: "*.rs".into(),
            paths: Vec::new(),
            limit: 10,
        },
    )
    .unwrap();
    assert_eq!(rs.paths, ["src/a.rs"]);
    let c = code_context::files::find(
        candidates,
        &FileQuery {
            glob: "*.c".into(),
            paths: Vec::new(),
            limit: 10,
        },
    )
    .unwrap();
    assert_eq!(c.paths, ["src/main.c"]);
}

#[test]
fn frontend_globs_support_compound_extensions_brace_globs_dynamic_routes_and_vite_aliases() {
    let dir = tempfile::tempdir().unwrap();
    for subdir in [
        "src/components",
        "src/assets",
        "src/styles",
        "src/shaders",
        "pages/users",
        "app/[...slug]",
    ] {
        std::fs::create_dir_all(dir.path().join(subdir)).unwrap();
    }
    for (path, text) in [
        ("src/components/Button.tsx", "export const Button = () => <button />;\n"),
        ("src/components/Button.test.tsx", "test('Button', () => {});\n"),
        ("src/components/Button.stories.tsx", "export default { title: 'Button' };\n"),
        ("src/components/Button.module.css", ".btn { color: red; }\n"),
        ("src/components/types.d.ts", "export interface ButtonProps {}\n"),
        ("src/assets/icon.svg", "<svg viewBox=\"0 0 24 24\"></svg>\n"),
        ("src/styles/theme.pcss", "@layer base { :root { --bg: #fff; } }\n"),
        ("src/shaders/wave.wgsl", "@vertex fn main() {}\n"),
        ("pages/users/[id].tsx", "export default function UserPage() { return null; }\n"),
        ("app/[...slug]/page.tsx", "export default function CatchAll() { return null; }\n"),
    ] {
        std::fs::write(dir.path().join(path), text).unwrap();
    }

    let mut watched = Watched::new(Spec {
        root: dir.path().to_path_buf(),
        scopes: vec![".".into()],
        globs: code_context::registration::DEFAULT_SOURCE_GLOBS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        excludes: vec![],
        default_excludes: true,
        prewarm: false,
        policy: Default::default(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();

    // Compound & recursive brace globs
    let tests_only = watched
        .call("code_files", json!({"glob": "**/*.test.tsx"}))
        .unwrap();
    assert_eq!(tests_only["paths"], json!(["src/components/Button.test.tsx"]));

    let css_modules = watched
        .call("code_files", json!({"glob": "*.module.css"}))
        .unwrap();
    assert_eq!(css_modules["paths"], json!(["src/components/Button.module.css"]));

    let dts = watched
        .call("code_files", json!({"glob": "./**/*.d.ts"}))
        .unwrap();
    assert_eq!(dts["paths"], json!(["src/components/types.d.ts"]));

    let scoped_brace = watched
        .call("code_files", json!({"glob": "src/components/**/*.{ts,tsx}"}))
        .unwrap();
    assert_eq!(
        scoped_brace["paths"],
        json!([
            "src/components/Button.stories.tsx",
            "src/components/Button.test.tsx",
            "src/components/Button.tsx",
            "src/components/types.d.ts"
        ])
    );

    // Next.js / Remix dynamic route bracket globs
    let dynamic_id = watched
        .call("code_files", json!({"glob": "pages/users/[id].tsx"}))
        .unwrap();
    assert_eq!(dynamic_id["paths"], json!(["pages/users/[id].tsx"]));

    let catch_all = watched
        .call("code_files", json!({"glob": "app/[...slug]/page.tsx"}))
        .unwrap();
    assert_eq!(catch_all["paths"], json!(["app/[...slug]/page.tsx"]));

    // Vite ?react / ?raw query suffix, @/ path alias, and tsc/Vite line:col normalization
    let read_aliased = watched
        .call(
            "code_read",
            json!({
                "ranges": [
                    {"path": "@/components/Button.tsx:1:10", "start": 1, "end": 1},
                    {"path": "@/assets/icon.svg?react", "start": 1, "end": 1},
                    {"path": "src/shaders/wave.wgsl?raw", "start": 1, "end": 1},
                    {"path": "src/styles/theme.pcss(1,5)", "start": 1, "end": 1}
                ]
            }),
        )
        .unwrap();
    assert_eq!(read_aliased[0]["path"], "src/components/Button.tsx");
    assert_eq!(read_aliased[1]["path"], "src/assets/icon.svg");
    assert_eq!(read_aliased[2]["path"], "src/shaders/wave.wgsl");
    assert_eq!(read_aliased[3]["path"], "src/styles/theme.pcss");

    // Comma-separated globs outside braces ("*.tsx, *.svg") and bare stem fallback ("Button")
    let comma_glob = watched
        .call("code_files", json!({"glob": "*.svg, *.wgsl"}))
        .unwrap();
    assert_eq!(
        comma_glob["paths"],
        json!(["src/assets/icon.svg", "src/shaders/wave.wgsl"])
    );

    let bare_stem = watched
        .call("code_files", json!({"glob": "Button"}))
        .unwrap();
    assert_eq!(
        bare_stem["paths"],
        json!([
            "src/components/Button.module.css",
            "src/components/Button.stories.tsx",
            "src/components/Button.test.tsx",
            "src/components/Button.tsx"
        ])
    );

    // Markdown links, backticks, and #L1C5 hash anchors in watched paths
    let read_markdown = watched
        .call(
            "code_read",
            json!({
                "ranges": [
                    {"path": "[Button.tsx](src/components/Button.tsx#L1C5)", "start": 1, "end": 1},
                    {"path": "`src/assets/icon.svg`", "start": 1, "end": 1}
                ]
            }),
        )
        .unwrap();
    assert_eq!(read_markdown[0]["path"], "src/components/Button.tsx");
    assert_eq!(read_markdown[1]["path"], "src/assets/icon.svg");
}
