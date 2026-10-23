use code_context::context::{ContextRequest, ReadRange, context, read_many};
use code_context::mcp::{handle, serve};
use code_context::search::{Search, search};
use code_context::workspace::{Workspace, capture};
use serde_json::json;
use std::io::Cursor;
use std::process::Command;

fn fixture() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".ignore"), "ignored.rs\n").unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "alpha\nneedle\nneedle two\nomega\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("한글.rs"), "보리 needle\nlast\n").unwrap();
    std::fs::write(dir.path().join("ignored.rs"), "needle\n").unwrap();
    std::fs::write(dir.path().join(".secret.rs"), "needle\n").unwrap();
    std::fs::write(dir.path().join("outside.txt"), "needle\n").unwrap();
    let workspace = Workspace::new(dir.path(), vec![".".into()], vec!["*.rs".into()]).unwrap();
    (dir, workspace)
}

fn request(pattern: &str) -> Search {
    Search {
        pattern: pattern.into(),
        regex: false,
        ignore_case: false,
        limit: 200,
    }
}

#[test]
fn literal_matches_ignore_rules_unicode_and_count() {
    let (_dir, workspace) = fixture();
    let result = search(&workspace, &request("needle")).unwrap();
    assert_eq!(result.total_matches, 3);
    assert_eq!(result.matches[0].path, "a.rs");
    assert_eq!(result.matches[0].line, 2);
    assert_eq!(result.matches[2].text, "보리 needle");
    assert!(!result.truncated);
}

#[test]
fn overlapping_scopes_do_not_duplicate_matching_lines() {
    let (dir, _) = fixture();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    std::fs::write(dir.path().join("nested/one.rs"), "unique\n").unwrap();
    let workspace = Workspace::new(
        dir.path(),
        vec![".".into(), "nested".into()],
        vec!["*.rs".into()],
    )
    .unwrap();
    let result = search(&workspace, &request("unique")).unwrap();
    assert_eq!(result.total_matches, 1);
    assert_eq!(result.matches[0].path, "nested/one.rs");
}

#[test]
fn regex_case_and_literal_semantics() {
    let (_dir, workspace) = fixture();
    assert!(
        search(&workspace, &request("need.*"))
            .unwrap()
            .matches
            .is_empty()
    );
    let mut input = request("NEED.*");
    input.regex = true;
    input.ignore_case = true;
    assert_eq!(search(&workspace, &input).unwrap().matches.len(), 3);
    input.pattern = "[".into();
    assert!(search(&workspace, &input).is_err());
}

#[test]
fn empty_invalid_limits_and_truncation() {
    let (_dir, workspace) = fixture();
    assert!(search(&workspace, &request("")).is_err());
    let mut input = request("needle");
    input.limit = 0;
    assert!(search(&workspace, &input).is_err());
    input.limit = 1;
    let result = search(&workspace, &input).unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.total_matches, 3);
    assert!(result.truncated);
}

#[test]
fn search_plus_context_merges_and_verifies_lines() {
    let (_dir, workspace) = fixture();
    let result = context(
        &workspace,
        &ContextRequest {
            search: request("needle"),
            radius: 1,
        },
    );
    assert!(result.is_err(), "context limit must be explicitly bounded");
    let mut input = request("needle");
    input.limit = 8;
    let result = context(
        &workspace,
        &ContextRequest {
            search: input,
            radius: 1,
        },
    )
    .unwrap();
    assert_eq!(result.snippets.len(), 2);
    assert_eq!(
        result.snippets[0].lines,
        ["alpha", "needle", "needle two", "omega"]
    );
    assert_eq!(result.snippets[0].sha256.len(), 64);
    assert_eq!(result.snippets[1].lines, ["보리 needle", "last"]);
}

#[test]
fn read_ranges_and_source_versions() {
    let (dir, workspace) = fixture();
    let range = ReadRange {
        path: "a.rs".into(),
        start: 2,
        end: 500,
    };
    let before = read_many(&workspace, std::slice::from_ref(&range)).unwrap();
    assert_eq!(before[0].end, 4);
    std::fs::write(dir.path().join("a.rs"), "changed\nnew\n").unwrap();
    let after = read_many(&workspace, &[range]).unwrap();
    assert_ne!(before[0].sha256, after[0].sha256);
    assert_eq!(after[0].lines, ["new"]);
}

#[test]
fn path_policy_and_ignored_file_reads() {
    let (_dir, workspace) = fixture();
    for path in [
        "../a.rs",
        "/etc/passwd",
        ".secret.rs",
        "ignored.rs",
        "outside.txt",
    ] {
        let range = ReadRange {
            path: path.into(),
            start: 1,
            end: 2,
        };
        assert!(read_many(&workspace, &[range]).is_err(), "{path}");
    }
    for (start, end) in [(0, 1), (3, 2), (1, 501), (100, 100)] {
        assert!(
            read_many(
                &workspace,
                &[ReadRange {
                    path: "a.rs".into(),
                    start,
                    end
                }]
            )
            .is_err()
        );
    }
    assert!(read_many(&workspace, &[]).is_err());
}

#[test]
fn live_search_observes_create_delete_rename_and_ignore_changes() {
    let (dir, workspace) = fixture();
    std::fs::write(dir.path().join("new.rs"), "unique_created\n").unwrap();
    assert_eq!(
        search(&workspace, &request("unique_created"))
            .unwrap()
            .total_matches,
        1
    );
    std::fs::rename(dir.path().join("new.rs"), dir.path().join("renamed.rs")).unwrap();
    assert_eq!(
        search(&workspace, &request("unique_created"))
            .unwrap()
            .matches[0]
            .path,
        "renamed.rs"
    );
    std::fs::remove_file(dir.path().join("renamed.rs")).unwrap();
    assert_eq!(
        search(&workspace, &request("unique_created"))
            .unwrap()
            .total_matches,
        0
    );
    std::fs::write(dir.path().join(".ignore"), "a.rs\nignored.rs\n").unwrap();
    assert_eq!(
        search(&workspace, &request("needle"))
            .unwrap()
            .total_matches,
        1
    );
}

#[cfg(unix)]
#[test]
fn symlinks_rejected_for_scopes_and_reads() {
    use std::os::unix::fs::symlink;
    let (dir, workspace) = fixture();
    symlink("/etc", dir.path().join("linked")).unwrap();
    assert!(Workspace::new(dir.path(), vec!["linked".into()], vec!["*.rs".into()]).is_err());
    symlink(dir.path().join("a.rs"), dir.path().join("link.rs")).unwrap();
    assert!(
        read_many(
            &workspace,
            &[ReadRange {
                path: "link.rs".into(),
                start: 1,
                end: 1
            }]
        )
        .is_err()
    );
}

#[test]
fn binary_and_invalid_utf8_fail_explicitly() {
    let (dir, workspace) = fixture();
    for (name, contents) in [("binary.rs", vec![0, 1, 2]), ("invalid.rs", vec![255, 254])] {
        std::fs::write(dir.path().join(name), contents).unwrap();
        assert!(
            read_many(
                &workspace,
                &[ReadRange {
                    path: name.into(),
                    start: 1,
                    end: 1
                }]
            )
            .is_err()
        );
    }
}

#[test]
fn mcp_initialize_list_call_and_errors() {
    let (_dir, workspace) = fixture();
    let init = handle(
        &workspace,
        json!({"id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}),
    )
    .unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    let listed = handle(&workspace, json!({"id":2,"method":"tools/list"})).unwrap();
    assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 4);
    for tool in ["code_grep", "code_context"] {
        let output = handle(&workspace, json!({"id":3,"method":"tools/call","params":{"name":tool,"arguments":{"pattern":"needle"}}})).unwrap();
        assert_eq!(output["result"]["isError"], false, "{output}");
    }
    let output = handle(&workspace, json!({"id":4,"method":"tools/call","params":{"name":"code_grep","arguments":{"pattern":"needle","ignored":1}}})).unwrap();
    assert_eq!(output["result"]["isError"], true);
    let output = handle(
        &workspace,
        json!({"id":4,"method":"tools/call","params":{"name":"code_context","arguments":{"pattern":"needle","ignored":1}}}),
    ).unwrap();
    assert_eq!(output["result"]["isError"], true);
    let unknown = handle(&workspace, json!({"id":5,"method":"unknown"})).unwrap();
    assert_eq!(unknown["error"]["code"], -32601);
    assert!(handle(&workspace, json!({"method":"notifications/initialized"})).is_none());
}

#[test]
fn mcp_read_contract_and_context_empty_result() {
    let (_dir, workspace) = fixture();
    let result = handle(&workspace, json!({
        "id":1,"method":"tools/call","params":{"name":"code_read",
        "arguments":{"ranges":[{"path":"a.rs","start":1,"end":2},{"path":"한글.rs","start":1,"end":1}]}}
    })).unwrap();
    assert_eq!(result["result"]["isError"], false);
    let snippets: serde_json::Value =
        serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(snippets.as_array().unwrap().len(), 2);
    let empty = context(
        &workspace,
        &ContextRequest {
            search: Search {
                limit: 8,
                ..request("not_present")
            },
            radius: 3,
        },
    )
    .unwrap();
    assert!(empty.snippets.is_empty());
    assert_eq!(empty.search.total_matches, 0);
}

#[test]
fn file_and_response_size_caps_are_explicit_errors() {
    let (dir, workspace) = fixture();
    std::fs::write(dir.path().join("large.rs"), vec![b'x'; 2 * 1024 * 1024 + 1]).unwrap();
    assert!(
        read_many(
            &workspace,
            &[ReadRange {
                path: "large.rs".into(),
                start: 1,
                end: 1
            }]
        )
        .is_err()
    );
    std::fs::write(dir.path().join("longline.rs"), vec![b'x'; 300 * 1024]).unwrap();
    assert!(search(&workspace, &request("xxx")).is_err());
    assert!(
        read_many(
            &workspace,
            &[ReadRange {
                path: "longline.rs".into(),
                start: 1,
                end: 1
            }]
        )
        .is_err()
    );
}

#[test]
fn stdio_messages_remain_json_and_parse_errors_are_visible() {
    let (_dir, workspace) = fixture();
    let input = b"broken\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n";
    let mut output = Vec::new();
    serve(&workspace, Cursor::new(input), &mut output).unwrap();
    let text = String::from_utf8(output).unwrap();
    let responses: Vec<serde_json::Value> = text
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["error"]["code"], -32700);
    assert_eq!(responses[1]["id"], 2);
}

#[test]
fn large_messages_and_subprocess_outputs_fail_explicitly() {
    let (_dir, workspace) = fixture();
    let input = vec![b'a'; code_context::mcp::MAX_MESSAGE + 2];
    assert!(serve(&workspace, Cursor::new(input), Vec::new()).is_err());
    let mut command = Command::new("head");
    command.args(["-c", "8388609", "/dev/zero"]);
    assert!(capture(&mut command).is_err());
}

#[test]
fn crlf_text_and_leading_dash_patterns() {
    let (dir, workspace) = fixture();
    std::fs::write(dir.path().join("crlf.rs"), "-dash\r\nneedle\r\n").unwrap();
    let mut input = request("-dash");
    input.limit = 8;
    let result = context(
        &workspace,
        &ContextRequest {
            search: input,
            radius: 0,
        },
    )
    .unwrap();
    assert_eq!(result.search.matches[0].text, "-dash\r");
}

#[test]
fn root_and_empty_scope_validation() {
    let (dir, _) = fixture();
    assert!(Workspace::new(dir.path(), vec![], vec!["*.rs".into()]).is_err());
    assert!(Workspace::new(dir.path(), vec!["../".into()], vec!["*.rs".into()]).is_err());
    assert!(Workspace::new(dir.path(), vec![".".into()], vec![]).is_err());
}
