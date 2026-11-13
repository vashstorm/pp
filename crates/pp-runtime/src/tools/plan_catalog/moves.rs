//! 生命周期搬迁：文件身份、发布／提交与已确认残留清理。
use anyhow::{Result, bail, ensure};
use std::path::Path;

use super::{PlanCatalogEntry, PlanCatalogLock, PlanFileIdentity, load_catalog, save_catalog};
use crate::tools::save::Workspace;
use pp_core::plan::PlanLocation;

/// 安全读取相对路径目标的文件身份；缺失返回 `None`，符号链接与非普通
/// 文件报错。供登记墓碑记录与同 inode 残留确认使用，不能替代操作前的
/// 精确字节读取。
pub(crate) fn file_identity(
    workspace: &Workspace,
    path: &Path,
) -> Result<Option<PlanFileIdentity>> {
    let target = workspace.target(path, false)?;
    Ok(target
        .stat_optional()?
        .map(|(device, inode)| PlanFileIdentity { device, inode }))
}

/// 生命周期搬迁的发布步骤：核对当前登记（墓碑、源身份、目标重名）后，
/// 在目标位置发布不覆盖的同 inode 链接，并以 CAS 原子更新登记表路径。
/// 失败保留源链接；同 inode 的既有目标允许幂等重入。锁顺序：调用方须
/// 先持有计划 `PlanLease`，再由本函数获取 `PlanCatalogLock`。发布成功
/// 后由调用方提交 workflow/handoff，再调用 `commit_plan_move` 移除源名。
pub(crate) fn publish_plan_move(
    workspace: &Workspace,
    id: &str,
    source: (&str, PlanLocation),
    target: (&str, PlanLocation),
    expected: &[u8],
) -> Result<Option<String>> {
    let _lock = PlanCatalogLock::acquire(workspace)?;
    let snapshot = load_catalog(workspace)?;
    if snapshot
        .catalog
        .entries
        .iter()
        .any(|entry| entry.deleted && entry.id == id)
    {
        bail!("Plan was deleted; choose another plan from /plan-list.");
    }
    let (source_name, source_location) = source;
    let (target_name, target_location) = target;
    match snapshot.catalog.active_by_id(id) {
        // 已在目标身份：幂等重入，只核对同 inode 链接，登记无需更新。
        Some(entry) if entry.file_name == target_name && entry.location == target_location => {
            return workspace.link_plan(
                Path::new(&path_for_name(source_name, source_location)),
                Path::new(&path_for_name(target_name, target_location)),
                expected,
            );
        }
        // 正常搬迁：登记条目必须仍指向源身份；否则登记表已被其他会话
        // 更改，fail-closed 等待按 ID 协调。
        Some(entry) => {
            ensure!(
                entry.file_name == source_name && entry.location == source_location,
                "Plan registration moved; reopen the plan from /plan-list."
            );
        }
        None => {}
    }
    ensure!(
        snapshot
            .catalog
            .entries
            .iter()
            .all(|entry| entry.deleted || entry.id == id || entry.file_name != target_name),
        "Plan file name is registered to another plan."
    );
    let warning = workspace.link_plan(
        Path::new(&path_for_name(source_name, source_location)),
        Path::new(&path_for_name(target_name, target_location)),
        expected,
    )?;
    let mut catalog = snapshot.catalog;
    match catalog
        .entries
        .iter_mut()
        .find(|entry| entry.id == id && !entry.deleted)
    {
        Some(entry) => {
            entry.file_name = target_name.to_owned();
            entry.location = target_location;
        }
        None => catalog.entries.push(PlanCatalogEntry {
            id: id.to_owned(),
            file_name: target_name.to_owned(),
            location: target_location,
            deleted: false,
            deleted_file_identity: None,
        }),
    }
    save_catalog(workspace, snapshot.raw.as_deref(), &catalog)
        .map(|registration_warning| warning.or(registration_warning))
}

/// 生命周期搬迁的提交步骤：调用方已提交 workflow/handoff 后移除源链接。
/// 失败保留源链接，交由按 ID 协调做同 inode 残留清理。
pub(crate) fn commit_plan_move(
    workspace: &Workspace,
    source: (&str, PlanLocation),
    target: (&str, PlanLocation),
    expected: &[u8],
) -> Result<Option<String>> {
    workspace.finish_plan_move(
        Path::new(&path_for_name(source.0, source.1)),
        Path::new(&path_for_name(target.0, target.1)),
        expected,
    )
}

/// 恢复协调的残留清理：仅当旧生命周期路径与新登记路径确认为同一 inode
/// 时移除旧名；缺失、不同 inode、符号链接或任何读取异常都保留现状，
/// 绝不越权删除，也不能让未确认的残留挡住计划访问。
pub(crate) fn cleanup_confirmed_leftover(
    workspace: &Workspace,
    current: &str,
    leftover: &str,
) -> Result<()> {
    if current == leftover {
        return Ok(());
    }
    let current_target = workspace.target(Path::new(current), false)?;
    let Some(current_identity) = current_target.stat_optional()? else {
        return Ok(());
    };
    let Ok(leftover_target) = workspace.target(Path::new(leftover), false) else {
        // 符号链接/特殊文件/缺失目录等异常残留不触碰，留给显式管理。
        return Ok(());
    };
    let Ok(Some(leftover_identity)) = leftover_target.stat_optional() else {
        return Ok(());
    };
    if leftover_identity != current_identity {
        return Ok(());
    }
    leftover_target.unlink()?;
    Ok(())
}

fn path_for_name(file_name: &str, location: PlanLocation) -> String {
    pp_core::plan::path_for(file_name, location)
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{sample_id, workspace};
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};

    #[test]
    fn file_identity_matches_metadata_and_refuses_missing_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        let path = dir.path().join(".pp/plans/a.md");
        fs::write(&path, b"# title").unwrap();

        let identity = file_identity(&ws, Path::new(".pp/plans/a.md"))
            .unwrap()
            .unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(identity.device, metadata.dev());
        assert_eq!(identity.inode, metadata.ino());

        assert!(
            file_identity(&ws, Path::new(".pp/plans/missing.md"))
                .unwrap()
                .is_none()
        );

        symlink("a.md", dir.path().join(".pp/plans/link.md")).unwrap();
        assert!(file_identity(&ws, Path::new(".pp/plans/link.md")).is_err());
    }

    #[test]
    fn publish_and_commit_move_update_registration_and_preserve_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        let id = sample_id(1);
        fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        fs::write(dir.path().join(".pp/drafts/a.md"), b"body").unwrap();

        // 未登记身份：发布 + 登记到目标位置；提交移除源。
        let warning = publish_plan_move(
            &ws,
            &id,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Plans),
            b"body",
        )
        .unwrap();
        assert_eq!(warning, None);
        let snapshot = load_catalog(&ws).unwrap();
        let entry = snapshot.catalog.active_by_id(&id).unwrap();
        assert_eq!(entry.file_name, "a.md");
        assert_eq!(entry.location, PlanLocation::Plans);
        assert_eq!(
            fs::read(dir.path().join(".pp/plans/a.md")).unwrap(),
            b"body"
        );

        commit_plan_move(
            &ws,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Plans),
            b"body",
        )
        .unwrap();
        assert!(!dir.path().join(".pp/drafts/a.md").exists());
        assert_eq!(
            fs::read(dir.path().join(".pp/plans/a.md")).unwrap(),
            b"body"
        );

        // 已在目标身份：幂等重入（含同 inode 残留源）。
        fs::hard_link(
            dir.path().join(".pp/plans/a.md"),
            dir.path().join(".pp/drafts/a.md"),
        )
        .unwrap();
        publish_plan_move(
            &ws,
            &id,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Plans),
            b"body",
        )
        .unwrap();
        commit_plan_move(
            &ws,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Plans),
            b"body",
        )
        .unwrap();
        assert!(!dir.path().join(".pp/drafts/a.md").exists());

        // 登记指向其他身份（既非源也非目标）时 fail-closed；墓碑报已删除。
        let error = publish_plan_move(
            &ws,
            &id,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Done),
            b"body",
        )
        .unwrap_err();
        assert!(error.to_string().contains("registration moved"), "{error}");

        let _lock = PlanCatalogLock::acquire(&ws).unwrap();
        let mut snapshot = load_catalog(&ws).unwrap();
        snapshot.catalog.entries[0].deleted = true;
        save_catalog(&ws, snapshot.raw.as_deref(), &snapshot.catalog).unwrap();
        drop(_lock);
        let error = publish_plan_move(
            &ws,
            &id,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Done),
            b"body",
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan was deleted; choose another plan from /plan-list."
        );
    }

    #[test]
    fn publish_move_refuses_other_registered_name_and_keeps_source() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        let id = sample_id(1);
        let other = sample_id(2);
        fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        fs::write(dir.path().join(".pp/drafts/mine.md"), b"body").unwrap();

        let _lock = PlanCatalogLock::acquire(&ws).unwrap();
        let snapshot = load_catalog(&ws).unwrap();
        let mut catalog = snapshot.catalog;
        catalog.entries.push(PlanCatalogEntry {
            id: other.clone(),
            file_name: "taken.md".into(),
            location: PlanLocation::Plans,
            deleted: false,
            deleted_file_identity: None,
        });
        save_catalog(&ws, snapshot.raw.as_deref(), &catalog).unwrap();
        drop(_lock);

        let error = publish_plan_move(
            &ws,
            &id,
            ("mine.md", PlanLocation::Drafts),
            ("taken.md", PlanLocation::Plans),
            b"body",
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("registered to another plan"),
            "{error}"
        );
        assert!(!dir.path().join(".pp/plans/taken-new.md").exists());
        assert_eq!(
            fs::read(dir.path().join(".pp/drafts/mine.md")).unwrap(),
            b"body"
        );
    }

    #[test]
    fn cleanup_leftover_only_removes_inode_confirmed_links() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        fs::write(dir.path().join(".pp/plans/a.md"), b"body").unwrap();

        // 同 inode 残留被移除；缺失残留静默完成。
        fs::hard_link(
            dir.path().join(".pp/plans/a.md"),
            dir.path().join(".pp/drafts/a.md"),
        )
        .unwrap();
        cleanup_confirmed_leftover(&ws, ".pp/plans/a.md", ".pp/drafts/a.md").unwrap();
        assert!(!dir.path().join(".pp/drafts/a.md").exists());
        cleanup_confirmed_leftover(&ws, ".pp/plans/a.md", ".pp/drafts/gone.md").unwrap();

        // 不同 inode、符号链接与特殊残留一律保留现状。
        fs::write(dir.path().join(".pp/drafts/other.md"), b"body").unwrap();
        symlink("a.md", dir.path().join(".pp/drafts/alias.md")).unwrap();
        cleanup_confirmed_leftover(&ws, ".pp/plans/a.md", ".pp/drafts/other.md").unwrap();
        cleanup_confirmed_leftover(&ws, ".pp/plans/a.md", ".pp/drafts/alias.md").unwrap();
        assert_eq!(
            fs::read(dir.path().join(".pp/drafts/other.md")).unwrap(),
            b"body"
        );
        assert!(dir.path().join(".pp/drafts/alias.md").is_symlink());

        // 相同路径与登记缺失均无动作。
        cleanup_confirmed_leftover(&ws, ".pp/plans/a.md", ".pp/plans/a.md").unwrap();
        cleanup_confirmed_leftover(&ws, ".pp/plans/missing.md", ".pp/drafts/other.md").unwrap();
        assert!(dir.path().join(".pp/plans/a.md").exists());
    }
}
