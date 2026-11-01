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
