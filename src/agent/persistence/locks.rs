use super::validate_id;
use anyhow::{Context, Result, ensure};
use rustix::fs::{FlockOperation, OFlags, flock};
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::SystemTime,
};

/// 文件描述符随会话保留，关闭时由操作系统释放锁；不删除锁文件。
pub(super) struct SessionLock {
    _file: File,
}

pub(crate) fn directory() -> Result<PathBuf> {
    Ok(crate::config::global_dir()?
        .with_file_name("run")
        .join("sessions"))
}

impl SessionLock {
    pub(super) fn acquire(dir: &Path, id: &str) -> Result<Self> {
        Self::try_acquire(dir, id)?.with_context(|| format!("session is already in use: {id}"))
    }

    pub(super) fn try_acquire(dir: &Path, id: &str) -> Result<Option<Self>> {
        validate_id(id)?;
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .context("creating session lock directory")?;
        ensure!(
            fs::symlink_metadata(dir)?.is_dir(),
            "session lock directory must be a directory, not a symlink"
        );
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
            .open(dir.join(format!("{id}.lock")))
            .context("opening session lock file")?;
        ensure!(
            file.metadata()?.is_file(),
            "session lock is not a regular file"
        );
        if !try_lock(&file)? {
            return Ok(None);
        }
        file.set_modified(SystemTime::now())
            .context("refreshing session lock timestamp")?;
        Ok(Some(Self { _file: file }))
    }
}

fn try_lock(file: &File) -> Result<bool> {
    match flock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(error) if error == rustix::io::Errno::WOULDBLOCK => Ok(false),
        Err(error) => Err(error).context("locking session"),
    }
}

pub(crate) fn gc(cutoff: SystemTime) -> Result<(u64, u64)> {
    gc_in(&directory()?, cutoff)
}

/// 只统计合法 session ID 对应的普通锁文件，不创建缺失的存储。
fn gc_in(dir: &Path, cutoff: SystemTime) -> Result<(u64, u64)> {
    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(error) => return Err(error).context("reading session lock directory"),
    };
    ensure!(
        metadata.is_dir(),
        "session lock directory must be a directory, not a symlink"
    );
    let mut total = 0;
    let mut removed = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".lock")) else {
            continue;
        };
        if validate_id(id).is_err() {
            continue;
        }
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("reading session lock metadata"),
        };
        if !metadata.is_file() {
            continue;
        }
        total += 1;
        if metadata.modified()? >= cutoff {
            continue;
        }
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("opening expired session lock"),
        };
        if !try_lock(&file)? {
            continue;
        }
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.modified()? >= cutoff {
            continue;
        }
        let current = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("checking expired session lock"),
        };
        if current.is_file() && current.dev() == metadata.dev() && current.ino() == metadata.ino() {
            // 按约定不引入协调锁；持锁删除仍存在并发打开旧 inode 的极小窗口。
            match fs::remove_file(path) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("removing expired session lock"),
            }
        }
    }
    Ok((total, removed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        time::{Duration, UNIX_EPOCH},
    };

    fn id(number: u8) -> String {
        format!("2026-10-17T01-02-03-{number:032x}")
    }

    fn modified(path: &Path, time: SystemTime) {
        File::open(path).unwrap().set_modified(time).unwrap();
    }

    #[test]
    fn session_lock_is_exclusive_persistent_and_refreshes_only_after_acquisition() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("run/sessions");
        let id = id(0);
        let path = dir.join(format!("{id}.lock"));
        let guard = SessionLock::acquire(&dir, &id).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let old = UNIX_EPOCH + Duration::from_secs(1);
        modified(&path, old);
        assert!(SessionLock::acquire(&dir, &id).is_err());
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), old);
        let other = SessionLock::acquire(&dir, &self::id(1)).unwrap();
        drop(other);
        drop(guard);
        assert!(path.is_file());
        let before = SystemTime::now();
        let _guard = SessionLock::acquire(&dir, &id).unwrap();
        assert!(fs::metadata(&path).unwrap().modified().unwrap() >= before);
    }

    #[test]
    fn session_lock_rejects_symlinks_nonregular_files_and_invalid_ids() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("locks");
        fs::create_dir(&dir).unwrap();
        let sentinel = root.path().join("sentinel");
        fs::write(&sentinel, b"keep").unwrap();
        let path = dir.join(format!("{}.lock", id(0)));
        symlink(&sentinel, &path).unwrap();
        assert!(SessionLock::acquire(&dir, &id(0)).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"keep");
        fs::create_dir(dir.join(format!("{}.lock", id(1)))).unwrap();
        assert!(SessionLock::acquire(&dir, &id(1)).is_err());
        assert!(SessionLock::acquire(&dir, "../escape").is_err());
        let link = root.path().join("link");
        symlink(&dir, &link).unwrap();
        assert!(SessionLock::acquire(&link, &id(2)).is_err());
        assert!(!dir.join(format!("{}.lock", id(2))).exists());
    }

    #[test]
    fn session_lock_gc_keeps_boundary_recent_future_held_and_unrecognized_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("locks");
        let cutoff = UNIX_EPOCH + Duration::from_secs(1_000);
        let old = cutoff - Duration::from_secs(1);
        let mut paths = Vec::new();
        for (number, time) in [old, cutoff, cutoff + Duration::from_secs(1), old, old]
            .into_iter()
            .enumerate()
        {
            drop(SessionLock::acquire(&dir, &id(number as u8)).unwrap());
            let path = dir.join(format!("{}.lock", id(number as u8)));
            modified(&path, time);
            paths.push(path);
        }
        let held = SessionLock::acquire(&dir, &id(3)).unwrap();
        modified(&paths[3], old);
        // 最近重新使用的旧锁文件刷新时间，释放后也不应被本次 GC 删除。
        drop(SessionLock::acquire(&dir, &id(4)).unwrap());
        let foreign = dir.join("unrelated.lock");
        fs::write(&foreign, b"keep").unwrap();
        modified(&foreign, old);
        let journal = dir.join(format!("{}.jsonl", id(5)));
        fs::write(&journal, b"saved session").unwrap();
        modified(&journal, old);
        let link = dir.join(format!("{}.lock", id(6)));
        symlink(&foreign, &link).unwrap();
        let directory = dir.join(format!("{}.lock", id(7)));
        fs::create_dir(&directory).unwrap();
        assert_eq!(gc_in(&dir, cutoff).unwrap(), (5, 1));
        assert!(!paths[0].exists());
        assert!(paths[1..].iter().all(|path| path.is_file()));
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(directory.is_dir());
        assert_eq!(fs::read(&journal).unwrap(), b"saved session");
        assert_eq!(fs::read(&foreign).unwrap(), b"keep");
        drop(held);
        assert_eq!(gc_in(&dir, cutoff).unwrap(), (4, 1));
        assert!(!paths[3].exists());
        assert_eq!(gc_in(&dir, cutoff).unwrap(), (3, 0));
    }

    #[test]
    fn session_lock_gc_does_not_create_missing_storage_or_follow_directory_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("missing");
        assert_eq!(gc_in(&dir, SystemTime::now()).unwrap(), (0, 0));
        assert!(!dir.exists());
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        symlink(&target, &dir).unwrap();
        assert!(gc_in(&dir, SystemTime::now()).is_err());
    }
}
