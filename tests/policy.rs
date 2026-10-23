use code_context::catalog::{Spec, private_dir};
use code_context::watched::Watched;
use serde_json::json;
use std::os::unix::fs::{PermissionsExt, symlink};

#[test]
fn private_state_rejects_public_directories_and_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let public = root.path().join("public");
    std::fs::create_dir(&public).unwrap();
    std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(private_dir(&public).is_err());
    let link = root.path().join("link");
    symlink(root.path(), &link).unwrap();
    assert!(private_dir(&link).is_err());
    let private = root.path().join("private");
    private_dir(&private).unwrap();
    assert_eq!(
        std::fs::metadata(private).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[test]
fn watcher_directory_recreation_recovers_under_periodic_contract() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("src");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("one.rs"), "old\n").unwrap();
    let mut watched = Watched::new(Spec {
        root: root.path().to_path_buf(),
        scopes: vec!["src".into()],
        globs: vec!["*.rs".into()],
        excludes: Vec::new(),
        default_excludes: true,
        prewarm: false,
        policy: Default::default(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();
    std::fs::remove_file(source.join("one.rs")).unwrap();
    std::fs::remove_dir(&source).unwrap();
    assert!(watched.call("code_sync", json!({})).is_err());
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("new.rs"), "new\n").unwrap();
    watched.call("code_sync", json!({})).unwrap();
    assert_eq!(
        watched.call("code_grep", json!({"pattern":"new"})).unwrap()["total_matches"],
        1
    );
}

#[test]
fn symlink_replacement_blocks_then_recovers_without_leaking_target() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    let file = root.path().join("src/one.rs");
    std::fs::write(&file, "source\n").unwrap();
    let mut watched = Watched::new(Spec {
        root: root.path().to_path_buf(),
        scopes: vec!["src".into()],
        globs: vec!["*.rs".into()],
        excludes: Vec::new(),
        default_excludes: true,
        prewarm: false,
        policy: Default::default(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();
    std::fs::remove_file(&file).unwrap();
    symlink("/etc/passwd", &file).unwrap();
    watched.call("code_sync", json!({})).unwrap();
    assert_eq!(
        watched
            .call("code_grep", json!({"pattern":"root:"}))
            .unwrap()["total_matches"],
        0
    );
    assert!(
        watched
            .call(
                "code_read",
                json!({"ranges":[{"path":"src/one.rs","start":1,"end":1}]})
            )
            .is_err()
    );
}
