use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{self, AtFlags, FileType, Mode, OFlags},
    io::Errno,
};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub(super) struct Workspace {
    root: File,
    pub(super) path: PathBuf,
    original: PathBuf,
}

pub(super) struct Target {
    parent: File,
    name: OsString,
    mode: Option<Mode>,
}

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

impl Workspace {
    pub(super) fn open(path: &Path) -> Result<Self> {
        let original = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        let path = original
            .canonicalize()
            .context("resolving startup directory")?;
        let root = File::open(&path).context("opening startup directory")?;
        ensure!(
            root.metadata()?.is_dir(),
            "startup path must be a directory"
        );
        Ok(Self {
            root,
            path,
            original,
        })
    }

    pub(super) fn components(&self, path: &Path) -> Result<Vec<OsString>> {
        let relative = if path.is_absolute() {
            path.strip_prefix(&self.original)
                .or_else(|_| path.strip_prefix(&self.path))
                .context("write/edit path must stay inside startup directory")?
        } else {
            path
        };
        let mut parts = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(name) => parts.push(name.to_owned()),
                Component::CurDir => {}
                Component::ParentDir => {
                    ensure!(
                        parts.pop().is_some(),
                        "write/edit path escapes startup directory"
                    );
                }
                _ => anyhow::bail!("invalid write/edit path"),
            }
        }
        Ok(parts)
    }

    fn directory(&self, parts: &[OsString], create: bool) -> Result<File> {
        let mut parent = self.root.try_clone()?;
        for part in parts {
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let fd = match fs::openat(&parent, part, flags, Mode::empty()) {
                Ok(fd) => fd,
                Err(Errno::NOENT) if create => {
                    match fs::mkdirat(&parent, part, Mode::from_raw_mode(0o777)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(error) => return Err(error).context("creating parent directory"),
                    }
                    fs::openat(&parent, part, flags, Mode::empty())
                        .context("opening created directory without symlinks")?
                }
                Err(error) => {
                    return Err(error)
                        .context("opening parent directory (symlink paths are forbidden)");
                }
            };
            parent = File::from(fd);
        }
        Ok(parent)
    }

    pub(super) fn cache_dir(&self) -> Result<PathBuf> {
        self.directory(&[OsString::from(".pp-cache")], true)?;
        Ok(self.path.join(".pp-cache"))
    }

    pub(super) fn target(&self, path: &Path, create_parents: bool) -> Result<Target> {
        let mut parts = self.components(path)?;
        let name = parts.pop().context("write/edit requires a file path")?;
        let parent = self.directory(&parts, create_parents)?;
        let mode = match fs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => {
                ensure!(
                    FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile,
                    "write/edit target must be a regular file; symlinks are forbidden"
                );
                Some(Mode::from_raw_mode(stat.st_mode))
            }
            Err(Errno::NOENT) => None,
            Err(error) => return Err(error).context("checking write/edit target"),
        };
        Ok(Target { parent, name, mode })
    }
}

impl Target {
    /// Opens the target for reading through the already-resolved parent
    /// directory handle; NOFOLLOW prevents a symlink from sneaking in after
    /// `Workspace::target` resolved the path.
    fn open_read(&self) -> rustix::io::Result<File> {
        let fd = fs::openat(
            &self.parent,
            &self.name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(File::from(fd))
    }

    pub(super) fn read(&self) -> Result<Vec<u8>> {
        let file = self
            .open_read()
            .context("opening edit target without symlinks")?;
        read_regular_file(file)
    }

    /// Optional snapshot for diff display. Only a genuinely missing target is
    /// `None`; every other failure (permissions, IO, race with a symlink swap)
    /// is a real error so callers can fall back to `Unavailable` instead of
    /// inventing an empty baseline.
    pub(super) fn read_optional(&self) -> Result<Option<Vec<u8>>> {
        match self.open_read() {
            Ok(file) => read_regular_file(file).map(Some),
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(error).context("reading previous content for diff"),
        }
    }

    /// Compares exact bytes through a fresh NOFOLLOW handle without allocating
    /// another copy of the file. Missing, unreadable and non-regular targets
    /// are conflicts just like a content mismatch.
    pub(super) fn ensure_matches(&self, expected: &[u8]) -> Result<()> {
        self.compare_bytes(expected)
            .context("File changed outside this task; undo refused.")
    }

    /// 共用精确比较；调用者负责提供操作对应的脱敏错误。
    fn compare_bytes(&self, expected: &[u8]) -> Result<()> {
        let mut file = self.open_read()?;
        ensure!(file.metadata()?.is_file(), "target must be a regular file");
        let mut buffer = [0_u8; 8192];
        for chunk in expected.chunks(buffer.len()) {
            file.read_exact(&mut buffer[..chunk.len()])?;
            ensure!(&buffer[..chunk.len()] == chunk, "target content mismatch");
        }
        match file.read_exact(&mut buffer[..1]) {
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(()),
            Err(error) => Err(error.into()),
            Ok(()) => anyhow::bail!("target content has additional bytes"),
        }
    }

    pub(super) fn save(&self, content: &[u8]) -> Result<Option<String>> {
        self.save_with(content, &SystemSave, false)
    }

    /// 在临时文件同步之后检查读取时的基线，再原子提交。
    pub(super) fn save_if_unchanged(
        &self,
        expected: &[u8],
        content: &[u8],
    ) -> Result<Option<String>> {
        self.save_checked_with(
            content,
            &SystemSave,
            false,
            Some((
                expected,
                "File changed since edit was staged; edit refused. Read the file again.",
            )),
        )
    }

    /// Restores the task baseline, or removes a task-created file, only after
    /// checking the last tool-written bytes. This is not filesystem CAS: an
    /// external writer can still race between comparison and rename/unlink.
    pub(super) fn restore_if_unchanged(
        &self,
        expected: &[u8],
        original: Option<&[u8]>,
    ) -> Result<Option<String>> {
        self.restore_with(expected, original, &SystemSave)
    }

    fn restore_with(
        &self,
        expected: &[u8],
        original: Option<&[u8]>,
        operations: &impl SaveOperations,
    ) -> Result<Option<String>> {
        if let Some(content) = original {
            return self.save_checked_with(
                content,
                operations,
                false,
                Some((expected, "File changed outside this task; undo refused.")),
            );
        }
        self.ensure_matches(expected)?;
        fs::unlinkat(&self.parent, &self.name, AtFlags::empty())
            .context("removing task-created file; undo not committed")?;
        Ok(operations
            .sync_directory(&self.parent)
            .err()
            .map(|error| format!("file deleted, but syncing parent directory failed: {error}")))
    }

    /// Atomically creates the target only when it does not already exist.
    /// A concurrent or pre-existing target is never overwritten; `EEXIST`
    /// becomes the exact plan-collision error so callers stay in Plan mode.
    pub(super) fn save_new(&self, content: &[u8]) -> Result<Option<String>> {
        self.save_with(content, &SystemSave, true)
    }

    fn create_temporary(&self) -> Result<(String, File)> {
        loop {
            let name = format!(
                ".pp-save-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            );
            let mode = Mode::from_raw_mode(if self.mode.is_some() { 0o600 } else { 0o666 });
            match fs::openat(
                &self.parent,
                name.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                mode,
            ) {
                Ok(fd) => return Ok((name, File::from(fd))),
                Err(Errno::EXIST) => continue,
                Err(error) => return Err(error).context("creating atomic save file"),
            }
        }
    }

    /// Shared atomic-commit path. `new_only` publishes via `linkat` and fails
    /// on an existing target instead of renaming over it.
    fn save_with(
        &self,
        content: &[u8],
        operations: &impl SaveOperations,
        new_only: bool,
    ) -> Result<Option<String>> {
        self.save_checked_with(content, operations, new_only, None)
    }

    fn save_checked_with(
        &self,
        content: &[u8],
        operations: &impl SaveOperations,
        new_only: bool,
        expected: Option<(&[u8], &'static str)>,
    ) -> Result<Option<String>> {
        let (temporary, mut file) = self.create_temporary()?;
        let cleanup = Temporary {
            parent: &self.parent,
            name: &temporary,
        };
        operations
            .write(&mut file, content)
            .context("writing atomic save file; target unchanged")?;
        if !new_only && let Some(mode) = self.mode {
            fs::fchmod(&file, mode).context("preserving target permissions; target unchanged")?;
        }
        operations
            .sync_file(&file)
            .context("syncing atomic save file; target unchanged")?;
        if let Some((expected, message)) = expected {
            // Keep this after the temporary file has been fully written and
            // synced, immediately before publishing it. This narrows, but does
            // not eliminate, the comparison-to-rename race with other writers.
            self.compare_bytes(expected).context(message)?;
        }
        let committed = if new_only {
            operations.commit_new(&self.parent, OsStr::new(&temporary), &self.name)
        } else {
            operations.commit(&self.parent, OsStr::new(&temporary), &self.name)
        };
        if let Err(error) = committed {
            if new_only && error.kind() == io::ErrorKind::AlreadyExists {
                anyhow::bail!("Plan path already exists; use /plan new.");
            }
            return Err(error).context("committing atomic save; target unchanged");
        }
        if new_only {
            // linkat 已提交且目标名持有同一 inode；删掉临时名，目标不受影响。
            drop(cleanup);
        } else {
            // 重命名已经提交，后续错误只能作为持久化警告，不能伪装成未修改。
            std::mem::forget(cleanup);
        }
        Ok(operations
            .sync_directory(&self.parent)
            .err()
            .map(|error| format!("file committed, but syncing parent directory failed: {error}")))
    }

    /// Bounded read through the safe NOFOLLOW handle: a regular-file check
    /// first, then at most `max_bytes + 1` bytes so callers can detect
    /// oversize files without allocating unbounded memory. FIFOs, directories
    /// and symlink targets are real errors, never partial reads.
    pub(super) fn read_bounded(&self, max_bytes: usize) -> Result<Vec<u8>> {
        let mut file = self
            .open_read()
            .context("opening plan file without symlinks")?;
        ensure!(
            file.metadata()?.is_file(),
            "plan file must be a regular file"
        );
        let mut bytes = Vec::new();
        (&mut file)
            .take(max_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .context("reading plan file")?;
        Ok(bytes)
    }
}

fn read_regular_file(file: File) -> Result<Vec<u8>> {
    let mut file = file;
    ensure!(file.metadata()?.is_file(), "edit requires a regular file");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .context("reading edit target")?;
    Ok(bytes)
}

struct Temporary<'a> {
    parent: &'a File,
    name: &'a str,
}
impl Drop for Temporary<'_> {
    fn drop(&mut self) {
        let _ = fs::unlinkat(self.parent, self.name, AtFlags::empty());
    }
}

trait SaveOperations {
    fn write(&self, file: &mut File, content: &[u8]) -> io::Result<()> {
        file.write_all(content)
    }
    fn sync_file(&self, file: &File) -> io::Result<()> {
        file.sync_all()
    }
    fn commit(&self, parent: &File, temporary: &OsStr, target: &OsStr) -> io::Result<()> {
        Ok(fs::renameat(parent, temporary, parent, target)?)
    }
    /// Publishes the temporary file under the target name only when the
    /// target does not exist yet; without AT_SYMLINK_FOLLOW an existing
    /// target fails with `EEXIST` instead of being replaced.
    fn commit_new(&self, parent: &File, temporary: &OsStr, target: &OsStr) -> io::Result<()> {
        Ok(fs::linkat(
            parent,
            temporary,
            parent,
            target,
            AtFlags::empty(),
        )?)
    }
    fn sync_directory(&self, parent: &File) -> io::Result<()> {
        parent.sync_all()
    }
}
struct SystemSave;
impl SaveOperations for SystemSave {}

#[cfg(test)]
mod tests {
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
            "Plan path already exists; use /plan new."
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
}
