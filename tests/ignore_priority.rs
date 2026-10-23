use code_context::workspace::Workspace;

#[test]
fn root_ignore_has_priority_over_root_gitignore() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("src")).unwrap();
    std::fs::write(directory.path().join("src/one.rs"), "source\n").unwrap();
    std::fs::write(directory.path().join(".gitignore"), "src/one.rs\n").unwrap();
    std::fs::write(directory.path().join(".ignore"), "!src/one.rs\n").unwrap();
    let workspace =
        Workspace::new(directory.path(), vec!["src".into()], vec!["*.rs".into()]).unwrap();
    assert_eq!(workspace.files().unwrap().len(), 1);
    std::fs::write(directory.path().join(".gitignore"), "!src/one.rs\n").unwrap();
    std::fs::write(directory.path().join(".ignore"), "src/one.rs\n").unwrap();
    assert_eq!(workspace.files().unwrap().len(), 0);
}
