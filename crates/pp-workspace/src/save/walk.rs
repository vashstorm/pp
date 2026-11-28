//! Directory resource walk: cross-recursion file and byte budgets, snapshot entry types.

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

/// Resource node kind: `lstat` semantics, symlinks are not followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Missing,
}

/// One item of a directory snapshot: regular files carry content, directories carry `None`.
/// The relative path identifies each node for both rename sides and recursive deletion;
/// `permissions` holds the permission bits at snapshot time (`st_mode & 0o7777`) and is
/// required to preserve the executable bit on rebuilds.
pub struct DirectoryEntry {
    pub relative: PathBuf,
    pub content: Option<Vec<u8>>,
    pub permissions: Option<u32>,
}

/// Recursive resource operation limit: at most 1000 regular files. Directories do not
/// consume quota — a legal tree with one regular file per subdirectory must still pass;
/// symlinks and special files are rejected outright by the per-node check.
pub const RESOURCE_FILE_LIMIT: usize = 1000;

/// Directory depth limit: only guards against unbounded recursion/stack exhaustion on
/// chains of empty directories (the file quota no longer constrains directories), not a
/// file count limit.
const RESOURCE_MAX_DEPTH: usize = 1000;

/// Total byte budget for regular files in a directory resource operation: the snapshot
/// walk accumulates across recursion, and the LSP virtual overlay result is checked
/// against the same limit. Exceeding it rejects only that suggestion without writing any
/// file; no new config key is added and the file count and depth limits are unchanged.
pub const RESOURCE_BYTE_LIMIT: u64 = 256 * 1024 * 1024;

/// Accumulated usage of a recursive snapshot: file count, byte count and their limits are
/// threaded down and accumulated across recursion. Exceeding a limit rejects the whole
/// operation, so the caller drops the suggestion without writing any file.
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

    /// A regular file consumes one file quota; directories are not counted, so a legal tree
    /// is never rejected because of its directory count.
    fn count_file(&mut self) -> Result<()> {
        self.files += 1;
        ensure!(
            self.files <= self.file_limit,
            "directory contains more than {} regular files; resource operation refused",
            self.file_limit
        );
        Ok(())
    }

    /// Readable bytes left; no further progress once usage exceeds the budget.
    fn remaining(&self) -> Result<u64> {
        self.byte_limit
            .checked_sub(self.bytes)
            .context("resource byte budget already exceeded")
    }

    /// Metadata pre-check before reading: reject when the file length exceeds the remaining
    /// budget instead of allocating straight from oversized metadata; on success, returns the
    /// number of readable bytes left.
    fn admit(&self, len: u64) -> Result<u64> {
        let remaining = self.remaining()?;
        ensure!(
            len <= remaining,
            "directory contains more than {} bytes of regular files; resource operation refused",
            self.byte_limit
        );
        Ok(remaining)
    }

    /// Accumulate the bytes actually read; metadata length is not trusted.
    fn record(&mut self, read: u64) {
        self.bytes += read;
    }
}

/// Shared walk for recursive snapshots: the caller has already opened the directory handle with NOFOLLOW.
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
                // Pre-check metadata before reading (reject over-budget files by the remaining
                // budget), then read with the bounded `take(remaining + 1)`.
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

/// Bounded regular-file read: reads at most `max_bytes + 1` bytes, so a file growing during
/// the read still cannot pass the limit; allocation follows the bytes actually read rather than
/// pre-allocating from oversized metadata.
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
