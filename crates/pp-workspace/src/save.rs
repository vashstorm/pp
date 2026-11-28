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

/// Workspace handle for the save layer: the startup directory is the primary managed root and
/// the temporary directory (`/tmp` and its real path) is the only additional managed root. Both
/// accept writes through NOFOLLOW directory fds opened level by level; symlinks and escaping
/// paths are still rejected outright.
pub struct Workspace {
    root: File,
    pub path: PathBuf,
    temp: Option<TempRoot>,
}

/// Managed temporary root: the canonicalized real path plus a directory fd.
struct TempRoot {
    root: File,
    path: PathBuf,
}

/// Managed root: the startup directory itself, or the temporary directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Anchor {
    Workspace,
    Temp,
}

/// Result of one path resolution: which managed root the path belongs to, that root's
/// directory fd and real path, and the components relative to that root.
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

/// Shared plan message for `save_new` collisions; neither plan bodies nor the registry may
/// overwrite existing files, and a collision must not be disguised as any error other than a
/// save failure.
const PLAN_SAVE_NEW_CONFLICT: &str = "Plan file name already exists; choose another file_name.";

/// Conflict description for exclusive creation; plan saves and ordinary writes each supply
/// their own explicit reason.
#[derive(Clone, Copy)]
enum NewConflict {
    Message(&'static str),
}

/// Read-only resource handle: the parent directory is opened with NOFOLLOW and the final node
/// type is verified on read.
pub struct ResourceTarget {
    parent: File,
    name: OsString,
    mode: Option<Mode>,
}

/// Public spelling of the temporary directory: on macOS it is a symlink to `/private/tmp`,
/// on Linux it is usually the real directory; both spellings map to the same managed root.
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

    /// Fold `/tmp/...` onto the real prefix of the temporary root; other spellings are returned
    /// unchanged. Pure value transform with no filesystem access, so `/tmp/a` and `/private/tmp/a`
    /// yield the same path.
    fn fold_temp(&self, path: &Path) -> PathBuf {
        match (&self.temp, path.strip_prefix(TEMP_ALIAS)) {
            (Some(temp), Ok(relative)) => temp.path.join(relative),
            _ => path.to_path_buf(),
        }
    }

    /// Path relative to the workspace root; returns `None` when an absolute path falls outside
    /// the workspace.
    fn workspace_relative<'a>(&self, folded: &'a Path) -> Option<&'a Path> {
        if folded.is_absolute() {
            folded.strip_prefix(&self.path).ok()
        } else {
            Some(folded)
        }
    }

    /// Resolve a managed path: workspace root first, then the temporary root; anything else is
    /// rejected. Relative paths always stay in the workspace and do not change meaning because a
    /// temporary root exists.
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

    /// Accept only path components inside the workspace root. Plan and LSP structural checks use
    /// it so temporary-root paths are never mistaken for workspace-relative paths.
    pub fn components(&self, path: &Path) -> Result<Vec<OsString>> {
        let folded = self.fold_temp(path);
        let relative = self
            .workspace_relative(&folded)
            .context("path must stay inside the startup directory")?;
        split_parts(relative)
    }

    /// Canonical key used by changes, undo and hashline snapshots: relative inside the workspace
    /// root, real absolute inside the temporary root, so `/tmp/a` and `/private/tmp/a` land on the
    /// same key.
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

    /// First step of a plan lifecycle move: after verifying the source regular file and its exact
    /// bytes, publish a hard link to the same inode at the destination with `linkat`; never copy
    /// over. An existing destination is only re-admitted idempotently on the same device/inode
    /// (returning `None`); same-name different inode, symlinks, path escapes and cross-filesystem
    /// targets all fail while keeping the source file. The post-publish identity check routes Err,
    /// mismatch and disappearance alike into the cleanup branch: remove the just-published
    /// destination name first, then return the fixed error, leaving no link pointing outside on
    /// disk. A parent-directory sync failure after the commit is returned only as a warning and
    /// does not retract the published link.
    pub fn link_plan(&self, from: &Path, to: &Path, expected: &[u8]) -> Result<Option<String>> {
        self.link_plan_impl(from, to, expected, &SystemPlanLink)
    }

    /// Same move flow as [`Self::link_plan`], but the publish action is injectable: only tests use
    /// it to build the deterministic interleaving "the source name is replaced between check and
    /// publish"; it is not compiled into production builds.
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

    /// Move implementation: the publish action comes from a port; the production implementation
    /// is `linkat`.
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
                // Idempotent re-entry: only an existing destination on the same device/inode is
                // accepted; a different file with identical bytes is also a conflict and is never
                // overwritten.
                let current = destination.stat_optional()?;
                ensure!(
                    current == Some(identity),
                    "Plan target already exists with different content; the source file is kept."
                );
                return Ok(None);
            }
            Err(error) => return Err(error).context("publishing the plan link"),
        }
        // Verify the destination identity after commit: Err, identity mismatch and disappearance
        // all go to the cleanup branch, which removes the just-published destination name before
        // returning the fixed error and keeps the source file intact. Err includes the real IO
        // error (ELOOP) from the NOFOLLOW open when the source name was swapped for a symlink, and
        // the published destination must not be left behind on disk.
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

    /// Commit step of a plan lifecycle move: after confirming the destination and source are still
    /// the same inode and the bytes match exactly, remove the source name. A missing source with
    /// correct destination bytes counts as done (idempotent re-entry); symlinks, path escapes and
    /// byte/identity mismatches all fail and leave the current state. A parent-directory sync failure
    /// after the commit is returned only as a warning.
    pub fn finish_plan_move(
        &self,
        from: &Path,
        to: &Path,
        expected: &[u8],
    ) -> Result<Option<String>> {
        let destination = self.target(to, true)?;
        // `target` also returns Ok for a missing final file name, so use a NOFOLLOW stat here to
        // tell the source name disappearing (idempotent completion) from real errors such as a
        // missing directory or symlink.
        let source = match self.target(from, false) {
            Ok(source) => source.stat_optional()?.map(|_| source),
            Err(error) if is_missing(&error) => None,
            Err(error) => return Err(error),
        };
        let Some(source) = source else {
            // Source already gone: done only if the destination exists with exactly matching bytes.
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

    /// Save entry for absolute paths outside the workspace explicitly cleared by the permission
    /// gate: it keeps the same `O_NOFOLLOW` level-by-level directory opening, regular-file check and
    /// atomic replace as inside the workspace, folding prefixes only for macOS system-root aliases
    /// (`/tmp`, `/var`, `/etc`).
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

    /// Resource operation entry: resolve the parent directory of the final name. With
    /// `create=false` a missing parent means the target does not exist yet (`Ok(None)`), so preview
    /// creates no directories; every existing level is verified through a NOFOLLOW directory handle
    /// and a symlink ancestor is still an error.
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

    /// Same as [`Self::walk_directory`], but checks the cancellation signal before each node so a
    /// long directory walk can abort instead of reading the whole snapshot first.
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
    /// `canonicalize` resolves `/tmp` to the real directory (on macOS `/private/tmp`).
    /// When it is missing or not a directory no temporary root is offered, so writes to
    /// `/tmp` fall back to an out-of-bounds rejection.
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

/// Open a directory level by level with NOFOLLOW from the given managed root, creating each
/// missing level when `create` is set.
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

/// Split a relative path into plain components; `..` may only pop an existing component and never
/// climbs above the managed root.
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

    /// NOFOLLOW `lstat`: missing returns `Missing`; symlinks and special files are errors, so a name
    /// that merely looks like a file is never treated as a regular file.
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

    /// Permission bits of the current target (`st_mode & 0o7777`, NOFOLLOW); missing returns
    /// `None`, symlinks and special files are errors. Snapshots and rollback rebuilds both use it
    /// to preserve the executable bit.
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

    /// Open the directory itself with NOFOLLOW for recursive walking and removal.
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

    /// Open the regular file through the already-resolved parent directory handle, rejecting
    /// symlinks and special files.
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

    /// Shared exact comparison; callers supply the redacted error appropriate to the operation.
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

    /// Check the baseline captured at read time after the temporary file is synced, then commit atomically.
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

    /// Exactly the same exclusive creation as `save_new` (`linkat`, no overwrite), but the caller
    /// supplies the collision message; non-body saves such as the plan registry use it to avoid a
    /// misleading plan hint.
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

    /// Exactly the same baseline-compared save as `save_if_unchanged`, but the caller supplies the
    /// conflict message.
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
        // Non-exclusive commits keep the existing target permissions; exclusive creation leaves it
        // to the umask.
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
            // linkat has committed and the target name holds the same inode; remove the temporary
            // name, the target is unaffected.
            drop(cleanup);
        } else {
            // The rename has committed, so later errors can only surface as persistence warnings and
            // must not be disguised as an unmodified file.
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

    /// Read the target's device/inode through the NOFOLLOW handle; missing returns `None` and
    /// symlinks or non-regular files are errors. Used by plan identity tombstones and inode checks.
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

    /// Read the target identity and modification time through the NOFOLLOW handle; missing returns
    /// `None` and symlinks or non-regular files are errors. Used by the management page listing to
    /// display `modified`.
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

    /// Safely remove the target name through the already-resolved parent directory handle; symlinks
    /// are not followed and only regular files are deleted. The caller verifies content and identity
    /// first (exact bytes/same inode); a missing target is an error and the caller decides how to
    /// report it upward.
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

/// Permission bits of `st_mode` (the low 12 bits including setuid/setgid/sticky), used to restore
/// the executable bit on snapshot rebuild; file type bits are never mixed into permissions.
fn permission_bits(mode: RawMode) -> u32 {
    (mode as u32) & 0o7777
}

/// Tell whether the error chain is only "path does not exist"; a missing move source means
/// idempotent completion.
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

/// Injectable port for publishing plan links: the production implementation is `linkat`, while tests
/// use it to swap the source name for a symlink before publishing, building a deterministic
/// interleaving (category: external side effect that tests cannot construct).
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
