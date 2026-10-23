use code_context::catalog::Spec;
use code_context::search::{Search, search};
use code_context::watched::Watched;
use serde_json::json;

#[test]
fn line_aware_regex_matches_rg_without_crossing_newlines() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("src")).unwrap();
    std::fs::write(
        directory.path().join("src/a.rs"),
        "alpha\nab\nA B\n한글 hello\n\nlast\r\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("src/b.rs"), "a\nb\nlast").unwrap();
    std::fs::write(directory.path().join("src/empty.rs"), "").unwrap();
    let spec = Spec {
        root: directory.path().to_path_buf(),
        scopes: vec!["src".into()],
        globs: vec!["*.rs".into()],
        excludes: Vec::new(),
        default_excludes: true,
        prewarm: false,
        policy: Default::default(),
        allow_worktrees: false,
        managed_parent: None,
    };
    let workspace = spec.workspace().unwrap();
    let mut watched = Watched::new(spec).unwrap();
    for pattern in [
        r"^",
        r"$",
        r"\A",
        r"\z",
        r"a\s*b",
        r".*",
        r"^last",
        r"hello|alpha",
        r"[^\n]+",
        r"\bhello\b",
        r"^$",
        r"\r$",
        r"(?i)ALPHA",
    ] {
        let expected = search(
            &workspace,
            &Search {
                pattern: pattern.into(),
                regex: true,
                ignore_case: false,
                limit: 1000,
            },
        )
        .unwrap();
        let actual = watched
            .call(
                "code_grep",
                json!({"pattern":pattern,"regex":true,"limit":1000}),
            )
            .unwrap();
        assert_eq!(
            actual["matches"],
            serde_json::to_value(expected.matches).unwrap(),
            "{pattern}"
        );
        assert_eq!(actual["total_matches"], expected.total_matches, "{pattern}");
    }
}
