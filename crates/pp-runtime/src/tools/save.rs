use anyhow::{Context, Result, ensure};
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::check;
use rustix::{
    fs::{self, AtFlags, Dir, FileType, Mode, OFlags, RawMode},
    io::Errno,
};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{self, Read, Write},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
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

/// 资源事务递归操作的上限：普通文件最多 1000 个。目录本身不占配额——
/// 一个普通文件配一个子目录的合法树仍要能通过；符号链接与特殊文件在
/// 逐节点检查中直接拒绝。
pub(crate) const RESOURCE_FILE_LIMIT: usize = 1000;

/// 目录深度上限：只为防止空目录链无限递归/爆栈（文件配额已不再约束
/// 目录），不是文件数量限制。
const RESOURCE_MAX_DEPTH: usize = 1000;

/// 排他新建的冲突说明；计划保存与普通 write 各自提供明确原因。
#[derive(Clone, Copy)]
enum NewConflict {
    Message(&'static str),
}

/// 资源节点类型：`lstat` 语义，不跟随符号链接。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File,
    Directory,
    Missing,
}

/// 目录快照的一项：普通文件带内容，目录内容为 `None`。相对路径用于
/// 重命名两端与递归删除的逐节点记录；`permissions` 是快照时的权限位
/// （`st_mode & 0o7777`），重建时用来保留可执行位，不能丢。
pub(crate) struct DirectoryEntry {
    pub(crate) relative: PathBuf,
    pub(crate) content: Option<Vec<u8>>,
    pub(crate) permissions: Option<u32>,
}

/// 只读资源句柄：父目录按 NOFOLLOW 打开，读取时核对最终节点类型。
pub(crate) struct ResourceTarget {
    parent: File,
    name: OsString,
    mode: Option<Mode>,
}

/// `/tmp` 的真实前缀：macOS 的 `/tmp` 指向该目录，路径判断把两种拼写
/// 归一后再与工作区根比对，且无论工作区根在哪里都只影响 `/tmp` 前缀。
const TMP_REAL_ROOT: &str = "/private/tmp";

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
        // macOS：`/tmp` 是符号链接，canonical 化后落在 `/private/tmp`。
        // `components` 把 `/tmp/...` 拼写替换成真实前缀后归入同一根，
        // 路径判断不再因根的两种拼写分叉误判。
        Ok(Self {
            root,
            path,
            original,
        })
    }

    /// 把 `/tmp/...` 别名拼写替换成真实前缀；纯值变换，不访问文件系统。
    /// 折返的是前缀本身：`/tmp/a` 变 `/private/tmp/a`，再与工作区根做
    /// 前缀判断。无别名或前缀不匹配时原样返回。
    fn demote_alias(&self, path: &Path) -> PathBuf {
        match path.strip_prefix("/tmp") {
            Ok(relative) => {
                let mut demoted = PathBuf::from(TMP_REAL_ROOT);
                demoted.push(relative);
                demoted
            }
            Err(_) => path.to_path_buf(),
        }
    }

    pub(crate) fn components(&self, path: &Path) -> Result<Vec<OsString>> {
        let path = self.demote_alias(path);
        let relative = if path.is_absolute() {
            path.strip_prefix(&self.original)
                .or_else(|_| path.strip_prefix(&self.path))
                .context("write/edit path must stay inside startup directory")?
        } else {
            path.as_path()
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
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut parent = self.root.try_clone()?;
        for part in parts {
            let next = match fs::openat(&parent, part, flags, Mode::empty()) {
                Ok(fd) => File::from(fd),
                Err(Errno::NOENT) if create => {
                    match fs::mkdirat(&parent, part, Mode::from_raw_mode(0o777)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(error) => return Err(error).context("creating parent directory"),
                    }
                    File::from(
                        fs::openat(&parent, part, flags, Mode::empty())
                            .context("opening created directory without symlinks")?,
                    )
                }
                Err(error) => {
                    return Err(error)
                        .context("opening parent directory (symlink paths are forbidden)");
                }
            };
            parent = next;
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

    /// 资源事务入口：解析最终名的父目录。`create=false` 时缺失的父目录
    /// 表示目标尚不存在（`Ok(None)`），preview 因此不会创建任何目录；
    /// 已存在的每一级都以 NOFOLLOW 目录句柄验证，符号链接祖先仍是错误。
    pub(crate) fn resource_target(
        &self,
        path: &Path,
        create: bool,
    ) -> Result<Option<ResourceTarget>> {
        let mut parts = self.components(path)?;
        let name = parts.pop().context("resource path requires a file name")?;
        let parent = match self.directory(&parts, create) {
            Ok(parent) => parent,
            Err(error) if !create && is_missing(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let mode = match fs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {
                Some(Mode::from_raw_mode(stat.st_mode))
            }
            _ => None,
        };
        Ok(Some(ResourceTarget { parent, name, mode }))
    }

    /// 与 [`Self::walk_directory`] 相同，但在每个节点前检查取消信号，让
    /// 长目录遍历可以中止而不是先读完整个快照。
    pub(crate) fn walk_directory_cancellable(
        &self,
        path: &Path,
        limit: usize,
        cancellation: &CancellationToken,
    ) -> Result<Vec<DirectoryEntry>> {
        self.walk_directory_bounded(path, limit, Some(cancellation))
    }

    fn walk_directory_bounded(
        &self,
        path: &Path,
        limit: usize,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Vec<DirectoryEntry>> {
        let target = self
            .resource_target(path, false)?
            .context("directory does not exist")?;
        ensure!(
            target.kind()? == EntryKind::Directory,
            "resource path must be a directory"
        );
        let dir = target.open_directory()?;
        let mut entries = Vec::new();
        let mut files = 0_usize;
        walk_entries(
            &dir,
            Path::new(""),
            &mut entries,
            &mut files,
            limit,
            cancellation,
        )?;
        Ok(entries)
    }
}

/// 递归快照的公用遍历：调用方已经用 NOFOLLOW 打开目录句柄。
fn walk_entries(
    dir: &File,
    prefix: &Path,
    entries: &mut Vec<DirectoryEntry>,
    files: &mut usize,
    limit: usize,
    cancellation: Option<&CancellationToken>,
) -> Result<()> {
    let mut names = Vec::new();
    for entry in Dir::read_from(dir)? {
        let entry = entry.context("listing directory")?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        names.push(OsStr::from_bytes(name.to_bytes()).to_os_string());
    }
    names.sort();
    for name in names {
        if let Some(cancellation) = cancellation {
            check(cancellation)?;
        }
        let relative = prefix.join(&name);
        let stat = fs::statat(dir, &name, AtFlags::SYMLINK_NOFOLLOW)?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile => {
                // 只有普通文件占配额；目录不计数，合法树不会被目录数挤爆。
                *files += 1;
                ensure!(
                    *files <= limit,
                    "directory contains more than {limit} regular files; resource operation refused"
                );
                let file = File::from(fs::openat(
                    dir,
                    &name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )?);
                entries.push(DirectoryEntry {
                    relative,
                    content: Some(read_regular_file(file)?),
                    permissions: Some(permission_bits(stat.st_mode)),
                });
            }
            FileType::Directory => {
                ensure!(
                    relative.components().count() <= RESOURCE_MAX_DEPTH,
                    "directory nesting exceeds {RESOURCE_MAX_DEPTH} levels; resource operation refused"
                );
                let file = File::from(fs::openat(
                    dir,
                    &name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?);
                entries.push(DirectoryEntry {
                    relative: relative.clone(),
                    content: None,
                    permissions: Some(permission_bits(stat.st_mode)),
                });
                walk_entries(&file, &relative, entries, files, limit, cancellation)?;
            }
            _ => anyhow::bail!(
                "directory contains a symlink or special file; resource operation refused"
            ),
        }
    }
    Ok(())
}

impl ResourceTarget {
    fn as_target(&self) -> Result<Target> {
        Ok(Target {
            parent: self.parent.try_clone()?,
            name: self.name.clone(),
            mode: self.mode,
        })
    }

    /// NOFOLLOW `lstat`：缺失返回 `Missing`；符号链接与特殊文件是错误，
    /// 不把“看起来像文件”的名字当作普通文件。
    pub(crate) fn kind(&self) -> Result<EntryKind> {
        match fs::statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile => Ok(EntryKind::File),
                FileType::Directory => Ok(EntryKind::Directory),
                _ => anyhow::bail!(
                    "resource target must be a regular file or directory; symlinks and special files are forbidden"
                ),
            },
            Err(Errno::NOENT) => Ok(EntryKind::Missing),
            Err(error) => Err(error).context("checking resource target"),
        }
    }

    /// 当前目标的权限位（`st_mode & 0o7777`，NOFOLLOW）；缺失返回 `None`，
    /// 符号链接与特殊文件是错误。快照与回滚重建都用它保留可执行位。
    pub(crate) fn permissions(&self) -> Result<Option<u32>> {
        match fs::statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile | FileType::Directory => {
                    Ok(Some(permission_bits(stat.st_mode)))
                }
                _ => anyhow::bail!(
                    "resource target must be a regular file or directory; symlinks and special files are forbidden"
                ),
            },
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(error).context("reading resource permissions"),
        }
    }

    pub(crate) fn read(&self) -> Result<Vec<u8>> {
        self.as_target()?.read()
    }

    /// 以 NOFOLLOW 打开目录本身，供递归遍历与移除。
    pub(crate) fn open_directory(&self) -> Result<File> {
        let fd = fs::openat(
            &self.parent,
            &self.name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context("opening directory without symlinks")?;
        Ok(File::from(fd))
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
        self.save_checked_conflict(
            content,
            &SystemSave,
            true,
            None,
            NewConflict::Message(conflict),
            None,
        )
    }

    /// 与 `save_if_unchanged` 完全相同的基线比较保存，但冲突文案由调用方提供。
    pub(crate) fn save_if_unchanged_custom(
        &self,
        expected: &[u8],
        content: &[u8],
        conflict: &'static str,
    ) -> Result<Option<String>> {
        self.save_if_unchanged_custom_with_permissions(expected, content, conflict, None)
    }

    /// 与 [`Self::save_if_unchanged_custom`] 相同，但显式设置提交后的权限位；
    /// 用于回滚时连可执行位一起还原。
    pub(crate) fn save_if_unchanged_custom_with_permissions(
        &self,
        expected: &[u8],
        content: &[u8],
        conflict: &'static str,
        permissions: Option<u32>,
    ) -> Result<Option<String>> {
        self.save_checked_conflict(
            content,
            &SystemSave,
            false,
            Some((expected, conflict)),
            NewConflict::Message(PLAN_SAVE_NEW_CONFLICT),
            permissions,
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
            NewConflict::Message(PLAN_SAVE_NEW_CONFLICT),
            None,
        )
    }

    fn save_checked_conflict(
        &self,
        content: &[u8],
        operations: &impl SaveOperations,
        new_only: bool,
        expected: Option<(&[u8], &'static str)>,
        new_only_conflict: NewConflict,
        permissions: Option<u32>,
    ) -> Result<Option<String>> {
        let (temporary, mut file) = self.create_temporary()?;
        let cleanup = Temporary {
            parent: &self.parent,
            name: &temporary,
        };
        operations
            .write(&mut file, content)
            .context("writing atomic save file; target unchanged")?;
        // 显式权限位优先（回滚重建被删/被移动的节点）；否则非排他提交保留
        // 既有目标权限，排他创建交给 umask。
        let target_mode = match permissions {
            Some(bits) => Some(Mode::from_raw_mode(bits as RawMode)),
            None if !new_only => self.mode,
            None => None,
        };
        if let Some(mode) = target_mode {
            fs::fchmod(&file, mode).context("setting target permissions; target unchanged")?;
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
                match new_only_conflict {
                    NewConflict::Message(message) => anyhow::bail!(message),
                }
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

/// `st_mode` 的权限位（含 setuid/setgid/sticky 的低 12 位），供快照重建时
/// 还原可执行位；不把文件类型位带进权限。
fn permission_bits(mode: RawMode) -> u32 {
    (mode as u32) & 0o7777
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
