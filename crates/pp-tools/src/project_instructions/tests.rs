use super::*;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    time::{Duration, SystemTime},
};

fn directory(root: &Path, name: &str) -> PathBuf {
    let directory = root.join(name);
    fs::create_dir_all(&directory).unwrap();
    directory
}

fn contents(documents: &[Document]) -> Vec<&str> {
    documents
        .iter()
        .map(|document| document.content.as_str())
        .collect()
}

#[test]
fn project_instructions_cache_invalidation_and_error_handling() {
    {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let path = root.join("AGENTS.md");
        fs::write(&path, "root-rule").unwrap();
        let mut state = State::new(&root);
        let first = state.refresh_shared().unwrap();
        assert_eq!(contents(&first), ["root-rule"]);
        // Unchanged sources share one snapshot: no disk re-read and no body rebuild.
        assert!(Arc::ptr_eq(&first, &state.refresh_shared().unwrap()));
        // Dropping read permission leaves mtime, length and inode unchanged: the next read must fail.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(contents(&state.refresh_shared().unwrap()), ["root-rule"]);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        // A body change must be re-read rather than served from the cache.
        fs::write(&path, "updated-rule").unwrap();
        assert_eq!(contents(&state.refresh_shared().unwrap()), ["updated-rule"]);
    }

    {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let path = root.join("AGENTS.md");
        fs::write(&path, "root-rule").unwrap();
        let mut state = State::new(&root);
        assert_eq!(contents(&state.refresh_shared().unwrap()), ["root-rule"]);
        // Touching only the mtime (body, length and inode unchanged) must also trigger a re-read.
        let file = fs::File::options().append(true).open(&path).unwrap();
        file.set_times(
            fs::FileTimes::new()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000)),
        )
        .unwrap();
        drop(file);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        // A failed re-read surfaces as a read error instead of silently passing as "rule missing".
        assert!(state.refresh_shared().is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(contents(&state.refresh_shared().unwrap()), ["root-rule"]);
    }

    {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let child = directory(&root, "child");
        fs::write(root.join("AGENTS.md"), "root-rule").unwrap();
        let path = child.join("AGENTS.md");
        fs::write(&path, "child-rule").unwrap();
        let mut state = State::new(&root);
        state.activate(child.clone()).unwrap();
        assert_eq!(
            contents(&state.refresh_shared().unwrap()),
            ["root-rule", "child-rule"]
        );
        fs::remove_file(&path).unwrap();
        let documents = state.refresh_shared().unwrap();
        // Deleting the child rule removes only that source; the parent rule stays and no empty body block is appended.
        assert_eq!(contents(&documents), ["root-rule"]);
        assert!(!render(&documents).contains(&child.display().to_string()));
    }

    {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let path = root.join("AGENTS.md");
        fs::write(&path, [0xff]).unwrap();
        let mut state = State::new(&root);
        assert!(state.refresh().is_err());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(state.refresh().is_err());
        fs::remove_dir(&path).unwrap();
        fs::write(root.join("source"), "must-not-follow").unwrap();
        symlink(root.join("source"), &path).unwrap();
        assert!(state.refresh().is_err());
        fs::remove_file(&path).unwrap();
        fs::write(path, "repaired").unwrap();
        assert_eq!(state.refresh().unwrap()[0].content, "repaired");
    }
}

#[test]
fn project_instructions_activation_and_request_freshness() {
    {
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

    {
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

    {
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
}

#[test]
fn project_instructions_git_scope_and_worktree_boundaries() {
    {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        directory(&root, ".git");
        let cwd = directory(&root, "crates/runtime/src");
        fs::write(root.join(".gitignore"), "AGENTS.md\n").unwrap();
        fs::write(root.join("AGENTS.md"), "root-rule").unwrap();
        fs::write(root.join("crates/AGENTS.md"), "   ").unwrap();
        fs::write(root.join("crates/runtime/AGENTS.md"), "runtime-rule").unwrap();
        let mut state = State::new(&cwd);
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

    {
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

    {
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
}
