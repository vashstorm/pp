use super::{CHANGED, DELETED_PLAN, NOT_FOUND, PlanSelection, VerifiedSelection, read_plan_bytes};
use crate::agent::Agent;
use crate::tools::plan_catalog::{self, PlanCatalogLock, PlanLease};
use crate::tools::save::Workspace;
use anyhow::{Result, anyhow, bail, ensure};
use pp_core::plan::PlanLocation;
use pp_core::plan::path_for;
use pp_core::plan::valid_id;
use std::path::Path;

impl Agent {
    /// 安全读取列表选中的计划正文，作为后续操作的 selection 令牌；只读，
    /// 不发审批 token，不进入模型历史。
    pub fn read_listed_plan(&mut self, id: &str) -> Result<PlanSelection> {
        ensure!(valid_id(id), "Invalid plan id.");
        let workspace = self.tools.plan_workspace();
        let _lock = PlanCatalogLock::acquire(workspace)?;
        let snapshot = plan_catalog::load_catalog(workspace)?;
        let entry = snapshot
            .catalog
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| anyhow!("{NOT_FOUND}"))?;
        let path = path_for(&entry.file_name, entry.location);
        if entry.deleted {
            // 只允许读取同 inode 的删除残留，供再次确认删除；进入/重命名
            // 仍经 resolve_active 拒绝墓碑，不恢复旧身份或执行权限。
            let expected = entry
                .deleted_file_identity
                .ok_or_else(|| anyhow!("{DELETED_PLAN}"))?;
            ensure!(
                plan_catalog::file_identity(workspace, Path::new(&path))? == Some(expected),
                "{DELETED_PLAN}"
            );
        }
        let content = read_plan_bytes(workspace, Path::new(&path))?;
        Ok(PlanSelection {
            id: id.to_owned(),
            path,
            content,
        })
    }

    /// 操作令牌核验：先非阻塞取得计划 lease（占用即 busy），再在登记表锁内
    /// 按 ID 解析 canonical 名称/位置（墓碑报已删除），最后安全读取并核对
    /// 精确字节。锁顺序固定 lease → 登记表锁；`publish_plan_move` 等登记
    /// 事务由调用方持有的 lease 保证互斥。不触碰 workflow/journal。
    pub(crate) fn verify_selection(
        &mut self,
        selection: &PlanSelection,
    ) -> Result<VerifiedSelection> {
        ensure!(valid_id(&selection.id), "Invalid plan selection.");
        let workspace = self.tools.plan_workspace();
        let lease = PlanLease::acquire(workspace, &selection.id)?;
        let (file_name, location) = resolve_active(workspace, &selection.id)?;
        let path = path_for(&file_name, location);
        ensure!(path == selection.path, "{CHANGED}");
        let content = read_plan_bytes(workspace, Path::new(&path))?;
        ensure!(content == selection.content, "{CHANGED}");
        Ok(VerifiedSelection {
            id: selection.id.clone(),
            file_name,
            location,
            path,
            content,
            _lease: lease,
        })
    }
}

/// 按 ID 解析登记表中的活动条目；墓碑与缺失分别报错，绝不按猜测路径操作。
fn resolve_active(workspace: &Workspace, id: &str) -> Result<(String, PlanLocation)> {
    let _lock = PlanCatalogLock::acquire(workspace)?;
    let snapshot = plan_catalog::load_catalog(workspace)?;
    if snapshot
        .catalog
        .entries
        .iter()
        .any(|entry| entry.deleted && entry.id == id)
    {
        bail!("{DELETED_PLAN}");
    }
    let entry = snapshot
        .catalog
        .active_by_id(id)
        .ok_or_else(|| anyhow!("{NOT_FOUND}"))?;
    Ok((entry.file_name.clone(), entry.location))
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{entry, fixture_agent, write_plan};
    use super::*;
    use crate::agent::tests::err_message;

    #[test]
    fn read_and_verify_selection_reject_stale_bytes_and_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        write_plan(
            &root,
            PlanLocation::Drafts,
            "verify-plan.md",
            "# Verify\n\n- [ ] task\n",
        );
        let listing = agent.list_plans().unwrap();
        let id = entry(&listing, "verify-plan.md").id.clone();

        let selection = agent.read_listed_plan(&id).unwrap();
        assert_eq!(selection.path, ".pp/drafts/verify-plan.md");
        assert!(selection.content.contains("# Verify"));
        assert!(agent.verify_selection(&selection).is_ok());

        // 精确字节不匹配：拒绝操作，不沿用旧正文。
        let mut stale = selection.clone();
        stale.content = "# Stale\n".to_owned();
        assert!(err_message(agent.verify_selection(&stale)).contains("Plan changed"));

        // 墓碑：verify/read 都拒绝，绝不复活。
        let workspace = agent.tools.plan_workspace();
        {
            let _lock = PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(workspace).unwrap();
            let mut catalog = snapshot.catalog;
            let entry = catalog
                .entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .unwrap();
            entry.deleted = true;
            entry.deleted_file_identity = Some(
                plan_catalog::file_identity(workspace, Path::new(&selection.path))
                    .unwrap()
                    .unwrap(),
            );
            plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog).unwrap();
        }
        assert_eq!(agent.read_listed_plan(&id).unwrap(), selection);
        assert!(err_message(agent.verify_selection(&selection)).contains("Plan was deleted"));
        std::fs::remove_file(root.join(&selection.path)).unwrap();
        assert!(err_message(agent.read_listed_plan(&id)).contains("Plan was deleted"));
    }
}
