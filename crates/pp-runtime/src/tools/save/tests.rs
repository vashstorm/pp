use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

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

fn assert_undo_conflict<T>(result: Result<T>) {
    let Err(error) = result else {
        panic!("expected undo conflict");
    };
    assert_eq!(
        error.to_string(),
        "File changed outside this task; undo refused."
    );
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
fn changes_matches_exact_bytes_across_chunks_and_eof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("target");
    let workspace = Workspace::open(dir.path()).unwrap();
    let target = workspace.target(Path::new("target"), false).unwrap();
    for length in [0, 1, 8191, 8192, 8193, 16384, 20000] {
        let expected: Vec<_> = (0..length).map(|index| (index % 251) as u8).collect();
        std::fs::write(&path, &expected).unwrap();
        target.ensure_matches(&expected).unwrap();

        let mut longer = expected.clone();
        longer.push(0);
        assert_undo_conflict(target.ensure_matches(&longer));
        std::fs::write(&path, &longer).unwrap();
        assert_undo_conflict(target.ensure_matches(&expected));

        if !expected.is_empty() {
            std::fs::write(&path, &expected[..length - 1]).unwrap();
            assert_undo_conflict(target.ensure_matches(&expected));
            for offset in [0, length / 2, length - 1] {
                let mut changed = expected.clone();
                changed[offset] ^= 0xff;
                std::fs::write(&path, &changed).unwrap();
                assert_undo_conflict(target.ensure_matches(&expected));
            }
        }
    }
}

#[test]
fn changes_restore_checks_after_temporary_sync_and_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("target");
    std::fs::write(&path, b"tool-written").unwrap();
    let workspace = Workspace::open(dir.path()).unwrap();
    let target = workspace.target(Path::new("target"), false).unwrap();
    target.ensure_matches(b"tool-written").unwrap();
    let operations = MutateAfterSync {
        path: &path,
        content: b"user-written",
    };
    assert_undo_conflict(target.restore_with(b"tool-written", Some(b"original"), &operations));
    assert_eq!(std::fs::read(&path).unwrap(), b"user-written");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn changes_restore_faults_keep_target_and_remove_temporary() {
    for fault in [Fault::Write, Fault::Sync, Fault::Commit] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target");
        std::fs::write(&path, b"tool-written\r\n\xff").unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        let target = workspace.target(Path::new("target"), false).unwrap();
        assert!(
            target
                .restore_with(
                    b"tool-written\r\n\xff",
                    Some(b"original\r\n\xef\xbb\xbf"),
                    &FaultSave(fault),
                )
                .is_err(),
            "{fault:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"tool-written\r\n\xff");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

#[test]
fn changes_restore_and_delete_report_directory_sync_warning_after_commit() {
    for original in [Some(&b"original\r\n\xff"[..]), None] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target");
        std::fs::write(&path, b"tool-written").unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        let target = workspace.target(Path::new("target"), false).unwrap();
        let warning = target
            .restore_with(b"tool-written", original, &FaultSave(Fault::Directory))
            .unwrap()
            .unwrap();
        assert!(warning.contains("syncing parent directory failed"));
        if let Some(original) = original {
            assert!(warning.contains("file committed"));
            assert_eq!(std::fs::read(&path).unwrap(), original);
        } else {
            assert!(warning.contains("file deleted"));
            assert!(!path.exists());
        }
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            usize::from(original.is_some())
        );
    }
}

#[test]
fn changes_restore_preserves_exact_baseline_and_current_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("target");
    let workspace = Workspace::open(dir.path()).unwrap();
    for original in [&b""[..], &b"\xef\xbb\xbfuser-local\r\nkeep\xff"[..]] {
        std::fs::write(&path, b"tool-written").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
        // Re-resolve at undo time, so a user chmod after the tool write is
        // preserved rather than restoring any baseline permission bits.
        let target = workspace.target(Path::new("target"), false).unwrap();
        assert_eq!(
            target
                .restore_if_unchanged(b"tool-written", Some(original))
                .unwrap(),
            None
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o751
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

#[test]
fn changes_delete_empty_and_nonempty_files_keeps_parent_directory() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(dir.path()).unwrap();
    for expected in [&b""[..], &b"created\xff\r\n"[..]] {
        let target = workspace.target(Path::new("parent/new"), true).unwrap();
        target.save(expected).unwrap();
        assert_eq!(target.restore_if_unchanged(expected, None).unwrap(), None);
        assert!(!dir.path().join("parent/new").exists());
        assert!(dir.path().join("parent").is_dir());
        assert_eq!(
            std::fs::read_dir(dir.path().join("parent"))
                .unwrap()
                .count(),
            0
        );
    }
}

#[test]
fn changes_restore_and_delete_refuse_external_bytes_and_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("target");
    let workspace = Workspace::open(dir.path()).unwrap();
    for original in [Some(&b"original"[..]), None] {
        for external in [
            &b""[..],
            &b"tool"[..],
            &b"tool-written-extra"[..],
            &b"user-written"[..],
        ] {
            std::fs::write(&path, external).unwrap();
            let target = workspace.target(Path::new("target"), false).unwrap();
            assert_undo_conflict(target.restore_if_unchanged(b"tool-written", original));
            assert_eq!(std::fs::read(&path).unwrap(), external);
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }
        std::fs::remove_file(&path).unwrap();
        let target = workspace.target(Path::new("target"), false).unwrap();
        assert_undo_conflict(target.ensure_matches(b"tool-written"));
        assert_undo_conflict(target.restore_if_unchanged(b"tool-written", original));
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}

#[test]
fn changes_undo_rejects_swapped_special_targets_without_following_them() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let peer = outside.path().join("peer");
    let path = dir.path().join("target");
    std::fs::write(&peer, b"tool-written").unwrap();
    let workspace = Workspace::open(dir.path()).unwrap();
    // Hold a previously valid target to exercise the late open's type
    // checks too, not only Workspace::target's initial validation.
    std::fs::write(&path, b"tool-written").unwrap();
    let target = workspace.target(Path::new("target"), false).unwrap();
    std::fs::remove_file(&path).unwrap();
    symlink(&peer, &path).unwrap();
    assert!(workspace.target(Path::new("target"), false).is_err());
    assert_undo_conflict(target.ensure_matches(b"tool-written"));
    for original in [Some(&b"original"[..]), None] {
        assert_undo_conflict(target.restore_if_unchanged(b"tool-written", original));
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&peer).unwrap(), b"tool-written");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    std::fs::remove_file(&path).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    assert!(workspace.target(Path::new("target"), false).is_err());
    assert_undo_conflict(target.ensure_matches(b""));
    for original in [Some(&b"original"[..]), None] {
        assert_undo_conflict(target.restore_if_unchanged(b"", original));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert_undo_conflict(target.ensure_matches(b""));
    for original in [Some(&b"original"[..]), None] {
        assert_undo_conflict(target.restore_if_unchanged(b"", original));
        assert!(path.is_dir());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

#[test]
fn changes_undo_target_resolution_rejects_parent_symlink_and_missing_parent() {
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

#[test]
fn changes_undo_refuses_unreadable_target_without_changing_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("target");
    std::fs::write(&path, b"tool-written").unwrap();
    let workspace = Workspace::open(dir.path()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Privileged test environments can bypass mode bits, as in the
    // existing read_optional permission regression.
    if std::fs::read(&path).is_err() {
        let target = workspace.target(Path::new("target"), false).unwrap();
        assert_undo_conflict(target.ensure_matches(b"tool-written"));
        for original in [Some(&b"original"[..]), None] {
            assert_undo_conflict(target.restore_if_unchanged(b"tool-written", original));
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0
            );
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"tool-written");
}

#[test]
fn atomic_save_failures_leave_existing_or_missing_target_unchanged() {
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

#[test]
fn atomic_save_preserves_mode_and_reports_post_commit_warning_as_success() {
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

#[test]
fn write_paths_reject_escape_and_symlinks_and_never_modify_hardlink_peer() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
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
    // 解析父目录后即使路径被换成外部链接，保存仍使用原目录句柄。
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
    // 启动目录的原始绝对路径和规范绝对路径均可使用。
    workspace
        .target(&dir.path().join("absolute"), true)
        .unwrap()
        .save(b"ok")
        .unwrap();
    workspace
        .target(&workspace.path.join("canonical"), true)
        .unwrap()
        .save(b"ok")
        .unwrap();
}

#[test]
fn read_optional_returns_none_only_for_missing_target() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(dir.path()).unwrap();
    let target = workspace.target(Path::new("data"), false).unwrap();
    // 缺失是唯一的 None 语义。
    assert_eq!(target.read_optional().unwrap(), None);
    // read() 的缺失错误语义保持不变。
    assert!(target.read().is_err());

    target.save(b"payload").unwrap();
    assert_eq!(
        target.read_optional().unwrap().as_deref(),
        Some(&b"payload"[..])
    );
    assert_eq!(target.read().unwrap(), b"payload");

    // 提交后名称被换成 symlink：NOFOLLOW 读取报真实错误，而不是 None。
    std::fs::remove_file(dir.path().join("data")).unwrap();
    symlink("elsewhere", dir.path().join("data")).unwrap();
    let error = target.read_optional().unwrap_err();
    assert!(!error.to_string().is_empty());
    assert!(target.read().is_err());
}

#[test]
fn read_optional_reports_real_read_errors_instead_of_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secret");
    std::fs::write(&path, b"hidden").unwrap();
    let workspace = Workspace::open(dir.path()).unwrap();
    let target = workspace.target(Path::new("secret"), false).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    // root 与部分环境不强制权限位；仅当普通读取确实被拒绝时断言。
    if std::fs::read(&path).is_ok() {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        return;
    }
    assert!(target.read_optional().is_err());
    assert!(target.read().is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
}

#[test]
fn save_new_creates_target_once_and_collision_never_overwrites() {
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

#[test]
fn save_new_faults_leave_no_target_or_temporaries() {
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

#[test]
fn read_bounded_caps_bytes_and_rejects_special_targets() {
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
#[test]
fn hashline_edit_save_checks_after_sync_and_preserves_external_content() {
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
#[test]
fn hashline_edit_save_refuses_removed_or_swapped_targets() {
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

// --- 计划生命周期搬迁原语 ---

#[test]
fn plan_move_preserves_bytes_and_finish_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(dir.path()).unwrap();
    let source = dir.path().join(".pp/drafts/fix-config.md");
    let target = dir.path().join(".pp/plans/fix-config.md");
    std::fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
    std::fs::write(&source, b"# plan\r\n\xff bytes").unwrap();

    // 发布目标链接：字节精确保留，源仍存在。
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
    // 同一 inode 的硬链接，不是复制。
    assert_eq!(
        std::fs::metadata(&source).unwrap().ino(),
        std::fs::metadata(&target).unwrap().ino()
    );

    // 重复发布是幂等重入；提交移除源名；再次提交同样成功。
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

#[test]
fn plan_move_collision_never_overwrites_and_keeps_source() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(dir.path()).unwrap();
    std::fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
    std::fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
    std::fs::write(dir.path().join(".pp/drafts/a.md"), b"source").unwrap();
    std::fs::write(dir.path().join(".pp/plans/a.md"), b"existing").unwrap();

    // 字节相同的不同 inode 目标同样冲突；不同字节自不必说。
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

    // 预期字节不匹配时报错且不发布目标。
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

#[test]
fn plan_move_refuses_symlinks_escapes_and_finish_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
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

    // 符号链接目标在发布前被拒绝；符号链接源同样拒绝，源保留。
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

    // 路径逃逸（工作区外）被拒绝。
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

    // 提交步骤要求源字节仍精确匹配；不匹配时两侧都保留。
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

    // 外部换掉的符号链接源不被提交步骤跟随。
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
