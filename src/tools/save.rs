use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{self, AtFlags, FileType, Mode, OFlags},
    io::Errno,
};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

pub(crate) struct Workspace {
    root: File,
    pub(crate) path: PathBuf,
    original: PathBuf,
}

pub(crate) struct Target {
    parent: File,
    name: OsString,
    mode: Option<Mode>,
}

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// `save_new` 碰撞的统一计划文案；计划正文与登记表都不得覆盖既有文件，
/// 也不得把碰撞伪装成保存失败以外的其他错误。
const PLAN_SAVE_NEW_CONFLICT: &str = "Plan file name already exists; choose another file_name.";

impl Workspace {
    pub(crate) fn open(path: &Path) -> Result<Self> {
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

    pub(crate) fn components(&self, path: &Path) -> Result<Vec<OsString>> {
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

    pub(crate) fn directory(&self, parts: &[OsString], create: bool) -> Result<File> {
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

    /// 计划生命周期搬迁的第一步：核对源普通文件与精确字节后，用 `linkat`
    /// 在目标位置发布同一 inode 的硬链接；绝不复制覆盖。已存在的目标仅在
    /// 同设备/inode 时允许幂等重入（返回 `None`）；不同 inode 的同名、符号
    /// 链接、路径逃逸与跨文件系统一律失败并保留源文件。提交后的父目录
    /// 同步失败只作为警告返回，不撤回已发布的链接。
    pub(crate) fn link_plan(
        &self,
        from: &Path,
        to: &Path,
        expected: &[u8],
    ) -> Result<Option<String>> {
        let source = self.target(from, false)?;
        source
            .compare_bytes(expected)
            .context("Plan source changed during the move; the source file is kept.")?;
        let identity = source
            .stat_optional()?
            .context("Plan source vanished during the move; the source file is kept.")?;
        let destination = self.target(to, true)?;
        match fs::linkat(
            &source.parent,
            &source.name,
            &destination.parent,
            &destination.name,
            AtFlags::empty(),
        ) {
            Ok(()) => {}
            Err(Errno::EXIST) => {
                // 幂等重入：仅同设备/inode 的既有目标允许；字节相同的不同
                // 文件同样冲突，绝不覆盖。
                let current = destination.stat_optional()?;
                ensure!(
                    current == Some(identity),
                    "Plan target already exists with different content; the source file is kept."
                );
                return Ok(None);
            }
            Err(error) => return Err(error).context("publishing the plan link"),
        }
        // 提交后核对目标身份：源名在核对与发布之间被换成符号链接时，
        // `linkat` 会链接符号链接本身；此时移除刚创建的目标名并报错，
        // 源文件保持原样。
        if destination.stat_optional()? != Some(identity) {
            let _ = fs::unlinkat(&destination.parent, &destination.name, AtFlags::empty());
            anyhow::bail!("Plan move published an unverified link; the source file is kept.");
        }
        Ok(destination.parent.sync_all().err().map(|error| {
            format!("plan link committed, but syncing parent directory failed: {error}")
        }))
    }

    /// 计划生命周期搬迁的提交步骤：确认目标与源仍同 inode 且字节精确匹配
    /// 后移除源名。源已缺失且目标字节正确视为已完成（幂等重入）；符号
    /// 链接、路径逃逸与字节/身份不一致一律失败并保留现状。提交后的父
    /// 目录同步失败只作为警告返回。
    pub(crate) fn finish_plan_move(
        &self,
        from: &Path,
        to: &Path,
        expected: &[u8],
    ) -> Result<Option<String>> {
        let destination = self.target(to, true)?;
        // `target` 对缺失的最终文件名也返回 Ok，这里用 NOFOLLOW stat 区分
        // 源名消失（幂等完成语义）与目录缺失/符号链接等真实错误。
        let source = match self.target(from, false) {
            Ok(source) => source.stat_optional()?.map(|_| source),
            Err(error) if is_missing(&error) => None,
            Err(error) => return Err(error),
        };
        let Some(source) = source else {
            // 源已消失：目标存在且字节精确匹配才算完成。
            ensure!(
                destination.stat_optional()?.is_some(),
                "Plan move did not complete; neither the source nor the target exists."
            );
            destination.compare_bytes(expected).context(
                "Plan target does not match the moved content; refusing to report completion.",
            )?;
            return Ok(None);
        };
        source
            .compare_bytes(expected)
            .context("Plan source changed during the move; both files are kept.")?;
        let source_identity = source
            .stat_optional()?
            .context("Plan source vanished during the move; both files are kept.")?;
        let destination_identity = destination
            .stat_optional()?
            .context("Plan target vanished during the move; the source file is kept.")?;
        ensure!(
            source_identity == destination_identity,
            "Plan target does not match the source file; the source file is kept."
        );
        fs::unlinkat(&source.parent, &source.name, AtFlags::empty())
            .context("removing the source plan link")?;
        Ok(source.parent.sync_all().err().map(|error| {
            format!("plan source removed, but syncing parent directory failed: {error}")
        }))
    }

    pub(super) fn cache_dir(&self) -> Result<PathBuf> {
        self.directory(&[OsString::from(".pp-cache")], true)?;
        Ok(self.path.join(".pp-cache"))
    }

    pub(crate) fn target(&self, path: &Path, create_parents: bool) -> Result<Target> {
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

    pub(crate) fn read(&self) -> Result<Vec<u8>> {
        let file = self
            .open_read()
            .context("opening edit target without symlinks")?;
        read_regular_file(file)
    }

    /// Optional snapshot for diff display. Only a genuinely missing target is
    /// `None`; every other failure (permissions, IO, race with a symlink swap)
    /// is a real error so callers can fall back to `Unavailable` instead of
    /// inventing an empty baseline.
    pub(crate) fn read_optional(&self) -> Result<Option<Vec<u8>>> {
        match self.open_read() {
            Ok(file) => read_regular_file(file).map(Some),
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(error).context("reading previous content for diff"),
        }
    }

    /// Compares exact bytes through a fresh NOFOLLOW handle without allocating
    /// another copy of the file. Missing, unreadable and non-regular targets
    /// are conflicts just like a content mismatch.
    pub(crate) fn ensure_matches(&self, expected: &[u8]) -> Result<()> {
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

    pub(crate) fn save(&self, content: &[u8]) -> Result<Option<String>> {
        self.save_with(content, &SystemSave, false)
    }

    /// 在临时文件同步之后检查读取时的基线，再原子提交。
    pub(crate) fn save_if_unchanged(
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
    pub(crate) fn restore_if_unchanged(
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
    pub(crate) fn save_new(&self, content: &[u8]) -> Result<Option<String>> {
        self.save_new_with_conflict(content, PLAN_SAVE_NEW_CONFLICT)
    }

    /// 与 `save_new` 完全相同的排他创建（linkat、不覆盖），但碰撞文案由调用方
    /// 提供；计划登记表等非正文保存用它避免误导性的计划提示。
    pub(crate) fn save_new_with_conflict(
        &self,
        content: &[u8],
        conflict: &'static str,
    ) -> Result<Option<String>> {
        self.save_checked_conflict(content, &SystemSave, true, None, conflict)
    }

    /// 与 `save_if_unchanged` 完全相同的基线比较保存，但冲突文案由调用方提供。
    pub(crate) fn save_if_unchanged_custom(
        &self,
        expected: &[u8],
        content: &[u8],
        conflict: &'static str,
    ) -> Result<Option<String>> {
        self.save_checked_conflict(
            content,
            &SystemSave,
            false,
            Some((expected, conflict)),
            PLAN_SAVE_NEW_CONFLICT,
        )
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
        self.save_checked_conflict(
            content,
            operations,
            new_only,
            expected,
            PLAN_SAVE_NEW_CONFLICT,
        )
    }

    fn save_checked_conflict(
        &self,
        content: &[u8],
        operations: &impl SaveOperations,
        new_only: bool,
        expected: Option<(&[u8], &'static str)>,
        new_only_conflict: &'static str,
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
                anyhow::bail!(new_only_conflict);
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
    pub(crate) fn read_bounded(&self, max_bytes: usize) -> Result<Vec<u8>> {
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

    /// 通过 NOFOLLOW 句柄读取目标的 device/inode；缺失返回 `None`，
    /// 符号链接与非普通文件报错。供计划身份登记的墓碑记录与 inode 确认使用。
    pub(crate) fn stat_optional(&self) -> Result<Option<(u64, u64)>> {
        match self.open_read() {
            Ok(file) => {
                let metadata = file.metadata().context("stating plan file")?;
                ensure!(metadata.is_file(), "plan file must be a regular file");
                Ok(Some((metadata.dev(), metadata.ino())))
            }
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(error).context("stating plan file"),
        }
    }

    /// 通过 NOFOLLOW 句柄读取目标身份与修改时间；缺失返回 `None`，符号
    /// 链接与非普通文件报错。供管理页列表展示 `modified` 使用。
    pub(crate) fn stat_optional_detailed(&self) -> Result<Option<(u64, u64, SystemTime)>> {
        match self.open_read() {
            Ok(file) => {
                let metadata = file.metadata().context("stating plan file")?;
                ensure!(metadata.is_file(), "plan file must be a regular file");
                Ok(Some((
                    metadata.dev(),
                    metadata.ino(),
                    metadata.modified().context("reading plan file mtime")?,
                )))
            }
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(error).context("stating plan file"),
        }
    }

    /// 通过已解析的父目录句柄安全移除目标名；不跟随符号链接，仅删除普通
    /// 文件。调用者负责先核对内容与身份（精确字节/同 inode）；缺失即报错，
    /// 由调用方决定如何向上报告。
    pub(crate) fn unlink(&self) -> Result<()> {
        let file = self
            .open_read()
            .context("opening target for removal without symlinks")?;
        ensure!(
            file.metadata()
                .context("checking target for removal")?
                .is_file(),
            "target must be a regular file"
        );
        fs::unlinkat(&self.parent, &self.name, AtFlags::empty()).context("removing target")?;
        Ok(())
    }
}

/// 判断错误链中是否只是“路径不存在”；搬迁源缺失属于幂等完成语义。
fn is_missing(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<Errno>() == Some(&Errno::NOENT))
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
mod tests;
