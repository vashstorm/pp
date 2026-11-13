//! 计划级辅助锁与登记表短事务锁。
use anyhow::{Context, Result, bail, ensure};
use rustix::{
    fs::{FlockOperation, Mode, OFlags, flock},
    io::Errno,
};
use std::{ffi::OsString, fs::File};

use crate::tools::save::Workspace;
use pp_core::plan::valid_id;

pub(super) const PP_DIR_NAME: &str = ".pp";
pub(super) const INDEX_LOCK_NAME: &str = "plan-index.lock";
pub(super) const LOCKS_DIR_NAME: &str = "plan-locks";

/// 非阻塞独占 `flock`；占用时报错不等待，`EINTR` 重试。
fn acquire_flock(file: &File, busy: &'static str) -> Result<()> {
    loop {
        match flock(file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(()),
            Err(Errno::WOULDBLOCK) => bail!("{busy}"),
            Err(Errno::INTR) => continue,
            Err(error) => return Err(error).context("acquiring plan lock"),
        }
    }
}

fn release_flock(file: &File) {
    let _ = flock(file, FlockOperation::Unlock);
}

/// 计划级辅助锁：同一计划 ID 的模型任务与管理动作互斥。fd-safe 创建
/// （NOFOLLOW、0600），`flock` 独占非阻塞；持有普通 `File`，Drop 即关闭
/// 解锁。锁文件常驻 `.pp/plan-locks/`，不随计划正文重命名或删除。占用时
/// 返回错误，不阻塞等待；注意同一进程内不同 fd 也会冲突，已持有 lease 的
/// 调用方应复用而不是重复 acquire。
#[derive(Debug)]
pub(crate) struct PlanLease {
    id: String,
    file: File,
}

impl PlanLease {
    pub(crate) fn acquire(workspace: &Workspace, id: &str) -> Result<Self> {
        ensure!(valid_id(id), "plan id must be 32 hexadecimal characters");
        let locks = workspace
            .directory(
                &[OsString::from(PP_DIR_NAME), OsString::from(LOCKS_DIR_NAME)],
                true,
            )
            .context("opening plan lock directory (symlink paths are forbidden)")?;
        let name = format!("{id}.lock");
        let file = File::from(
            rustix::fs::openat(
                &locks,
                name.as_str(),
                OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .context("opening plan lock file (symlinks are forbidden)")?,
        );
        ensure!(
            file.metadata()
                .context("checking plan lock file")?
                .is_file(),
            "plan lock path must be a regular file"
        );
        acquire_flock(
            &file,
            "Plan is in use; stop the active task before managing it.",
        )?;
        Ok(Self {
            id: id.to_owned(),
            file,
        })
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for PlanLease {
    fn drop(&mut self) {
        release_flock(&self.file);
    }
}

/// 登记表短事务锁 `.pp/plan-index.lock`：只保护登记表的读改写事务，
/// 列表纳管也只取此锁。锁顺序固定：先取得目标计划的 `PlanLease`，再取本锁。
#[derive(Debug)]
pub(crate) struct PlanCatalogLock(File);

impl PlanCatalogLock {
    pub(crate) fn acquire(workspace: &Workspace) -> Result<Self> {
        let pp = workspace
            .directory(&[OsString::from(PP_DIR_NAME)], true)
            .context("opening .pp directory (symlink paths are forbidden)")?;
        let file = File::from(
            rustix::fs::openat(
                &pp,
                INDEX_LOCK_NAME,
                OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .context("opening plan index lock file (symlinks are forbidden)")?,
        );
        ensure!(
            file.metadata()
                .context("checking plan index lock file")?
                .is_file(),
            "plan index lock path must be a regular file"
        );
        acquire_flock(
            &file,
            "Plan index is busy; retry the catalog update shortly.",
        )?;
        Ok(Self(file))
    }
}

impl Drop for PlanCatalogLock {
    fn drop(&mut self) {
        release_flock(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{sample_id, workspace};
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn plan_lease_is_exclusive_per_id_and_persists_after_release() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        let id = sample_id(1);

        let lease = PlanLease::acquire(&ws, &id).unwrap();
        assert_eq!(lease.id(), id);

        // 其他计划的 lease 不冲突。
        PlanLease::acquire(&ws, &sample_id(2)).unwrap();

        let error = PlanLease::acquire(&ws, &id).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan is in use; stop the active task before managing it."
        );

        drop(lease);
        // 锁文件常驻，不随正文删除；释放后可重新取得。
        assert!(
            dir.path()
                .join(".pp/plan-locks")
                .join(format!("{id}.lock"))
                .exists()
        );
        PlanLease::acquire(&ws, &id).unwrap();

        assert!(PlanLease::acquire(&ws, "not-hex").is_err());
    }

    #[test]
    fn plan_lease_rejects_symlinked_lock_directory_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        let outside = tempfile::tempdir().unwrap();
        let id = sample_id(1);

        fs::create_dir_all(dir.path().join(".pp")).unwrap();
        symlink(outside.path(), dir.path().join(".pp/plan-locks")).unwrap();
        let error = PlanLease::acquire(&ws, &id).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error}");

        fs::remove_file(dir.path().join(".pp/plan-locks")).unwrap();
        fs::create_dir_all(dir.path().join(".pp/plan-locks")).unwrap();
        symlink(
            outside.path().join("elsewhere.lock"),
            dir.path().join(".pp/plan-locks").join(format!("{id}.lock")),
        )
        .unwrap();
        let error = PlanLease::acquire(&ws, &id).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error}");
    }

    #[test]
    fn catalog_lock_is_exclusive_and_coexists_with_plan_lease() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        let lease = PlanLease::acquire(&ws, &sample_id(1)).unwrap();

        let lock = PlanCatalogLock::acquire(&ws).unwrap();
        let error = PlanCatalogLock::acquire(&ws).unwrap_err();
        assert!(error.to_string().contains("Plan index is busy"), "{error}");
        drop(lock);
        PlanCatalogLock::acquire(&ws).unwrap();
        drop(lease);
    }
}
