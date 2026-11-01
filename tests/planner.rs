use code_context::query::{Query, QueryCache, search_fast, search_profiled};
use code_context::resident::Resident;
use code_context::search::{Search, search};
use code_context::workspace::Workspace;

fn fixture() -> (tempfile::TempDir, Workspace, Resident) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("src")).unwrap();
    std::fs::write(
        directory.path().join("src/a.rs"),
        "foo middle bar\nquux end\nasync def work():\nKERNEL\nKernel\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("src/b.rs"),
        "foo only\n12345\nbar only\nunrelated\n",
    )
    .unwrap();
    for index in 0..40 {
        std::fs::write(
            directory.path().join(format!("src/noise{index}.rs")),
            format!("noise_{index}\n"),
        )
        .unwrap();
    }
    let workspace =
        Workspace::new(directory.path(), vec!["src".into()], vec!["*.rs".into()]).unwrap();
    let resident = Resident::load(&workspace, true, 1).unwrap();
    (directory, workspace, resident)
}

fn query(pattern: &str, regex: bool, ignore_case: bool) -> Query {
    Query {
        pattern: pattern.into(),
        regex,
        ignore_case,
        paths: Vec::new(),
        glob: None,
        limit: 1000,
        max_matches_per_file: None,
    }
}

fn compare(workspace: &Workspace, resident: &Resident, query: &Query) {
    let expected = search(
        workspace,
        &Search {
            pattern: query.pattern.clone(),
            regex: query.regex,
            ignore_case: query.ignore_case,
            limit: query.limit,
        },
    )
    .unwrap();
    let actual = search_profiled(resident, query, &mut QueryCache::default()).unwrap();
    assert_eq!(actual.result.matches, expected.matches, "{}", query.pattern);
    assert_eq!(
        actual.result.total_matches, expected.total_matches,
        "{}",
        query.pattern
    );
    assert_eq!(actual.result.truncated, expected.truncated);
}

#[test]
fn planned_regex_candidates_never_change_results() {
    let (_directory, workspace, resident) = fixture();
    for input in [
        query("foo.*bar", true, false),
        query("(?:foo|quux)", true, false),
        query("(?:foo|[0-9]+)", true, false),
        query("(?:foo)?bar", true, false),
        query("^async def ", true, false),
        query("KERNEL", false, true),
        query(".*", true, false),
        query("^$", true, false),
    ] {
        compare(&workspace, &resident, &input);
    }
}

#[test]
fn selective_regex_uses_index_and_weak_regex_falls_back() {
    let (_directory, _workspace, resident) = fixture();
    let mut cache = QueryCache::default();
    let selective =
        search_profiled(&resident, &query("foo.*bar", true, false), &mut cache).unwrap();
    assert!(selective.profile.used_index, "{:?}", selective.profile);
    assert!(selective.profile.candidate_files < selective.profile.eligible_files);
    let weak =
        search_profiled(&resident, &query("(?:foo|[0-9]+)", true, false), &mut cache).unwrap();
    assert!(!weak.profile.used_index);
    assert_eq!(weak.profile.candidate_files, weak.profile.eligible_files);
}

#[test]
fn compiled_matcher_cache_is_bounded_and_observable() {
    let (_directory, _workspace, resident) = fixture();
    let mut cache = QueryCache::default();
    let input = query("^async def ", true, false);
    let first = search_profiled(&resident, &input, &mut cache).unwrap();
    let second = search_profiled(&resident, &input, &mut cache).unwrap();
    assert!(!first.profile.cache_hit);
    assert!(second.profile.cache_hit);
    assert_eq!(second.profile.compile_us, 0);
    for index in 0..80 {
        let input = query(&format!("noise_{index}"), false, false);
        search_profiled(&resident, &input, &mut cache).unwrap();
    }
    let evicted = search_profiled(&resident, &input, &mut cache).unwrap();
    assert!(!evicted.profile.cache_hit);
}

#[test]
fn unicode_case_equivalents_are_not_filtered_out() {
    let (_directory, workspace, resident) = fixture();
    let input = query("KERNEL", false, true);
    compare(&workspace, &resident, &input);
    let result = search_profiled(&resident, &input, &mut QueryCache::default()).unwrap();
    assert_eq!(result.result.total_matches, 2);
    assert!(result.profile.used_index, "{:?}", result.profile);
    assert!(result.profile.candidate_files < result.profile.eligible_files);
}

#[test]
fn max_matches_per_file_caps_noisy_files_and_prioritizes_definitions() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("src")).unwrap();
    std::fs::write(
        directory.path().join("src/a_calls.rs"),
        "target_sym();\ntarget_sym();\ntarget_sym();\npub fn target_sym() {}\ntarget_sym();\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("src/b_other.rs"),
        "pub struct target_sym;\ntarget_sym();\n",
    )
    .unwrap();
    let workspace =
        Workspace::new(directory.path(), vec!["src".into()], vec!["*.rs".into()]).unwrap();
    let resident = Resident::load(&workspace, true, 1).unwrap();
    let mut cache = QueryCache::default();
    let mut input = query("target_sym", false, false);
    input.limit = 4;
    input.max_matches_per_file = Some(2);
    let result = search_fast(&resident, &input, &mut cache).unwrap();
    assert_eq!(result.total_matches, 7);
    assert!(result.truncated);
    assert_eq!(result.matches.len(), 4);
    assert!(
        result
            .matches
            .iter()
            .any(|hit| hit.path == "src/a_calls.rs" && hit.line == 4)
    );
    assert_eq!(
        result
            .matches
            .iter()
            .filter(|hit| hit.path == "src/b_other.rs")
            .count(),
        2
    );
}

#[test]
fn large_unplannable_regex_uses_parallel_fallback_without_reordering() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("src")).unwrap();
    let content = format!("{}\n{}\n", "a".repeat(1023), "#".repeat(63 * 1024));
    for index in 0..160 {
        std::fs::write(
            directory.path().join(format!("src/file{index:03}.rs")),
            &content,
        )
        .unwrap();
    }

    let workspace =
        Workspace::new(directory.path(), vec!["src".into()], vec!["*.rs".into()]).unwrap();
    let resident = Resident::load(&workspace, true, 1).unwrap();
    let input = query(r"^\w+$", true, false);
    let expected = search(
        &workspace,
        &Search {
            pattern: input.pattern.clone(),
            regex: true,
            ignore_case: false,
            limit: 1000,
        },
    )
    .unwrap();
    let actual = search_profiled(&resident, &input, &mut QueryCache::default()).unwrap();
    assert_eq!(actual.result.matches, expected.matches);
    assert_eq!(actual.result.total_matches, expected.total_matches);
    if std::thread::available_parallelism().is_ok_and(|workers| workers.get() > 1) {
        assert!(actual.profile.parallel_workers > 1);
    }
}

#[test]
fn result_cache_is_bounded_and_generation_aware() {
    let (_directory, workspace, resident) = fixture();
    let mut cache = QueryCache::default();
    let input = query("foo", false, false);
    let first = search_fast(&resident, &input, &mut cache).unwrap();
    let second = search_fast(&resident, &input, &mut cache).unwrap();
    assert_eq!(first.matches, second.matches);
    assert_eq!(cache.stats()["result_entries"], 1);

    let next_generation = Resident::load(&workspace, true, 2).unwrap();
    search_fast(&next_generation, &input, &mut cache).unwrap();
    assert_eq!(cache.stats()["result_entries"], 2);

    for index in 0..40 {
        search_fast(
            &resident,
            &query(&format!("noise_{index}"), false, false),
            &mut cache,
        )
        .unwrap();
    }
    assert!(cache.stats()["result_entries"].as_u64().unwrap() <= 32);
}

#[test]
fn max_matches_per_file_retains_front_and_back_anchors_with_definitions() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("src")).unwrap();
    let mut lines = Vec::new();
    for i in 1..=10 {
        if i == 5 {
            lines.push("pub fn anchor_sym() {}".to_string());
        } else {
            lines.push(format!("let _ = anchor_sym({i});"));
        }
    }
    std::fs::write(
        directory.path().join("src/anchors.rs"),
        lines.join("\n") + "\n",
    )
    .unwrap();
    let workspace =
        Workspace::new(directory.path(), vec!["src".into()], vec!["*.rs".into()]).unwrap();
    let resident = Resident::load(&workspace, true, 1).unwrap();
    let mut cache = QueryCache::default();
    let mut input = query("anchor_sym", false, false);
    input.limit = 4;
    input.max_matches_per_file = Some(4);
    let result = search_fast(&resident, &input, &mut cache).unwrap();
    let matched_lines: Vec<usize> = result.matches.iter().map(|h| h.line).collect();
    assert!(matched_lines.contains(&1), "front anchor retained: {matched_lines:?}");
    assert!(matched_lines.contains(&5), "definition retained: {matched_lines:?}");
    assert!(matched_lines.contains(&10), "back anchor retained: {matched_lines:?}");
}

#[test]
fn leading_repetition_case_insensitive_regex_prefilters_and_matches_oracle() {
    let (_directory, workspace, resident) = fixture();
    for pattern in [r"^\s*async\s+def\s+work", r"^\w+.*KERNEL", r"^.*foo.*bar"] {
        compare(&workspace, &resident, &query(pattern, true, true));
    }
}

#[test]
fn react_vite_definitions_barrels_jsx_tags_bom_and_quote_alias_planner_work() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("src/components")).unwrap();
    std::fs::create_dir_all(directory.path().join("src/styles")).unwrap();

    // 1. Component file with UTF-8 BOM, multi-line imports, barrel re-export, local const,
    //    exported React.memo/forwardRef component, custom hook, JSX opening/closing tags, and console.log
    let component_source = concat!(
        "\u{feff}/// <reference types=\"vite/client\" />\n",
        "import {\n",
        "  useState,\n",
        "  useEffect,\n",
        "} from 'react';\n",
        "import { helper } from '@/utils/ButtonHelper';\n",
        "export { ButtonGroup } from './ButtonGroup';\n",
        "function internalSetup() {\n",
        "  const Button = helper();\n",
        "  console.log(Button);\n",
        "  return Button;\n",
        "}\n",
        "\n",
        "// Primary UI button component\n",
        "export const Button = React.memo(forwardRef<HTMLButtonElement, ButtonProps>((props, ref) => {\n",
        "  const [active, setActive] = useState(false);\n",
        "  return (\n",
        "    <ButtonWrapper variant={props.variant}>\n",
        "      <button ref={ref}>{props.label}</button>\n",
        "    </ButtonWrapper>\n",
        "  );\n",
        "}));\n",
        "\n",
        "export function useButtonState() {\n",
        "  return true;\n",
        "}\n"
    );
    std::fs::write(
        directory.path().join("src/components/Button.tsx"),
        component_source,
    )
    .unwrap();

    // 2. Vue SFC file to verify <script setup> / <template> / <style> section boundary clamping
    let vue_source = concat!(
        "<script setup lang=\"ts\">\n",
        "import { ref } from 'vue';\n",
        "const count = ref(0);\n",
        "export function incrementCounter() {\n",
        "  count.value += 1;\n",
        "}\n",
        "</script>\n",
        "<template>\n",
        "  <button @click=\"incrementCounter\">{{ count }}</button>\n",
        "</template>\n",
        "<style scoped>\n",
        ".btn { color: blue; }\n",
        "</style>\n"
    );
    std::fs::write(
        directory.path().join("src/components/Counter.vue"),
        vue_source,
    )
    .unwrap();

    // 3. CSS file with @keyframes and @layer
    std::fs::write(
        directory.path().join("src/styles/app.css"),
        "@import \"tailwindcss\";\n@layer components {\n  .btn { display: inline-flex; }\n}\n@keyframes pulseSpin {\n  from { opacity: 0; }\n  to { opacity: 1; }\n}\n",
    )
    .unwrap();

    for index in 0..30 {
        std::fs::write(
            directory.path().join(format!("src/noise_{index}.ts")),
            format!("export const noise_{index} = {index};\n"),
        )
        .unwrap();
    }

    let workspace = Workspace::new(
        directory.path(),
        vec!["src".into()],
        vec!["*.tsx".into(), "*.ts".into(), "*.vue".into(), "*.css".into()],
    )
    .unwrap();
    let resident = Resident::load(&workspace, true, 1).unwrap();
    let mut cache = QueryCache::default();

    // Verify 3-way quote + @/ alias regex uses the trigram index
    let alias_query = query(r#"from\s+['"`]@/"#, true, false);
    let alias_profiled = search_profiled(&resident, &alias_query, &mut cache).unwrap();
    assert!(alias_profiled.profile.used_index, "{:?}", alias_profiled.profile);
    assert_eq!(alias_profiled.result.total_matches, 1);

    // Verify max_matches_per_file=1 on Button.tsx picks the top-level exported component
    // (line 15) rather than the barrel re-export (line 7) or indented local const (line 9)
    let mut btn_query = query("Button", false, false);
    btn_query.paths = vec!["src/components/Button.tsx".into()];
    btn_query.max_matches_per_file = Some(1);
    let btn_result = search_fast(&resident, &btn_query, &mut cache).unwrap();
    assert_eq!(btn_result.matches.len(), 1);
    assert!(
        btn_result.matches[0]
            .text
            .starts_with("export const Button = React.memo"),
        "expected exported component definition first, got: {:?}",
        btn_result.matches[0]
    );

    // Verify max_matches_per_file=1 on ButtonWrapper picks opening JSX tag with props over closing tag
    let mut jsx_query = query("ButtonWrapper", false, false);
    jsx_query.paths = vec!["src/components/Button.tsx".into()];
    jsx_query.max_matches_per_file = Some(1);
    let jsx_result = search_fast(&resident, &jsx_query, &mut cache).unwrap();
    assert_eq!(jsx_result.matches.len(), 1);
    assert!(
        jsx_result.matches[0].text.contains("<ButtonWrapper variant="),
        "expected opening JSX tag, got: {:?}",
        jsx_result.matches[0]
    );

    // Verify multi-entry eligible cache handles alternating .tsx / .ts / .css globs
    for glob_pat in ["*.tsx", "*.ts", "*.css", "*.tsx", "*.ts"] {
        let mut q = query("export", false, false);
        q.glob = Some(glob_pat.into());
        let _ = search_profiled(&resident, &q, &mut cache).unwrap();
    }

    // Verify context clamping on React component (`}));` closure + leading `//` comment)
    // and Vue SFC (`<script setup>` / `</script>` boundary)
    let mut watched = code_context::watched::Watched::new(code_context::catalog::Spec {
        root: directory.path().to_path_buf(),
        scopes: vec!["src".into()],
        globs: vec!["*.tsx".into(), "*.ts".into(), "*.vue".into(), "*.css".into()],
        excludes: vec![],
        default_excludes: true,
        prewarm: false,
        policy: Default::default(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();

    let ctx_react = watched
        .call(
            "code_context",
            serde_json::json!({"pattern": "setActive", "radius": 10}),
        )
        .unwrap();
    let react_lines: Vec<&str> = ctx_react["snippets"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        react_lines
            .iter()
            .any(|l| l.contains("// Primary UI button component")),
        "leading // comment preserved: {react_lines:?}"
    );
    assert_eq!(
        react_lines.last().copied(),
        Some("}));"),
        "clamped at }})); closure: {react_lines:?}"
    );

    let ctx_vue = watched
        .call(
            "code_context",
            serde_json::json!({"pattern": "count.value += 1", "radius": 12}),
        )
        .unwrap();
    let vue_lines: Vec<&str> = ctx_vue["snippets"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        !vue_lines.iter().any(|l| l.contains("<template>")),
        "Vue script snippet should not bleed into <template>: {vue_lines:?}"
    );
}

#[test]
fn polyglot_definitions_block_clamping_comment_demotion_and_utf8_ci_planner_work() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("src")).unwrap();

    // 1. Rust file with comment-only mention, usage, and pub(in ...) extern "C" fn definition
    std::fs::write(
        directory.path().join("src/ffi.rs"),
        concat!(
            "// ffi_bridge_symbol is mentioned in this top comment\n",
            "fn caller() {\n",
            "    ffi_bridge_symbol();\n",
            "}\n",
            "\n",
            "/// FFI entrypoint doc\n",
            "pub(in crate::ffi) unsafe extern \"C\" fn ffi_bridge_symbol() -> i32 {\n",
            "    let 보리_kernel_flag = 42;\n",
            "    보리_kernel_flag\n",
            "}\n",
        ),
    )
    .unwrap();

    // 2. SQL file with multiple statements
    std::fs::write(
        directory.path().join("src/schema.sql"),
        concat!(
            "CREATE TABLE accounts (\n",
            "    id BIGINT PRIMARY KEY\n",
            ");\n",
            "\n",
            "-- Audit table\n",
            "CREATE TABLE audit_events (\n",
            "    event_id BIGINT PRIMARY KEY,\n",
            "    audit_payload TEXT NOT NULL\n",
            ");\n",
            "\n",
            "CREATE INDEX idx_audit ON audit_events(event_id);\n",
        ),
    )
    .unwrap();

    // 3. Ruby / Elixir style `end` block file
    std::fs::write(
        directory.path().join("src/worker.rb"),
        concat!(
            "class LegacyWorker\n",
            "  def noop\n",
            "  end\n",
            "end\n",
            "\n",
            "def process_invoice_batch(items)\n",
            "  invoice_marker = items.count\n",
            "  invoice_marker\n",
            "end\n",
            "\n",
            "def after_batch\n",
            "end\n",
        ),
    )
    .unwrap();

    // 4. Makefile & Dockerfile
    std::fs::write(
        directory.path().join("src/Makefile"),
        concat!(
            "clean:\n",
            "\trm -rf target\n",
            "\n",
            "# Build release binary\n",
            "release-bundle:\n",
            "\tcargo build --release --bin cctx_special\n",
            "\tstrip target/release/cctx_special\n",
            "\n",
            "test:\n",
            "\tcargo test\n",
        ),
    )
    .unwrap();

    std::fs::write(
        directory.path().join("src/Dockerfile"),
        concat!(
            "FROM rust:1.85-alpine AS builder\n",
            "WORKDIR /app\n",
            "RUN cargo build --release --bin docker_stage_target\n",
            "\n",
            "FROM alpine:3.21 AS runtime\n",
            "COPY --from=builder /app/target/release/cctx /usr/local/bin/cctx\n",
        ),
    )
    .unwrap();

    for index in 0..25 {
        std::fs::write(
            directory.path().join(format!("src/pad_{index}.rs")),
            format!("pub fn pad_{index}() -> usize {{ {index} }}\n"),
        )
        .unwrap();
    }

    let workspace = Workspace::new(
        directory.path(),
        vec!["src".into()],
        vec![
            "*.rs".into(),
            "*.sql".into(),
            "*.rb".into(),
            "Makefile".into(),
            "Dockerfile".into(),
        ],
    )
    .unwrap();
    let resident = Resident::load(&workspace, true, 1).unwrap();
    let mut cache = QueryCache::default();

    // Verify UTF-8 + ASCII case-insensitive regex uses trigram index
    let utf8_ci = query(r"보리_KERNEL_flag", true, true);
    let utf8_profiled = search_profiled(&resident, &utf8_ci, &mut cache).unwrap();
    assert!(
        utf8_profiled.profile.used_index,
        "UTF-8 + ASCII ignore_case regex should use trigram index: {:?}",
        utf8_profiled.profile
    );
    assert_eq!(utf8_profiled.result.total_matches, 2);

    // Verify non-capturing group + word boundary + small class uses trigram index
    let grouped_q = query(r"\b(?:ffi)[-_]bridge_symbol\b", true, true);
    let grouped_prof = search_profiled(&resident, &grouped_q, &mut cache).unwrap();
    assert!(
        grouped_prof.profile.used_index,
        "grouped small-class ignore_case regex should use trigram index: {:?}",
        grouped_prof.profile
    );

    // Verify max_matches_per_file=1 selects `pub(in crate::ffi) unsafe extern "C" fn` over top comment
    let mut ffi_q = query("ffi_bridge_symbol", false, false);
    ffi_q.paths = vec!["src/ffi.rs:1-10".into()];
    ffi_q.max_matches_per_file = Some(1);
    let ffi_res = search_fast(&resident, &ffi_q, &mut cache).unwrap();
    assert_eq!(ffi_res.matches.len(), 1);
    assert!(
        ffi_res.matches[0].text.contains("extern \"C\" fn ffi_bridge_symbol"),
        "expected FFI definition line, got: {:?}",
        ffi_res.matches[0]
    );

    // Verify SQL, Ruby `end`, Makefile target, and Dockerfile stage block clamping
    let mut watched = code_context::watched::Watched::new(code_context::catalog::Spec {
        root: directory.path().to_path_buf(),
        scopes: vec!["src".into()],
        globs: vec![
            "*.rs".into(),
            "*.sql".into(),
            "*.rb".into(),
            "Makefile".into(),
            "Dockerfile".into(),
        ],
        excludes: vec![],
        default_excludes: true,
        prewarm: false,
        policy: Default::default(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();

    let sql_ctx = watched
        .call(
            "code_context",
            serde_json::json!({"pattern": "audit_payload", "radius": 10}),
        )
        .unwrap();
    let sql_lines: Vec<&str> = sql_ctx["snippets"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(sql_lines.first().copied(), Some("-- Audit table"));
    assert_eq!(sql_lines.last().copied(), Some(");"));

    let rb_ctx = watched
        .call(
            "code_context",
            serde_json::json!({"pattern": "invoice_marker = items.count", "radius": 10}),
        )
        .unwrap();
    let rb_lines: Vec<&str> = rb_ctx["snippets"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(rb_lines.first().copied(), Some("def process_invoice_batch(items)"));
    assert_eq!(rb_lines.last().copied(), Some("end"));

    let mk_ctx = watched
        .call(
            "code_context",
            serde_json::json!({"pattern": "cctx_special", "radius": 10}),
        )
        .unwrap();
    let mk_lines: Vec<&str> = mk_ctx["snippets"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(mk_lines.first().copied(), Some("# Build release binary"));
    assert_eq!(mk_lines.last().copied(), Some("\tstrip target/release/cctx_special"));

    let df_ctx = watched
        .call(
            "code_context",
            serde_json::json!({"pattern": "docker_stage_target", "radius": 10}),
        )
        .unwrap();
    let df_lines: Vec<&str> = df_ctx["snippets"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        df_lines.first().copied(),
        Some("FROM rust:1.85-alpine AS builder")
    );
    assert!(
        !df_lines.iter().any(|l| l.contains("FROM alpine")),
        "Dockerfile stage should not bleed into next FROM stage: {df_lines:?}"
    );
}

#[test]
fn first_try_exact_edit_clamping_preserves_multiline_signatures_structs_overloads_and_regex_hints() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("src")).unwrap();

    // 1. Multi-line #[derive] + doc comment + struct with 8 fields + multi-line fn signature with inner if block
    std::fs::write(
        directory.path().join("src/service.rs"),
        concat!(
            "#[derive(\n",
            "    Clone,\n",
            "    Debug,\n",
            ")]\n",
            "/// Configuration for edit-safe service\n",
            "pub struct EditSafeServiceConfig {\n",
            "    pub host: String,\n",
            "    pub port: u16,\n",
            "    pub max_retries: usize,\n",
            "    pub timeout_ms: u64,\n",
            "    pub tls_enabled: bool,\n",
            "    pub region: String,\n",
            "}\n",
            "\n",
            "#[tokio::test]\n",
            "async fn verify_multiline_sig(\n",
            "    first_arg: usize,\n",
            "    second_arg: &str,\n",
            ") -> Result<usize, String> {\n",
            "    if first_arg == 0 {\n",
            "        return Err(\"zero\".into());\n",
            "    }\n",
            "    Ok(first_arg + second_arg.len())\n",
            "}\n",
        ),
    )
    .unwrap();

    // 2. TypeScript overload signatures followed by implementation
    std::fs::write(
        directory.path().join("src/overloads.ts"),
        concat!(
            "export function parseTokenPayload(raw: string): object;\n",
            "export function parseTokenPayload(raw: Uint8Array): object;\n",
            "export function parseTokenPayload(raw: string | Uint8Array): object {\n",
            "  return typeof raw === 'string' ? JSON.parse(raw) : {};\n",
            "}\n",
        ),
    )
    .unwrap();

    let mut watched = code_context::watched::Watched::new(code_context::catalog::Spec {
        root: directory.path().to_path_buf(),
        scopes: vec!["src".into()],
        globs: vec!["*.rs".into(), "*.ts".into()],
        excludes: vec![],
        default_excludes: true,
        prewarm: false,
        policy: Default::default(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();

    // Struct definition lookup preserves multi-line #[derive], doc comment, and all 6 fields through closing `}`
    let struct_ctx = watched
        .call(
            "code_context",
            serde_json::json!({"pattern": "EditSafeServiceConfig", "radius": 10}),
        )
        .unwrap();
    let struct_lines: Vec<&str> = struct_ctx["snippets"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(struct_lines.first().copied(), Some("#[derive("));
    assert_eq!(struct_lines.last().copied(), Some("}"));
    assert!(struct_lines.iter().any(|l| l.contains("pub region: String,")));

    // Multi-line function signature hit extends through closing `) -> Result<usize, String> {` and body without inner `}` truncation
    let fn_ctx = watched
        .call(
            "code_context",
            serde_json::json!({"pattern": "verify_multiline_sig", "radius": 10}),
        )
        .unwrap();
    let fn_lines: Vec<&str> = fn_ctx["snippets"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(fn_lines.first().copied(), Some("#[tokio::test]"));
    assert!(
        fn_lines.iter().any(|l| l.contains(") -> Result<usize, String> {")),
        "multi-line signature return type preserved: {fn_lines:?}"
    );
    assert!(
        fn_lines.iter().any(|l| l.contains("Ok(first_arg + second_arg.len())")),
        "inner if-block closing brace must not truncate outer function: {fn_lines:?}"
    );

    // TypeScript overload signature hit does not prematurely stop before implementation
    let ts_ctx = watched
        .call(
            "code_context",
            serde_json::json!({"pattern": "parseTokenPayload", "radius": 8}),
        )
        .unwrap();
    let ts_lines: Vec<&str> = ts_ctx["snippets"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        ts_lines.iter().any(|l| l.contains("JSON.parse(raw)")),
        "TS overload + implementation body preserved together: {ts_lines:?}"
    );

    // Unintended literal regex search (0 matches with regex=false) emits helpful regex=true hint
    let zero_regex = watched
        .call(
            "code_grep",
            serde_json::json!({"pattern": "pub\\s+struct\\s+EditSafeServiceConfig", "regex": false}),
        )
        .unwrap();
    assert_eq!(zero_regex["total_matches"], 0);
    assert!(
        zero_regex["freshness"]["notice"]
            .as_str()
            .unwrap()
            .contains("regex=true"),
        "expected regex=true hint in notice: {:?}",
        zero_regex["freshness"]
    );
}
