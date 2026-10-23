use code_context::context::{ContextRequest, ReadRange};
use code_context::engine::{Backend, Engine};
use code_context::mcp::{handle_engine, serve_engine};
use code_context::resident::Resident;
use code_context::search::Search;
use code_context::workspace::Workspace;
use serde_json::{Value, json};
use std::io::Cursor;

fn fixture() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".ignore"), "ignored.rs\n").unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "aaaaa\nneedle needle\n보리 hello\n-tail\r\n\nlast",
    )
    .unwrap();
    std::fs::write(dir.path().join("b.rs"), "abc---bcd\nzneedle\n").unwrap();
    std::fs::write(dir.path().join("empty.rs"), "").unwrap();
    std::fs::write(dir.path().join("ignored.rs"), "needle\n").unwrap();
    std::fs::write(dir.path().join(".hidden.rs"), "needle\n").unwrap();
    let workspace = Workspace::new(dir.path(), vec![".".into()], vec!["*.rs".into()]).unwrap();
    (dir, workspace)
}

fn request(pattern: &str) -> Search {
    Search {
        pattern: pattern.into(),
        regex: false,
        ignore_case: false,
        limit: 8,
    }
}

fn normalize(mut value: Value) -> Value {
    value.as_object_mut().unwrap().remove("consistency");
    if let Some(search) = value.get_mut("search") {
        search.as_object_mut().unwrap().remove("consistency");
    }
    value
}

#[test]
fn both_resident_modes_match_rg_for_edges_and_unicode() {
    let (_dir, workspace) = fixture();
    let live = Engine::new(workspace.clone(), Backend::Live).unwrap();
    for backend in [Backend::Ram, Backend::Index] {
        let resident = Engine::new(workspace.clone(), backend).unwrap();
        for pattern in [
            "a",
            "aa",
            "aaa",
            "needle",
            "보리",
            "리 h",
            "hello",
            "-tail",
            "\r",
            "last",
            "abcd",
            "nonexistent",
            "z",
        ] {
            let input = request(pattern);
            let expected = normalize(serde_json::to_value(live.search(&input).unwrap()).unwrap());
            let actual = normalize(serde_json::to_value(resident.search(&input).unwrap()).unwrap());
            assert_eq!(expected, actual, "{backend:?} {pattern}");
            let input = ContextRequest {
                search: input,
                radius: 1,
            };
            assert_eq!(
                normalize(serde_json::to_value(live.context(&input).unwrap()).unwrap()),
                normalize(serde_json::to_value(resident.context(&input).unwrap()).unwrap()),
                "{backend:?} {pattern}"
            );
        }
    }
}

#[test]
fn index_candidates_are_only_a_filter_not_an_answer_lookup() {
    let (_dir, workspace) = fixture();
    let indexed = Resident::load(&workspace, true, 1).unwrap();
    assert_eq!(
        indexed.candidate_stats("abcd").0,
        1,
        "both trigrams present in b.rs"
    );
    assert_eq!(indexed.search(&request("abcd")).unwrap().total_matches, 0);
    assert_eq!(indexed.candidate_stats("needle").0, 2);
    assert_eq!(indexed.candidate_stats("this-is-absent").0, 0);
}

#[test]
fn deterministic_generated_substrings_match_rg() {
    let (dir, workspace) = fixture();
    let mut text = String::new();
    let mut state = 23_u32;
    for i in 0..2000 {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        text.push((b'a' + (state % 12) as u8) as char);
        if i % 53 == 0 {
            text.push('\n');
        }
    }
    std::fs::write(dir.path().join("generated.rs"), &text).unwrap();
    let live = Engine::new(workspace.clone(), Backend::Live).unwrap();
    let ram = Engine::new(workspace.clone(), Backend::Ram).unwrap();
    let index = Engine::new(workspace, Backend::Index).unwrap();
    for i in (0..text.len() - 8).step_by(23) {
        let pattern = &text[i..i + 7];
        if pattern.contains('\n') {
            continue;
        }
        let input = request(pattern);
        let expected = normalize(serde_json::to_value(live.search(&input).unwrap()).unwrap());
        for engine in [&ram, &index] {
            assert_eq!(
                expected,
                normalize(serde_json::to_value(engine.search(&input).unwrap()).unwrap()),
                "{pattern}"
            );
        }
    }
}

#[test]
fn resident_queries_and_reads_work_after_source_is_removed() {
    let (dir, workspace) = fixture();
    for backend in [Backend::Ram, Backend::Index] {
        let engine = Engine::new(workspace.clone(), backend).unwrap();
        let file = dir.path().join("a.rs");
        let content = std::fs::read(&file).unwrap();
        std::fs::remove_file(&file).unwrap();
        assert_eq!(engine.search(&request("hello")).unwrap().total_matches, 1);
        let snippets = engine
            .read_many(&[ReadRange {
                path: "a.rs".into(),
                start: 3,
                end: 3,
            }])
            .unwrap();
        assert_eq!(snippets[0].lines, ["보리 hello"]);
        std::fs::write(file, content).unwrap();
    }
}

#[test]
fn refresh_rebuilds_create_modify_delete_rename_and_ignore_changes() {
    let (dir, workspace) = fixture();
    for backend in [Backend::Ram, Backend::Index] {
        std::fs::write(dir.path().join(".ignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("a.rs"), "old\n").unwrap();
        let mut engine = Engine::new(workspace.clone(), backend).unwrap();
        std::fs::write(dir.path().join("a.rs"), "fresh\n").unwrap();
        std::fs::write(dir.path().join("new.rs"), "fresh\n").unwrap();
        assert_eq!(engine.search(&request("fresh")).unwrap().total_matches, 0);
        let next = engine.refresh().unwrap();
        assert_eq!(next["stats"]["generation"], 2);
        assert_eq!(engine.search(&request("fresh")).unwrap().total_matches, 2);
        std::fs::rename(dir.path().join("new.rs"), dir.path().join("renamed.rs")).unwrap();
        std::fs::remove_file(dir.path().join("a.rs")).unwrap();
        engine.refresh().unwrap();
        let found = engine.search(&request("fresh")).unwrap();
        assert_eq!(found.matches[0].path, "renamed.rs");
        std::fs::write(dir.path().join(".ignore"), "ignored.rs\nrenamed.rs\n").unwrap();
        engine.refresh().unwrap();
        assert_eq!(engine.search(&request("fresh")).unwrap().total_matches, 0);
        std::fs::remove_file(dir.path().join("renamed.rs")).unwrap();
    }
}

#[test]
fn failed_refresh_keeps_old_generation_and_returns_an_error() {
    let (dir, workspace) = fixture();
    let mut engine = Engine::new(workspace, Backend::Index).unwrap();
    std::fs::write(dir.path().join("bad.rs"), [255]).unwrap();
    assert!(engine.refresh().is_err());
    assert_eq!(engine.status()["stats"]["generation"], 1);
    assert_eq!(engine.search(&request("needle")).unwrap().total_matches, 2);
}

#[test]
fn resident_input_limits_and_read_policy_are_explicit() {
    let (_dir, workspace) = fixture();
    let engine = Engine::new(workspace, Backend::Index).unwrap();
    for pattern in ["", "a\nb", "a\0b"] {
        assert!(engine.search(&request(pattern)).is_err());
    }
    let mut input = request("needle");
    input.regex = true;
    assert!(engine.search(&input).is_err());
    input.regex = false;
    input.ignore_case = true;
    assert!(engine.search(&input).is_err());
    for limit in [0, 1001] {
        assert!(
            engine
                .search(&Search {
                    limit,
                    ..request("needle")
                })
                .is_err()
        );
    }
    for path in [
        "../a.rs",
        "/etc/passwd",
        ".hidden.rs",
        "ignored.rs",
        "unknown.rs",
        "empty.rs",
    ] {
        assert!(
            engine
                .read_many(&[ReadRange {
                    path: path.into(),
                    start: 1,
                    end: 1
                }])
                .is_err(),
            "{path}"
        );
    }
    assert!(engine.read_many(&[]).is_err());
    assert!(
        engine
            .read_many(&[ReadRange {
                path: "a.rs".into(),
                start: 0,
                end: 1
            }])
            .is_err()
    );
}

#[test]
fn total_count_and_truncation_agree_for_repeated_and_many_matches() {
    let (_dir, workspace) = fixture();
    for backend in [Backend::Ram, Backend::Index] {
        let engine = Engine::new(workspace.clone(), backend).unwrap();
        let result = engine
            .search(&Search {
                limit: 1,
                ..request("needle")
            })
            .unwrap();
        assert_eq!(result.total_matches, 2);
        assert_eq!(result.matches.len(), 1);
        assert!(result.truncated);
    }
}

#[test]
fn very_long_literal_uses_bounded_posting_intersections_without_false_negatives() {
    let (dir, workspace) = fixture();
    let pattern = "abcdefghijklmnopqrstuvwxyz".repeat(64);
    std::fs::write(dir.path().join("long.rs"), format!("{pattern}\n")).unwrap();
    let live = Engine::new(workspace.clone(), Backend::Live).unwrap();
    let indexed = Engine::new(workspace, Backend::Index).unwrap();
    let input = Search {
        pattern,
        regex: false,
        ignore_case: false,
        limit: 8,
    };
    assert_eq!(
        live.search(&input).unwrap().matches,
        indexed.search(&input).unwrap().matches
    );
}

#[test]
fn mcp_resident_lists_accurate_capabilities_and_refreshes() {
    let (_dir, workspace) = fixture();
    let mut engine = Engine::new(workspace, Backend::Index).unwrap();
    let listed = handle_engine(&mut engine, json!({"id":1,"method":"tools/list"})).unwrap();
    assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 6);
    assert!(
        listed["result"]["tools"][0]["inputSchema"]["properties"]
            .get("regex")
            .is_none()
    );
    for (name, arguments) in [
        ("code_grep", json!({"pattern":"needle"})),
        ("code_context", json!({"pattern":"needle"})),
        ("code_status", json!({})),
        ("code_refresh", json!({})),
    ] {
        let value = handle_engine(
            &mut engine,
            json!({"id":2,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
        )
        .unwrap();
        assert_eq!(value["result"]["isError"], false, "{value}");
    }
    let mut output = Vec::new();
    serve_engine(
        &mut engine,
        Cursor::new(b"{\"id\":3,\"method\":\"ping\"}\n"),
        &mut output,
    )
    .unwrap();
    assert!(
        serde_json::from_slice::<Value>(&output)
            .unwrap()
            .get("result")
            .is_some()
    );
}
