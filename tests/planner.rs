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
