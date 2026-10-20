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

    fn components(&self, path: &Path) -> Result<Vec<OsString>> {
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

    pub(super) fn save(&self, content: &[u8]) -> Result<Option<String>> {
        self.save_with(content, &SystemSave)
    }

    fn save_with(
        &self,
        content: &[u8],
        operations: &impl SaveOperations,
    ) -> Result<Option<String>> {
        let (temporary, mut file) = loop {
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
                Ok(fd) => break (name, File::from(fd)),
                Err(Errno::EXIST) => continue,
                Err(error) => return Err(error).context("creating atomic save file"),
            }
        };
        let cleanup = Temporary {
            parent: &self.parent,
            name: &temporary,
        };
        operations
            .write(&mut file, content)
            .context("writing atomic save file; target unchanged")?;
        if let Some(mode) = self.mode {
            fs::fchmod(&file, mode).context("preserving target permissions; target unchanged")?;
        }
        operations
            .sync_file(&file)
            .context("syncing atomic save file; target unchanged")?;
        operations
            .commit(&self.parent, OsStr::new(&temporary), &self.name)
            .context("committing atomic save; target unchanged")?;
        // 重命名已经提交，后续错误只能作为持久化警告，不能伪装成未修改。
        std::mem::forget(cleanup);
        Ok(operations
            .sync_directory(&self.parent)
            .err()
            .map(|error| format!("file committed, but syncing parent directory failed: {error}")))
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

    #[derive(Clone, Copy)]
    enum Fault {
        Write,
        Sync,
        Commit,
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
        fn sync_directory(&self, parent: &File) -> io::Result<()> {
            if matches!(self.0, Fault::Directory) {
                return Err(failure());
            }
            SystemSave.sync_directory(parent)
        }
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
                assert!(target.save_with(b"replacement", &FaultSave(fault)).is_err());
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
            .save_with(b"new", &FaultSave(Fault::Directory))
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
}
