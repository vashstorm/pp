mod walk;

use anyhow::{Context, Result, ensure};
use pp_core::cancellation::CancellationToken;
use rustix::{
    fs::{self, AtFlags, FileType, Mode, OFlags, RawMode},
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
pub use walk::{DirectoryEntry, EntryKind, RESOURCE_BYTE_LIMIT, RESOURCE_FILE_LIMIT};
use walk::{WalkBudget, walk_entries};

/// 保存层的工作区句柄：启动目录是主受管根，临时目录（`/tmp` 与其真实
/// 路径）是唯一额外的受管根。两者都用 NOFOLLOW 逐级打开的目录 fd 承接
/// 写入，符号链接与逃逸路径仍一律拒绝。
pub struct Workspace {
    root: File,
    pub path: PathBuf,
    temp: Option<TempRoot>,
}

/// 临时目录受管根：`canonicalize` 后的真实路径加目录 fd。
struct TempRoot {
    root: File,
    path: PathBuf,
}

/// 受管根：启动目录本身，或临时目录。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Anchor {
    Workspace,
    Temp,
}

/// 一次路径解析的结果：路径归属哪个受管根、该根的目录 fd 与真实路径，
/// 以及相对该根的分量。
struct Location<'a> {
    anchor: Anchor,
    root: &'a File,
    base: &'a Path,
    parts: Vec<OsString>,
}

#[derive(Debug)]
pub struct Target {
    parent: File,
    name: OsString,
    mode: Option<Mode>,
}

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// `save_new` 碰撞的统一计划文案；计划正文与登记表都不得覆盖既有文件，
/// 也不得把碰撞伪装成保存失败以外的其他错误。
const PLAN_SAVE_NEW_CONFLICT: &str = "Plan file name already exists; choose another file_name.";

/// 排他新建的冲突说明；计划保存与普通 write 各自提供明确原因。
#[derive(Clone, Copy)]
enum NewConflict {
    Message(&'static str),
}

/// 只读资源句柄：父目录按 NOFOLLOW 打开，读取时核对最终节点类型。
pub struct ResourceTarget {
    parent: File,
    name: OsString,
    mode: Option<Mode>,
}

/// 临时目录的公开拼写：macOS 上它是 `/private/tmp` 的符号链接，Linux 上
/// 通常就是真实目录；两种拼写都归入同一受管根。
const TEMP_ALIAS: &str = "/tmp";

impl Workspace {
    pub fn open(path: &Path) -> Result<Self> {
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
            temp: TempRoot::open(),
        })
    }

    /// 把 `/tmp/...` 折叠成临时根的真实前缀；其他拼写原样返回。纯值变换，
    /// 不访问文件系统，`/tmp/a` 与 `/private/tmp/a` 因此得到同一路径。
    fn fold_temp(&self, path: &Path) -> PathBuf {
        match (&self.temp, path.strip_prefix(TEMP_ALIAS)) {
            (Some(temp), Ok(relative)) => temp.path.join(relative),
            _ => path.to_path_buf(),
        }
    }

    /// 工作区根内的相对路径；绝对路径落在工作区之外时返回 `None`。
    fn workspace_relative<'a>(&self, folded: &'a Path) -> Option<&'a Path> {
        if folded.is_absolute() {
            folded.strip_prefix(&self.path).ok()
        } else {
            Some(folded)
        }
    }

    /// 解析受管路径：工作区根优先，其次临时根；两者之外一律拒绝。相对路径
    /// 始终留在工作区，不因临时根存在而改变含义。
    fn resolve(&self, path: &Path) -> Result<Location<'_>> {
        let folded = self.fold_temp(path);
        if let Some(relative) = self.workspace_relative(&folded) {
            return Ok(Location {
                anchor: Anchor::Workspace,
                root: &self.root,
                base: &self.path,
                parts: split_parts(relative)?,
            });
        }
        if let Some(temp) = &self.temp
            && let Ok(relative) = folded.strip_prefix(&temp.path)
        {
            return Ok(Location {
                anchor: Anchor::Temp,
                root: &temp.root,
                base: &temp.path,
                parts: split_parts(relative)?,
            });
        }
        anyhow::bail!("path must stay inside the startup directory or /tmp")
    }

    /// 只接受工作区根内的路径分量。Plan 与 LSP 的结构校验用它，临时根路径
    /// 不会被误当作工作区相对路径。
    pub fn components(&self, path: &Path) -> Result<Vec<OsString>> {
        let folded = self.fold_temp(path);
        let relative = self
            .workspace_relative(&folded)
            .context("path must stay inside the startup directory")?;
        split_parts(relative)
    }

    /// 变更、撤销与 hashline 快照使用的规范键：工作区根内是相对路径，临时
    /// 根内是真实绝对路径，`/tmp/a` 与 `/private/tmp/a` 因此落到同一个键。
    pub fn key(&self, path: &Path) -> Result<PathBuf> {
        let location = self.resolve(path)?;
        let relative = location.parts.iter().collect::<PathBuf>();
        Ok(match location.anchor {
            Anchor::Workspace => relative,
            Anchor::Temp => location.base.join(relative),
        })
    }

    pub fn directory(&self, parts: &[OsString], create: bool) -> Result<File> {
        open_contained_directory(&self.root, parts, create)
    }

    /// 计划生命周期搬迁的第一步：核对源普通文件与精确字节后，用 `linkat`
    /// 在目标位置发布同一 inode 的硬链接；绝不复制覆盖。已存在的目标仅在
    /// 同设备/inode 时允许幂等重入（返回 `None`）；不同 inode 的同名、符号
    /// 链接、路径逃逸与跨文件系统一律失败并保留源文件。发布后的身份核对
    /// 把 Err、不符与缺失统一送入清理分支：先移除刚发布的目标名再返回
    /// 固定错误，不把指向外部的残留链接留在盘上。提交后的父目录同步失败
    /// 只作为警告返回，不撤回已发布的链接。
    pub fn link_plan(&self, from: &Path, to: &Path, expected: &[u8]) -> Result<Option<String>> {
        self.link_plan_impl(from, to, expected, &SystemPlanLink)
    }

    /// 与 [`Self::link_plan`] 相同的搬迁流程，但发布动作可注入：仅供测试
    /// 构造“核对与发布之间源名被替换”的确定交错，不进入生产构建。
    #[cfg(test)]
    fn link_plan_with(
        &self,
        from: &Path,
        to: &Path,
        expected: &[u8],
        operations: &dyn PlanLinkOperations,
    ) -> Result<Option<String>> {
        self.link_plan_impl(from, to, expected, operations)
    }

    /// 搬迁的实现：发布动作由端口提供，生产实现是 `linkat`。
    fn link_plan_impl(
        &self,
        from: &Path,
        to: &Path,
        expected: &[u8],
        operations: &dyn PlanLinkOperations,
    ) -> Result<Option<String>> {
        let source = self.target(from, false)?;
        source
            .compare_bytes(expected)
            .context("Plan source changed during the move; the source file is kept.")?;
        let identity = source
            .stat_optional()?
            .context("Plan source vanished during the move; the source file is kept.")?;
        let destination = self.target(to, true)?;
        match operations.publish(
            &source.parent,
            &source.name,
            &destination.parent,
            &destination.name,
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
        // 提交后核对目标身份：Err、身份不符与缺失都进入清理分支，先移除
        // 刚发布的目标名再返回固定错误，源文件保持原样。Err 包含源名被
        // 换成符号链接时 NOFOLLOW 打开的真实 IO 错误（ELOOP），不能把已
        // 发布的目标丢在盘上。
        if destination.stat_optional().ok().flatten() != Some(identity) {
            let _ = fs::unlinkat(&destination.parent, &destination.name, AtFlags::empty());
            anyhow::bail!("Plan move published an unverified link; the source file is kept.");
        }
        Ok(operations
            .sync_directory(&destination.parent)
            .err()
            .map(|error| {
                format!("plan link committed, but syncing parent directory failed: {error}")
            }))
    }

    /// 计划生命周期搬迁的提交步骤：确认目标与源仍同 inode 且字节精确匹配
    /// 后移除源名。源已缺失且目标字节正确视为已完成（幂等重入）；符号
    /// 链接、路径逃逸与字节/身份不一致一律失败并保留现状。提交后的父
    /// 目录同步失败只作为警告返回。
    pub fn finish_plan_move(
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

    pub fn cache_dir(&self) -> Result<PathBuf> {
        self.directory(&[OsString::from(".pp-cache")], true)?;
        Ok(self.path.join(".pp-cache"))
    }

    pub fn target(&self, path: &Path, create_parents: bool) -> Result<Target> {
        let Location {
            root, mut parts, ..
        } = self.resolve(path)?;
        let name = parts.pop().context("write/edit requires a file path")?;
        let parent = open_contained_directory(root, &parts, create_parents)?;
        open_target_at(parent, name)
    }

    /// 已获权限门显式放行的工作区外绝对路径保存入口：
    /// 保留与工作区内一致的 `O_NOFOLLOW` 逐级目录打开、常规文件核对与原子替换，
    /// 仅对 macOS 系统根别名（`/tmp`、`/var`、`/etc`）做前缀折叠。
    pub fn external_target(&self, path: &Path, create_parents: bool) -> Result<Target> {
        ensure!(
            path.is_absolute(),
            "external write/edit target must be an absolute path"
        );
        let folded = fold_system_root_alias(path);
        let relative = folded
            .strip_prefix(Path::new("/"))
            .context("external write/edit path must be rooted")?;
        let mut parts = split_parts(relative)?;
        let name = parts.pop().context("write/edit requires a file path")?;
        let fs_root = File::open("/").context("opening filesystem root")?;
        let parent = open_contained_directory(&fs_root, &parts, create_parents)?;
        open_target_at(parent, name)
    }

    /// 资源事务入口：解析最终名的父目录。`create=false` 时缺失的父目录
    /// 表示目标尚不存在（`Ok(None)`），preview 因此不会创建任何目录；
    /// 已存在的每一级都以 NOFOLLOW 目录句柄验证，符号链接祖先仍是错误。
    pub fn resource_target(&self, path: &Path, create: bool) -> Result<Option<ResourceTarget>> {
        let Location {
            root, mut parts, ..
        } = self.resolve(path)?;
        let name = parts.pop().context("resource path requires a file name")?;
        let parent = match open_contained_directory(root, &parts, create) {
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
    pub fn walk_directory_cancellable(
        &self,
        path: &Path,
        limit: usize,
        byte_budget: u64,
        cancellation: &CancellationToken,
    ) -> Result<Vec<DirectoryEntry>> {
        self.walk_directory_bounded(path, limit, byte_budget, Some(cancellation))
    }

    fn walk_directory_bounded(
        &self,
        path: &Path,
        limit: usize,
        byte_budget: u64,
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
        let mut budget = WalkBudget::new(limit, byte_budget);
        walk_entries(&dir, Path::new(""), &mut entries, &mut budget, cancellation)?;
        Ok(entries)
    }
}

impl TempRoot {
    /// `canonicalize` 把 `/tmp` 解析成真实目录（macOS 为 `/private/tmp`）。
    /// 缺失或不是目录时不提供临时根，`/tmp` 写入回退为越界拒绝。
    fn open() -> Option<Self> {
        let path = Path::new(TEMP_ALIAS).canonicalize().ok()?;
        let root = File::open(&path).ok()?;
        root.metadata()
            .ok()?
            .is_dir()
            .then_some(Self { root, path })
    }
}

fn open_target_at(parent: File, name: OsString) -> Result<Target> {
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

#[cfg(target_os = "macos")]
fn fold_system_root_alias(path: &Path) -> PathBuf {
    for alias in ["/tmp", "/var", "/etc"] {
        if let Ok(relative) = path.strip_prefix(alias)
            && let Ok(canon) = Path::new(alias).canonicalize()
        {
            return canon.join(relative);
        }
    }
    path.to_path_buf()
}

#[cfg(not(target_os = "macos"))]
fn fold_system_root_alias(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// 从给定受管根逐级以 NOFOLLOW 打开目录，`create` 时建出缺失的一级。
fn open_contained_directory(root: &File, parts: &[OsString], create: bool) -> Result<File> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut parent = root.try_clone()?;
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

/// 把相对路径拆成普通分量；`..` 只能弹回已有分量，绝不越过受管根。
fn split_parts(relative: &Path) -> Result<Vec<OsString>> {
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
    pub fn kind(&self) -> Result<EntryKind> {
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
    pub fn permissions(&self) -> Result<Option<u32>> {
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

    pub fn read(&self) -> Result<Vec<u8>> {
        self.as_target()?.read()
    }

    /// 以 NOFOLLOW 打开目录本身，供递归遍历与移除。
    pub fn open_directory(&self) -> Result<File> {
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

    /// 通过已解析的父目录句柄打开普通文件，拒绝符号链接与特殊文件。
    pub fn open_regular_read(&self) -> Result<File> {
        let file = self.open_read().context("opening file without symlinks")?;
        ensure!(file.metadata()?.is_file(), "target must be a regular file");
        Ok(file)
    }

    pub fn read(&self) -> Result<Vec<u8>> {
        let file = self
            .open_read()
            .context("opening edit target without symlinks")?;
        read_regular_file(file)
    }

    /// Optional snapshot for diff display. Only a genuinely missing target is
    /// `None`; every other failure (permissions, IO, race with a symlink swap)
    /// is a real error so callers can fall back to `Unavailable` instead of
    /// inventing an empty baseline.
    pub fn read_optional(&self) -> Result<Option<Vec<u8>>> {
        match self.open_read() {
            Ok(file) => read_regular_file(file).map(Some),
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(error).context("reading previous content for diff"),
        }
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

    pub fn save(&self, content: &[u8]) -> Result<Option<String>> {
        self.save_with(content, &SystemSave, false)
    }

    /// 在临时文件同步之后检查读取时的基线，再原子提交。
    pub fn save_if_unchanged(&self, expected: &[u8], content: &[u8]) -> Result<Option<String>> {
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

    /// Atomically creates the target only when it does not already exist.
    /// A concurrent or pre-existing target is never overwritten; `EEXIST`
    /// becomes the exact plan-collision error so callers stay in Plan mode.
    pub fn save_new(&self, content: &[u8]) -> Result<Option<String>> {
        self.save_new_with_conflict(content, PLAN_SAVE_NEW_CONFLICT)
    }

    /// 与 `save_new` 完全相同的排他创建（linkat、不覆盖），但碰撞文案由调用方
    /// 提供；计划登记表等非正文保存用它避免误导性的计划提示。
    pub fn save_new_with_conflict(
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
        )
    }

    /// 与 `save_if_unchanged` 完全相同的基线比较保存，但冲突文案由调用方提供。
    pub fn save_if_unchanged_custom(
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
            NewConflict::Message(PLAN_SAVE_NEW_CONFLICT),
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
        )
    }

    fn save_checked_conflict(
        &self,
        content: &[u8],
        operations: &impl SaveOperations,
        new_only: bool,
        expected: Option<(&[u8], &'static str)>,
        new_only_conflict: NewConflict,
    ) -> Result<Option<String>> {
        let (temporary, mut file) = self.create_temporary()?;
        let cleanup = Temporary {
            parent: &self.parent,
            name: &temporary,
        };
        operations
            .write(&mut file, content)
            .context("writing atomic save file; target unchanged")?;
        // 非排他提交保留既有目标权限，排他创建交给 umask。
        if !new_only && let Some(mode) = self.mode {
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
    pub fn read_bounded(&self, max_bytes: usize) -> Result<Vec<u8>> {
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
    pub fn stat_optional(&self) -> Result<Option<(u64, u64)>> {
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
    pub fn stat_optional_detailed(&self) -> Result<Option<(u64, u64, SystemTime)>> {
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
    pub fn unlink(&self) -> Result<()> {
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
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "edit requires a regular file");
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
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

/// 计划链接发布的可注入端口：生产实现是 `linkat`，测试用它在发布前把
/// 源名换成符号链接，构造确定交错（类别：测试无法构造的外部副作用）。
trait PlanLinkOperations {
    fn publish(
        &self,
        source_parent: &File,
        source_name: &OsStr,
        destination_parent: &File,
        destination_name: &OsStr,
    ) -> rustix::io::Result<()> {
        fs::linkat(
            source_parent,
            source_name,
            destination_parent,
            destination_name,
            AtFlags::empty(),
        )
    }
    fn sync_directory(&self, parent: &File) -> io::Result<()> {
        parent.sync_all()
    }
}
struct SystemPlanLink;
impl PlanLinkOperations for SystemPlanLink {}

#[cfg(test)]
mod tests;
