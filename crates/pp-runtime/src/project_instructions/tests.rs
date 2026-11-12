use super::*;
use std::{fs, os::unix::fs::symlink};

fn directory(root: &Path, name: &str) -> PathBuf {
    let directory = root.join(name);
    fs::create_dir_all(&directory).unwrap();
    directory
}

#[test]
fn project_instructions_git_ancestor_chain_and_ignored_rules() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    directory(&root, ".git");
    let cwd = directory(&root, "crates/runtime/src");
    fs::write(root.join(".gitignore"), "AGENTS.md\n").unwrap();
    fs::write(root.join("AGENTS.md"), "root-rule").unwrap();
    fs::write(root.join("crates/AGENTS.md"), "   ").unwrap();
    fs::write(root.join("crates/runtime/AGENTS.md"), "runtime-rule").unwrap();
    let mut state = State::new(&cwd);
    assert!(state.refresh().unwrap().is_empty());
    state.activate(cwd.clone()).unwrap();
    let documents = state.refresh().unwrap();
    assert_eq!(
        documents
            .iter()
            .map(|d| d.content.as_str())
            .collect::<Vec<_>>(),
        ["root-rule", "runtime-rule"]
    );
    state.sent();
    state.check(cwd.join("new/directory")).unwrap();
}

#[test]
fn project_instructions_read_activation_requires_a_new_request() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let child = directory(&root, "child");
    fs::write(child.join("AGENTS.md"), "child-rule").unwrap();
    let mut state = State::new(&root);
    state.refresh().unwrap();
    state.sent();
    state.activate(child.clone()).unwrap();
    state.refresh().unwrap();
    assert!(state.check(child.clone()).is_err());
    state.sent();
    state.check(child).unwrap();
}

#[test]
fn project_instructions_creation_change_deletion_and_recreation_require_new_requests() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let path = root.join("AGENTS.md");
    let mut state = State::new(&root);
    state.refresh().unwrap();
    state.sent();
    for content in [
        Some("first"),
        Some("second"),
        None,
        Some("second"),
        Some(" "),
    ] {
        if let Some(content) = content {
            fs::write(&path, content).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
        }
        assert!(state.check(root.clone()).is_err());
        state.refresh().unwrap();
        state.sent();
        state.check(root.clone()).unwrap();
    }
}

#[test]
fn project_instructions_external_worktree_nested_repository_and_loose_files() {
    let temporary = tempfile::tempdir().unwrap();
    let parent = temporary.path().canonicalize().unwrap();
    let root = directory(&parent, "main");
    directory(&root, ".git");
    fs::write(root.join("AGENTS.md"), "main-rule").unwrap();
    let nested = directory(&root, "nested");
    fs::write(nested.join(".git"), "gitdir: fixture").unwrap();
    fs::write(nested.join("AGENTS.md"), "nested-rule").unwrap();
    let external = directory(&parent, "external");
    fs::write(external.join(".git"), "gitdir: fixture").unwrap();
    fs::write(external.join("AGENTS.md"), "external-rule").unwrap();
    fs::write(parent.join("AGENTS.md"), "unrelated-parent-rule").unwrap();
    let loose = directory(&parent, "loose");
    fs::write(loose.join("AGENTS.md"), "loose-rule").unwrap();
    let mut state = State::new(&root);
    assert_eq!(
        state
            .chain(&nested)
            .unwrap()
            .iter()
            .map(|d| d.content.as_str())
            .collect::<Vec<_>>(),
        ["nested-rule"]
    );
    state.activate(root.clone()).unwrap();
    state.activate(external.clone()).unwrap();
    state.activate(loose.clone()).unwrap();
    let text = render(&state.refresh().unwrap());
    assert!(text.contains("main-rule") && text.contains("external-rule"));
    assert!(!text.contains("loose-rule") && !text.contains("unrelated-parent-rule"));
    state.sent();
    state.check(external).unwrap();
    state.check(loose).unwrap();
}

#[test]
fn project_instructions_rule_failures_never_become_empty_rules() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let path = root.join("AGENTS.md");
    fs::write(&path, [0xff]).unwrap();
    let mut state = State::new(&root);
    assert!(state.activate(root.clone()).is_err());
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(state.activate(root.clone()).is_err());
    fs::remove_dir(&path).unwrap();
    fs::write(root.join("source"), "must-not-follow").unwrap();
    symlink(root.join("source"), &path).unwrap();
    assert!(state.activate(root.clone()).is_err());
    fs::remove_file(&path).unwrap();
    fs::write(path, "repaired").unwrap();
    state.activate(root).unwrap();
    assert_eq!(state.refresh().unwrap()[0].content, "repaired");
}

#[test]
fn project_instructions_sibling_scopes_do_not_block_each_other() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let a = directory(&root, "a");
    let b = directory(&root, "b");
    fs::write(a.join("AGENTS.md"), "a-rule").unwrap();
    fs::write(b.join("AGENTS.md"), "b-rule").unwrap();
    let mut state = State::new(&root);
    state.activate(a.clone()).unwrap();
    state.refresh().unwrap();
    state.sent();
    state.check(a.clone()).unwrap();
    assert!(state.check(b.clone()).is_err());
    state.refresh().unwrap();
    state.sent();
    fs::write(b.join("AGENTS.md"), "changed-b-rule").unwrap();
    state.check(a).unwrap();
    assert!(state.check(b).is_err());
}

#[test]
fn project_instructions_nested_empty_project_removes_ancestor_rules() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    directory(&root, ".git");
    let child = directory(&root, "child");
    fs::write(root.join("AGENTS.md"), "outer-rule").unwrap();
    let mut state = State::new(&root);
    state.activate(root.clone()).unwrap();
    state.refresh().unwrap();
    state.sent();
    directory(&child, ".git");
    assert!(state.check(child.clone()).is_err());
    state.refresh().unwrap();
    assert!(state.scope_summary().contains(child.to_str().unwrap()));
    state.sent();
    state.check(child).unwrap();
}
