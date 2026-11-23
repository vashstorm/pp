//! 目录资源遍历：跨递归的文件数与字节预算、快照条目类型。

use anyhow::{Context, Result, ensure};
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::check;
use rustix::fs::{self, AtFlags, Dir, FileType, Mode, OFlags};
use std::{
    ffi::OsStr,
    fs::File,
    io::Read,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use super::permission_bits;

/// 资源节点类型：`lstat` 语义，不跟随符号链接。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Missing,
}

/// 目录快照的一项：普通文件带内容，目录内容为 `None`。相对路径用于
/// 重命名两端与递归删除的逐节点记录；`permissions` 是快照时的权限位
/// （`st_mode & 0o7777`），重建时用来保留可执行位，不能丢。
pub struct DirectoryEntry {
    pub relative: PathBuf,
    pub content: Option<Vec<u8>>,
    pub permissions: Option<u32>,
}

/// 资源事务递归操作的上限：普通文件最多 1000 个。目录本身不占配额——
/// 一个普通文件配一个子目录的合法树仍要能通过；符号链接与特殊文件在
/// 逐节点检查中直接拒绝。
pub const RESOURCE_FILE_LIMIT: usize = 1000;

/// 目录深度上限：只为防止空目录链无限递归/爆栈（文件配额已不再约束
/// 目录），不是文件数量限制。
const RESOURCE_MAX_DEPTH: usize = 1000;

/// 目录资源操作的普通文件总字节预算：快照遍历跨递归累计，LSP 虚拟叠加
/// 结果用同一上限复核。超限只拒绝该次建议，不提交任何文件；不新增
/// 配置项，文件数与深度限制保持不变。
pub const RESOURCE_BYTE_LIMIT: u64 = 256 * 1024 * 1024;

/// 递归快照的累计用量：文件数、字节数与其上限一起下传，跨递归累计。
/// 超限即拒绝整次操作，调用方据此放弃建议，不写入任何文件。
pub(super) struct WalkBudget {
    file_limit: usize,
    byte_limit: u64,
    files: usize,
    bytes: u64,
}

impl WalkBudget {
    pub(super) fn new(file_limit: usize, byte_limit: u64) -> Self {
        Self {
            file_limit,
            byte_limit,
            files: 0,
            bytes: 0,
        }
    }

    /// 普通文件占一个文件额度；目录不计数，合法树不会被目录数挤爆。
    fn count_file(&mut self) -> Result<()> {
        self.files += 1;
        ensure!(
            self.files <= self.file_limit,
            "directory contains more than {} regular files; resource operation refused",
            self.file_limit
        );
        Ok(())
    }

    /// 剩余可读字节；已用量超过预算时不再继续。
    fn remaining(&self) -> Result<u64> {
        self.byte_limit
            .checked_sub(self.bytes)
            .context("resource byte budget already exceeded")
    }

    /// 读取前的 metadata 预检：文件长度超过剩余预算即拒绝，不能按超大
    /// metadata 直接分配；通过后返回可读取的剩余字节数。
    fn admit(&self, len: u64) -> Result<u64> {
        let remaining = self.remaining()?;
        ensure!(
            len <= remaining,
            "directory contains more than {} bytes of regular files; resource operation refused",
            self.byte_limit
        );
        Ok(remaining)
    }

    /// 按实际读到的字节数累计，不信任 metadata 的长度。
    fn record(&mut self, read: u64) {
        self.bytes += read;
    }
}

/// 递归快照的公用遍历：调用方已经用 NOFOLLOW 打开目录句柄。
pub(super) fn walk_entries(
    dir: &File,
    prefix: &Path,
    entries: &mut Vec<DirectoryEntry>,
    budget: &mut WalkBudget,
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
                budget.count_file()?;
                let file = File::from(fs::openat(
                    dir,
                    &name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )?);
                // 读取前先做 metadata 预检（按剩余预算拒绝超限文件），实际
                // 读取再以 `take(remaining + 1)` 有界进行。
                let metadata = file.metadata()?;
                ensure!(
                    metadata.is_file(),
                    "directory contains a non-regular file; resource operation refused"
                );
                let remaining = budget.admit(metadata.len())?;
                let content =
                    read_bounded_regular_file(file, remaining, "reading directory snapshot")?;
                budget.record(content.len() as u64);
                entries.push(DirectoryEntry {
                    relative,
                    content: Some(content),
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
                walk_entries(&file, &relative, entries, budget, cancellation)?;
            }
            _ => anyhow::bail!(
                "directory contains a symlink or special file; resource operation refused"
            ),
        }
    }
    Ok(())
}

/// 有界的普通文件读取：最多读 `max_bytes + 1` 字节，读取过程中文件增长也
/// 不会越过上限；分配量由实际读到的字节数决定，不按超大 metadata 预分配。
fn read_bounded_regular_file(file: File, max_bytes: u64, context: &'static str) -> Result<Vec<u8>> {
    let mut file = file;
    let mut bytes = Vec::new();
    (&mut file)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .context(context)?;
    ensure!(
        bytes.len() as u64 <= max_bytes,
        "file grew beyond the remaining byte budget; resource operation refused"
    );
    Ok(bytes)
}
