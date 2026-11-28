use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

fn outside_dir() -> tempfile::TempDir {
    tempfile::Builder::new().tempdir_in("/var/tmp").unwrap()
}

#[derive(Debug, Clone, Copy)]
enum Fault {
    Write,
    Sync,
    Commit,
    CommitNew,
    Directory,
}
struct FaultSave(Fault);
fn failure() -> io::Error {
    io::Error::other("injected save failure")
}
impl SaveOperations for FaultSave {
    fn write(&self, file: &mut File, content: &[u8]) -> io::Result<()> {
        if matches!(self.0, Fault::Write) {
            file.write_all(&content[..content.len() / 2])?;
            return Err(failure());
        }
        SystemSave.write(file, content)
    }
    fn sync_file(&self, file: &File) -> io::Result<()> {
        if matches!(self.0, Fault::Sync) {
            return Err(failure());
        }
        SystemSave.sync_file(file)
    }
    fn commit(&self, parent: &File, temporary: &OsStr, target: &OsStr) -> io::Result<()> {
        if matches!(self.0, Fault::Commit) {
            return Err(failure());
        }
        SystemSave.commit(parent, temporary, target)
    }
    fn commit_new(&self, parent: &File, temporary: &OsStr, target: &OsStr) -> io::Result<()> {
        if matches!(self.0, Fault::CommitNew) {
            return Err(failure());
        }
        SystemSave.commit_new(parent, temporary, target)
    }
    fn sync_directory(&self, parent: &File) -> io::Result<()> {
        if matches!(self.0, Fault::Directory) {
            return Err(failure());
        }
        SystemSave.sync_directory(parent)
    }
}

struct MutateAfterSync<'a> {
    path: &'a Path,
    content: &'a [u8],
}
impl SaveOperations for MutateAfterSync<'_> {
    fn sync_file(&self, file: &File) -> io::Result<()> {
        SystemSave.sync_file(file)?;
        std::fs::write(self.path, self.content)
    }
}

#[test]
fn compare_bytes_and_target_resolution() {
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target");
        let workspace = Workspace::open(dir.path()).unwrap();
        let target = workspace.target(Path::new("target"), false).unwrap();
        for length in [0, 1, 8191, 8192, 8193, 16384, 20000] {
            let expected: Vec<_> = (0..length).map(|index| (index % 251) as u8).collect();
            std::fs::write(&path, &expected).unwrap();
            target.compare_bytes(&expected).unwrap();

            let mut longer = expected.clone();
            longer.push(0);
            assert!(target.compare_bytes(&longer).is_err());
            std::fs::write(&path, &longer).unwrap();
            assert!(target.compare_bytes(&expected).is_err());

            if !expected.is_empty() {
                std::fs::write(&path, &expected[..length - 1]).unwrap();
                assert!(target.compare_bytes(&expected).is_err());
                for offset in [0, length / 2, length - 1] {
                    let mut changed = expected.clone();
                    changed[offset] ^= 0xff;
                    std::fs::write(&path, &changed).unwrap();
                    assert!(target.compare_bytes(&expected).is_err());
                }
            }
        }
    }
    {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("parent")).unwrap();
        std::fs::write(dir.path().join("parent/target"), b"tool-written").unwrap();
        std::fs::write(outside.path().join("target"), b"outside").unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        std::fs::rename(dir.path().join("parent"), dir.path().join("moved")).unwrap();
        symlink(outside.path(), dir.path().join("parent")).unwrap();
        assert!(workspace.target(Path::new("parent/target"), false).is_err());
        assert!(
            workspace
                .target(Path::new("missing/target"), false)
                .is_err()
        );
        assert!(!dir.path().join("missing").exists());
        assert_eq!(
            std::fs::read(outside.path().join("target")).unwrap(),
            b"outside"
        );
        assert_eq!(
            std::fs::read(dir.path().join("moved/target")).unwrap(),
            b"tool-written"
        );
    }
}

#[test]
fn atomic_save_faults_mode_and_path_safety() {
    {
        for existing in [false, true] {
            for fault in [Fault::Write, Fault::Sync, Fault::Commit] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("target");
                if existing {
                    std::fs::write(&path, b"original\r\n\xe4\xb8\xad").unwrap();
                }
                let workspace = Workspace::open(dir.path()).unwrap();
                let target = workspace.target(Path::new("target"), true).unwrap();
                assert!(
                    target
                        .save_with(b"replacement", &FaultSave(fault), false)
                        .is_err()
                );
                if existing {
                    assert_eq!(std::fs::read(&path).unwrap(), b"original\r\n\xe4\xb8\xad");
                } else {
                    assert!(!path.exists());
                }
                assert_eq!(
                    std::fs::read_dir(dir.path()).unwrap().count(),
                    usize::from(existing)
                );
            }
        }
    }
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        let target = workspace.target(Path::new("script"), false).unwrap();
        let warning = target
            .save_with(b"new", &FaultSave(Fault::Directory), false)
            .unwrap();
        assert!(warning.unwrap().contains("file committed"));
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o751
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    {
        // The save layer validates absolute paths against the canonical root; a temporary parent
        // directory spells the same path once canonicalized.
        let canonical = std::env::temp_dir().canonicalize().unwrap();
        let dir = tempfile::Builder::new().tempdir_in(&canonical).unwrap();
        let outside = outside_dir();
        let peer = outside.path().join("peer");
        std::fs::write(&peer, "outside").unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        symlink(outside.path(), dir.path().join("link")).unwrap();
        symlink(&peer, dir.path().join("file-link")).unwrap();
        symlink("file-link", dir.path().join("link-chain")).unwrap();
        for path in [
            Path::new("../escape"),
            peer.as_path(),
            Path::new("link/peer"),
            Path::new("file-link"),
            Path::new("link-chain"),
        ] {
            assert!(workspace.target(path, true).is_err(), "{path:?}");
        }
        // Once the parent directory is resolved, saving still uses the original directory handle
        // even if the path is swapped for an external link.
        std::fs::create_dir(dir.path().join("parent")).unwrap();
        let target = workspace.target(Path::new("parent/new"), false).unwrap();
        std::fs::rename(dir.path().join("parent"), dir.path().join("moved")).unwrap();
        symlink(outside.path(), dir.path().join("parent")).unwrap();
        target.save(b"inside").unwrap();
        assert!(!outside.path().join("new").exists());
        assert_eq!(
            std::fs::read(dir.path().join("moved/new")).unwrap(),
            b"inside"
        );
        std::fs::hard_link(&peer, dir.path().join("hardlink")).unwrap();
        workspace
            .target(Path::new("hardlink"), false)
            .unwrap()
            .save(b"local")
            .unwrap();
        assert_eq!(std::fs::read(&peer).unwrap(), b"outside");
        // The canonical absolute path of the startup directory works directly (same target as the
        // relative path).
        workspace
            .target(&workspace.path.join("canonical"), true)
            .unwrap()
            .save(b"ok")
            .unwrap();
    }
}

#[test]
#[cfg(target_os = "macos")]
fn macos_tmp_alias_spelling_and_temp_root_writability() {
    {
        // On macOS /tmp is a symlink to /private/tmp; once the workspace canonical root lands in
        // /private/tmp, the /tmp/... spelling must be equivalent to the canonical result, with the
        // same out-of-bounds and symlink rules. The test requires the alias to exist on the actual
        // filesystem; non-default environments fail with an explicit reason instead of skipping
        // silently.
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        assert!(
            dir.path()
                .canonicalize()
                .unwrap()
                .starts_with("/private/tmp"),
            "tempdir_in(/tmp) must canonicalize under /private/tmp; alias test requires the default macOS layout"
        );
        let workspace = Workspace::open(dir.path()).unwrap();
        let alias_root = std::path::PathBuf::from("/tmp").join(
            dir.path()
                .strip_prefix("/tmp")
                .expect("tempdir lies under /tmp"),
        );
        // All three spellings of the same directory are fully equivalent.
        workspace
            .target(&alias_root.join("aliased"), true)
            .unwrap()
            .save(b"aliased")
            .unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("aliased")).unwrap(),
            b"aliased"
        );
        let root_missing_file = workspace
            .target(&alias_root, false)
            .expect_err("directory target has no file path")
            .to_string();
        assert_eq!(root_missing_file, "write/edit requires a file path");
        // The workspace is inside the temporary root: sibling directories belong to the temporary
        // root too and are writable, and the key is the real absolute path rather than a
        // workspace-relative path.
        let sibling = tempfile::tempdir_in("/tmp").unwrap();
        workspace
            .target(&sibling.path().join("peer"), true)
            .unwrap()
            .save(b"peer")
            .unwrap();
        assert_eq!(std::fs::read(sibling.path().join("peer")).unwrap(), b"peer");
        assert_eq!(
            workspace.key(&sibling.path().join("peer")).unwrap(),
            sibling.path().canonicalize().unwrap().join("peer")
        );
    }
    {
        // When the startup directory is not inside the temporary directory, the `/tmp` and
        // `/private/tmp` spellings still map to the same managed root and the same canonical key;
        // paths outside the temporary root and the workspace root are still rejected.
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        let outside = tempfile::tempdir_in("/tmp").unwrap();
        let name = outside
            .path()
            .strip_prefix("/tmp")
            .expect("tempdir lies under /tmp");
        let aliased = Path::new("/tmp").join(name).join("probe.txt");
        let real = Path::new("/private/tmp").join(name).join("probe.txt");
        assert_eq!(
            workspace.key(&aliased).unwrap(),
            workspace.key(&real).unwrap()
        );
        assert_eq!(workspace.key(&aliased).unwrap(), real);
        workspace
            .target(&aliased, true)
            .unwrap()
            .save(b"probe")
            .unwrap();
        assert_eq!(std::fs::read(&real).unwrap(), b"probe");
        // Absolute paths outside the temporary root and relative paths escaping the workspace cannot
        // be written.
        assert!(workspace.key(Path::new("/etc/hosts")).is_err());
        assert!(workspace.target(Path::new("../escape"), true).is_err());
    }
}
#[test]
fn read_and_save_new_guards() {
    {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        let target = workspace.target(Path::new("data"), false).unwrap();
        // Missing is the only `None` semantic.
        assert_eq!(target.read_optional().unwrap(), None);
        // The missing-file error semantics of read() are unchanged.
        assert!(target.read().is_err());

        target.save(b"payload").unwrap();
        assert_eq!(
            target.read_optional().unwrap().as_deref(),
            Some(&b"payload"[..])
        );
        assert_eq!(target.read().unwrap(), b"payload");

        // After commit the name is swapped for a symlink: the NOFOLLOW read reports a real error
        // rather than None.
        std::fs::remove_file(dir.path().join("data")).unwrap();
        symlink("elsewhere", dir.path().join("data")).unwrap();
        let error = target.read_optional().unwrap_err();
        assert!(!error.to_string().is_empty());
        assert!(target.read().is_err());
    }
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, b"hidden").unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        let target = workspace.target(Path::new("secret"), false).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        // root and some environments do not enforce permission bits; assert only when the plain
        // read is actually denied.
        if std::fs::read(&path).is_err() {
            assert!(target.read_optional().is_err());
            assert!(target.read().is_err());
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        let target = workspace
            .target(Path::new(".pp/plans/abcdef.md"), true)
            .unwrap();
        assert_eq!(target.save_new(b"first").unwrap(), None);
        assert_eq!(
            std::fs::read(dir.path().join(".pp/plans/abcdef.md")).unwrap(),
            b"first"
        );
        // Neither the success nor a later collision may leave temporaries.
        assert_eq!(
            std::fs::read_dir(dir.path().join(".pp/plans"))
                .unwrap()
                .count(),
            1
        );

        // A pre-existing or raced target is never overwritten.
        std::fs::write(dir.path().join(".pp/plans/abcdef.md"), b"external").unwrap();
        let error = target.save_new(b"second").unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan file name already exists; choose another file_name."
        );
        assert_eq!(
            std::fs::read(dir.path().join(".pp/plans/abcdef.md")).unwrap(),
            b"external"
        );
        assert_eq!(
            std::fs::read_dir(dir.path().join(".pp/plans"))
                .unwrap()
                .count(),
            1
        );
    }
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".pp/plans/abcdef.md");
        let workspace = Workspace::open(dir.path()).unwrap();
        let target = workspace
            .target(Path::new(".pp/plans/abcdef.md"), true)
            .unwrap();
        for fault in [Fault::Write, Fault::Sync, Fault::CommitNew] {
            assert!(
                target
                    .save_with(b"content", &FaultSave(fault), true)
                    .is_err(),
                "{fault:?}"
            );
            assert!(!path.exists());
            assert_eq!(
                std::fs::read_dir(dir.path().join(".pp/plans"))
                    .unwrap()
                    .count(),
                0,
                "{fault:?}"
            );
        }
    }
    {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        std::fs::write(dir.path().join("exact.bin"), vec![b'a'; 4096]).unwrap();
        let exact = workspace.target(Path::new("exact.bin"), false).unwrap();
        assert_eq!(exact.read_bounded(4096).unwrap().len(), 4096);

        // One byte over the cap reads max+1 so callers detect oversize.
        std::fs::write(dir.path().join("over.bin"), vec![b'b'; 4097]).unwrap();
        let over = workspace.target(Path::new("over.bin"), false).unwrap();
        assert_eq!(over.read_bounded(4096).unwrap().len(), 4097);

        // Much larger files still read at most max+1 bytes.
        std::fs::write(dir.path().join("huge.bin"), vec![b'c'; 100_000]).unwrap();
        let huge = workspace.target(Path::new("huge.bin"), false).unwrap();
        assert_eq!(huge.read_bounded(4096).unwrap().len(), 4097);

        assert!(
            workspace
                .target(Path::new("missing.bin"), false)
                .unwrap()
                .read_bounded(16)
                .is_err()
        );

        // Symlinks are refused before any read.
        symlink("exact.bin", dir.path().join("link.bin")).unwrap();
        assert!(workspace.target(Path::new("link.bin"), false).is_err());

        // FIFO targets must not hang on open and are rejected outright.
        std::process::Command::new("mkfifo")
            .arg(dir.path().join("pipe"))
            .status()
            .unwrap();
        assert!(workspace.target(Path::new("pipe"), false).is_err());

        // Directories are rejected as non-regular targets.
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        assert!(workspace.target(Path::new("subdir"), false).is_err());
    }
}
#[test]
fn hashline_checked_save_and_swap_rejection() {
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target");
        std::fs::write(&path, b"read-version").unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        let target = workspace.target(Path::new("target"), false).unwrap();
        let operations = MutateAfterSync {
            path: &path,
            content: b"external-version",
        };
        let error = target
            .save_checked_with(
                b"edit-version",
                &operations,
                false,
                Some((b"read-version", "edit refused")),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "edit refused");
        assert_eq!(std::fs::read(&path).unwrap(), b"external-version");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        for fault in [Fault::Write, Fault::Sync, Fault::Commit] {
            std::fs::write(&path, b"read-version").unwrap();
            assert!(
                target
                    .save_checked_with(
                        b"edit-version",
                        &FaultSave(fault),
                        false,
                        Some((b"read-version", "edit refused"))
                    )
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"read-version");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }
        let warning = target
            .save_checked_with(
                b"edit-version",
                &FaultSave(Fault::Directory),
                false,
                Some((b"read-version", "edit refused")),
            )
            .unwrap();
        assert!(warning.is_some());
        assert_eq!(std::fs::read(&path).unwrap(), b"edit-version");
    }
    {
        struct Swap<'a> {
            path: &'a Path,
            outside: &'a Path,
            kind: usize,
        }
        impl SaveOperations for Swap<'_> {
            fn sync_file(&self, file: &File) -> io::Result<()> {
                SystemSave.sync_file(file)?;
                std::fs::remove_file(self.path)?;
                match self.kind {
                    0 => Ok(()),
                    1 => std::fs::create_dir(self.path),
                    _ => symlink(self.outside, self.path),
                }
            }
        }
        for kind in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let outside = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(outside.path(), b"outside").unwrap();
            let path = dir.path().join("target");
            std::fs::write(&path, b"original").unwrap();
            let workspace = Workspace::open(dir.path()).unwrap();
            let target = workspace.target(Path::new("target"), false).unwrap();
            let error = target
                .save_checked_with(
                    b"edited",
                    &Swap {
                        path: &path,
                        outside: outside.path(),
                        kind,
                    },
                    false,
                    Some((b"original", "edit refused")),
                )
                .unwrap_err();
            assert_eq!(error.to_string(), "edit refused");
            assert_eq!(std::fs::read(outside.path()).unwrap(), b"outside");
            assert_eq!(
                std::fs::read_dir(dir.path()).unwrap().count(),
                usize::from(kind != 0)
            );
            if kind == 1 {
                assert!(path.is_dir());
            }
            if kind == 2 {
                assert!(path.is_symlink());
            }
        }
    }
}

// --- Plan lifecycle move primitives ---

#[test]
fn plan_move_lifecycle_collisions_and_escapes() {
    {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        let source = dir.path().join(".pp/drafts/fix-config.md");
        let target = dir.path().join(".pp/plans/fix-config.md");
        std::fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        std::fs::write(&source, b"# plan\r\n\xff bytes").unwrap();

        // Publish the destination link: bytes preserved exactly, source still present.
        let warning = workspace
            .link_plan(
                Path::new(".pp/drafts/fix-config.md"),
                Path::new(".pp/plans/fix-config.md"),
                b"# plan\r\n\xff bytes",
            )
            .unwrap();
        assert_eq!(warning, None);
        assert_eq!(std::fs::read(&target).unwrap(), b"# plan\r\n\xff bytes");
        assert_eq!(std::fs::read(&source).unwrap(), b"# plan\r\n\xff bytes");
        // A hard link to the same inode, not a copy.
        assert_eq!(
            std::fs::metadata(&source).unwrap().ino(),
            std::fs::metadata(&target).unwrap().ino()
        );

        // Republishing is idempotent re-entry; the commit removes the source name; committing again
        // also succeeds.
        assert_eq!(
            workspace
                .link_plan(
                    Path::new(".pp/drafts/fix-config.md"),
                    Path::new(".pp/plans/fix-config.md"),
                    b"# plan\r\n\xff bytes"
                )
                .unwrap(),
            None
        );
        assert_eq!(
            workspace
                .finish_plan_move(
                    Path::new(".pp/drafts/fix-config.md"),
                    Path::new(".pp/plans/fix-config.md"),
                    b"# plan\r\n\xff bytes"
                )
                .unwrap(),
            None
        );
        assert!(!source.exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"# plan\r\n\xff bytes");
        assert_eq!(
            workspace
                .finish_plan_move(
                    Path::new(".pp/drafts/fix-config.md"),
                    Path::new(".pp/plans/fix-config.md"),
                    b"# plan\r\n\xff bytes"
                )
                .unwrap(),
            None
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"# plan\r\n\xff bytes");
    }
    {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        std::fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        std::fs::write(dir.path().join(".pp/drafts/a.md"), b"source").unwrap();
        std::fs::write(dir.path().join(".pp/plans/a.md"), b"existing").unwrap();

        // A destination with identical bytes but a different inode is also a conflict; different
        // bytes need no argument.
        std::fs::hard_link(
            dir.path().join(".pp/drafts/a.md"),
            dir.path().join(".pp/plans/copy.md"),
        )
        .unwrap();
        std::fs::write(dir.path().join(".pp/plans/same-bytes.md"), b"source").unwrap();
        for target in ["a.md", "same-bytes.md"] {
            let error = workspace
                .link_plan(
                    Path::new(".pp/drafts/a.md"),
                    Path::new(&format!(".pp/plans/{target}")),
                    b"source",
                )
                .unwrap_err();
            assert!(
                error.to_string().contains("source file is kept"),
                "{target}: {error}"
            );
        }
        assert_eq!(
            std::fs::read(dir.path().join(".pp/plans/a.md")).unwrap(),
            b"existing"
        );
        assert_eq!(
            std::fs::read(dir.path().join(".pp/drafts/a.md")).unwrap(),
            b"source"
        );
        std::fs::remove_file(dir.path().join(".pp/plans/copy.md")).unwrap();
        std::fs::remove_file(dir.path().join(".pp/plans/same-bytes.md")).unwrap();

        // A mismatched expected byte sequence errors and publishes no destination.
        let error = workspace
            .link_plan(
                Path::new(".pp/drafts/a.md"),
                Path::new(".pp/plans/b.md"),
                b"other",
            )
            .unwrap_err();
        assert!(error.to_string().contains("source changed"), "{error}");
        assert!(!dir.path().join(".pp/plans/b.md").exists());
        assert_eq!(
            std::fs::read(dir.path().join(".pp/drafts/a.md")).unwrap(),
            b"source"
        );
    }
    {
        let dir = tempfile::tempdir().unwrap();
        let outside = outside_dir();
        std::fs::write(outside.path().join("peer"), b"outside").unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        std::fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        std::fs::write(dir.path().join(".pp/drafts/a.md"), b"inside").unwrap();
        symlink(
            outside.path().join("peer"),
            dir.path().join(".pp/plans/link.md"),
        )
        .unwrap();

        // A symlink destination is rejected before publishing; a symlink source is rejected too and
        // the source is kept.
        let error = workspace
            .link_plan(
                Path::new(".pp/drafts/a.md"),
                Path::new(".pp/plans/link.md"),
                b"inside",
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("symlinks are forbidden"),
            "{error}"
        );
        assert!(
            std::fs::symlink_metadata(dir.path().join(".pp/plans/link.md"))
                .unwrap()
                .is_symlink()
        );
        symlink(
            dir.path().join(".pp/drafts/a.md"),
            dir.path().join(".pp/drafts/alias.md"),
        )
        .unwrap();
        assert!(
            workspace
                .link_plan(
                    Path::new(".pp/drafts/alias.md"),
                    Path::new(".pp/plans/b.md"),
                    b"inside"
                )
                .is_err()
        );
        assert!(!dir.path().join(".pp/plans/b.md").exists());
        assert_eq!(
            std::fs::read(dir.path().join(".pp/drafts/a.md")).unwrap(),
            b"inside"
        );

        // A path escape (outside the workspace) is rejected.
        assert!(
            workspace
                .link_plan(
                    Path::new(".pp/drafts/a.md"),
                    &outside.path().join("escape.md"),
                    b"inside"
                )
                .is_err()
        );
        assert!(!outside.path().join("escape.md").exists());

        // The commit step requires the source bytes to still match; on mismatch both sides are kept.
        std::fs::write(dir.path().join(".pp/drafts/a.md"), b"changed").unwrap();
        workspace
            .link_plan(
                Path::new(".pp/drafts/a.md"),
                Path::new(".pp/plans/b.md"),
                b"changed",
            )
            .unwrap();
        std::fs::write(dir.path().join(".pp/drafts/a.md"), b"tampered").unwrap();
        let error = workspace
            .finish_plan_move(
                Path::new(".pp/drafts/a.md"),
                Path::new(".pp/plans/b.md"),
                b"changed",
            )
            .unwrap_err();
        assert!(error.to_string().contains("both files are kept"), "{error}");
        assert!(dir.path().join(".pp/drafts/a.md").exists());
        assert!(dir.path().join(".pp/plans/b.md").exists());

        // A source symlink swapped in from outside is not followed by the commit step.
        std::fs::remove_file(dir.path().join(".pp/drafts/a.md")).unwrap();
        symlink(
            outside.path().join("peer"),
            dir.path().join(".pp/drafts/a.md"),
        )
        .unwrap();
        assert!(
            workspace
                .finish_plan_move(
                    Path::new(".pp/drafts/a.md"),
                    Path::new(".pp/plans/b.md"),
                    b"changed"
                )
                .is_err()
        );
        assert_eq!(
            std::fs::read(outside.path().join("peer")).unwrap(),
            b"outside"
        );
        assert!(dir.path().join(".pp/plans/b.md").exists());
    }
}

// --- Directory snapshot byte budgets and cleanup after publishing plan links ---

/// Swap the source name for a symlink pointing outside just before linkat: on macOS a linkat
/// without `AT_SYMLINK_FOLLOW` publishes the symlink itself, on Linux it fails with EPERM; neither
/// platform may leave the destination name on disk, and neither may swallow the error.
struct SwapSourceWithSymlink {
    source: PathBuf,
    outside: PathBuf,
}

impl PlanLinkOperations for SwapSourceWithSymlink {
    fn publish(
        &self,
        source_parent: &File,
        source_name: &OsStr,
        destination_parent: &File,
        destination_name: &OsStr,
    ) -> rustix::io::Result<()> {
        std::fs::remove_file(&self.source).unwrap();
        symlink(&self.outside, &self.source).unwrap();
        SystemPlanLink.publish(
            source_parent,
            source_name,
            destination_parent,
            destination_name,
        )
    }
}

#[test]
fn plan_move_symlink_swap_walk_budget_and_external_target() {
    {
        let dir = tempfile::tempdir().unwrap();
        let outside = outside_dir();
        let peer = outside.path().join("peer");
        std::fs::write(&peer, b"outside").unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        std::fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        let source = dir.path().join(".pp/drafts/a.md");
        std::fs::write(&source, b"inside").unwrap();
        let target = dir.path().join(".pp/plans/b.md");
        let error = workspace
            .link_plan_with(
                Path::new(".pp/drafts/a.md"),
                Path::new(".pp/plans/b.md"),
                b"inside",
                &SwapSourceWithSymlink {
                    source: source.clone(),
                    outside: peer.clone(),
                },
            )
            .unwrap_err();
        // The error is diagnosable: the cleanup branch returns the fixed message and does not
        // swallow the failure.
        assert!(error.to_string().contains("unverified link"), "{error}");
        // The old behavior left a symlink pointing outside under the destination name; after the fix
        // the destination name does not exist.
        assert!(
            std::fs::symlink_metadata(&target).is_err(),
            "published plan link target must be cleaned up, found: {}",
            target.display()
        );
        // The external file is untouched and the source name (now a symlink to it) is also kept.
        assert_eq!(std::fs::read(&peer).unwrap(), b"outside");
        assert!(
            std::fs::symlink_metadata(&source).is_ok(),
            "the failed move must keep the source name"
        );
    }
    {
        // Small-budget table: every tree is top.bin's 4 bytes plus sub/deep.bin's 1 byte, exactly 5
        // bytes total; a budget equal to the total passes and one byte less is refused. The two files
        // sit at different levels, so cross-recursion accumulation is covered as well; directories do
        // not consume the byte budget.
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        for (budget, case) in [
            (5_u64, "exact"),
            (4_u64, "one-byte-over"),
            (5_u64, "flat-exact"),
            (4_u64, "flat-over"),
        ] {
            let root = dir.path().join(case);
            let nested = root.join("sub");
            std::fs::create_dir_all(&nested).unwrap();
            std::fs::write(root.join("top.bin"), b"abcd").unwrap();
            // The flat case puts the second file at the top level: same total, different structure.
            if case.starts_with("flat") {
                std::fs::write(root.join("side.bin"), b"e").unwrap();
            } else {
                std::fs::write(nested.join("deep.bin"), b"e").unwrap();
            }
            let over = case.ends_with("over");
            let result = workspace.walk_directory_cancellable(
                Path::new(case),
                RESOURCE_FILE_LIMIT,
                budget,
                &CancellationToken::new(),
            );
            if over {
                let error = result.err().expect("over-budget walk must be refused");
                assert!(
                    error.to_string().contains("more than") && error.to_string().contains("bytes"),
                    "{case}: {error}"
                );
            } else {
                let entries = result.expect("the total equals the budget exactly");
                let total: u64 = entries
                    .iter()
                    .filter_map(|entry| entry.content.as_ref())
                    .map(|content| content.len() as u64)
                    .sum();
                assert_eq!(total, 5, "{case}");
            }
            std::fs::remove_dir_all(&root).unwrap();
        }
    }
    {
        // A sparse file proves it without allocating 256 MiB for real: a regular file whose set_len
        // exceeds the limit is rejected by the metadata pre-check and never read into memory.
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        let big = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.path().join("d/big.bin"))
            .unwrap();
        big.set_len(RESOURCE_BYTE_LIMIT + 1).unwrap();
        drop(big);
        let error = workspace
            .walk_directory_cancellable(
                Path::new("d"),
                RESOURCE_FILE_LIMIT,
                RESOURCE_BYTE_LIMIT,
                &CancellationToken::new(),
            )
            .err()
            .expect("sparse file over the limit must be refused");
        assert!(
            error.to_string().contains("more than") && error.to_string().contains("bytes"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_dir(dir.path().join("d")).unwrap().count(),
            1,
            "rejection must not delete or commit anything"
        );
    }
    {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        std::fs::write(dir.path().join("d/a.bin"), b"a").unwrap();
        std::fs::create_dir(dir.path().join("d/sub")).unwrap();
        std::fs::write(dir.path().join("d/sub/b.bin"), b"b").unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let error = workspace
            .walk_directory_cancellable(
                Path::new("d"),
                RESOURCE_FILE_LIMIT,
                RESOURCE_BYTE_LIMIT,
                &cancelled,
            )
            .err()
            .expect("cancelled walk must be refused");
        assert!(error.to_string().contains("cancelled"), "{error}");
        // Existing files stay intact: a rejection only abandons the snapshot and commits/deletes
        // nothing.
        assert!(dir.path().join("d/a.bin").exists());
        assert!(dir.path().join("d/sub/b.bin").exists());
    }
    {
        let dir = tempfile::tempdir().unwrap();
        let outside = outside_dir();
        let workspace = Workspace::open(dir.path()).unwrap();

        let external_file = outside.path().join("nested/note.txt");
        let target = workspace.external_target(&external_file, true).unwrap();
        target.save_new_with_conflict(b"hello", "exists").unwrap();
        assert_eq!(std::fs::read(&external_file).unwrap(), b"hello");

        let link = outside.path().join("sym.txt");
        symlink(&external_file, &link).unwrap();
        let err = workspace.external_target(&link, false).unwrap_err();
        assert!(err.to_string().contains("symlinks are forbidden"), "{err}");

        assert!(
            workspace
                .external_target(Path::new("relative.txt"), false)
                .is_err()
        );
    }
}
