use super::{
    CHANGED, DELETED_PLAN, NAME_EXISTS, NOT_FOUND, PP_DIR_NAME, PlanListing, PlanSelection,
    is_missing, read_plan_bytes,
};
use crate::agent::Agent;
use crate::tools::plan_catalog::{self, PlanCatalogLock, PlanFileIdentity, PlanLease};
use crate::tools::save::Workspace;
use anyhow::{Context, Result, anyhow, bail, ensure};
use pp_core::plan::PLAN_MAX_BYTES;
use pp_core::plan::PlanLocation;
use pp_core::plan::path_for;
use pp_core::plan::valid_id;
use pp_core::plan::validate_file_name;
use pp_core::plan::validate_stored_file_name;
use rustix::fs::Dir;
use std::ffi::OsString;
use std::path::Path;

impl Agent {
    /// 重命名：核验 selection 后检查新名（严格 kebab 规则、三个目录磁盘
    /// 重名与登记表活动重名），同名直接成功；否则 link→CAS 更新登记→同步
    /// 当前绑定→清理旧名链接。只改文件名，不改 H1/正文/checkbox/ID/正文
    /// revision；旧 path token 因路径变化自然失效。
    pub fn rename_listed_plan(
        &mut self,
        selection: PlanSelection,
        file_name: String,
    ) -> Result<PlanListing> {
        self.journal.check()?;
        let verified = self.verify_selection(&selection)?;
        if file_name == verified.file_name {
            return self.list_plans();
        }
        validate_file_name(&file_name)?;
        let workspace = self.tools.plan_workspace();
        let verified_identity = plan_catalog::file_identity(workspace, Path::new(&verified.path))?
            .context("Plan file vanished; refresh the list.")?;
        {
            let _lock = PlanCatalogLock::acquire(workspace)?;
            let snapshot = plan_catalog::load_catalog(workspace)?;
            ensure!(
                snapshot.catalog.active_by_file_name(&file_name).is_none(),
                "{NAME_EXISTS}"
            );
            // 三个生命周期目录的磁盘重名预检；缺失目录视为无重名，预检后
            // 的并发碰撞由 `link_plan` 的同 inode 确认兜底，绝不覆盖。
            for location in [
                PlanLocation::Drafts,
                PlanLocation::Plans,
                PlanLocation::Done,
            ] {
                let candidate = path_for(&file_name, location);
                match plan_catalog::file_identity(workspace, Path::new(&candidate)) {
                    Ok(Some(identity)) => {
                        ensure!(identity == verified_identity, "{NAME_EXISTS}");
                    }
                    Ok(None) => {}
                    Err(error) if is_missing(&error) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        // 发布新名链接并 CAS 更新登记；失败保留源名，返回真实错误。
        plan_catalog::publish_plan_move(
            workspace,
            &verified.id,
            (verified.file_name.as_str(), verified.location),
            (file_name.as_str(), verified.location),
            verified.content.as_bytes(),
        )?;
        // 同步当前绑定：仅文件名；登记已提交，journal 失败返回真实错误。
        // 先结束上面的 workspace 借用再改 workflow，避免借用冲突。
        if self
            .workflow
            .plan
            .as_ref()
            .is_some_and(|document| document.id == verified.id)
        {
            let previous = self.workflow.clone();
            let document = self.workflow.plan.as_mut().expect("bound plan");
            document.file_name = Some(file_name.clone());
            if let Err(error) = self.journal.write_workflow(&self.workflow) {
                self.workflow = previous;
                return Err(error);
            }
        }
        // 清理旧名链接；失败交给按 ID 协调的同 inode 残留清理。
        let workspace = self.tools.plan_workspace();
        if let Err(error) = plan_catalog::commit_plan_move(
            workspace,
            (verified.file_name.as_str(), verified.location),
            (file_name.as_str(), verified.location),
            verified.content.as_bytes(),
        ) {
            tracing::warn!(event = "plan_rename_cleanup_failed", error = %error);
        }
        self.list_plans()
    }

    /// 确认删除：核验 selection 后先以 CAS 提交记录删除身份的 tombstone，
    /// 再仅删除选中普通文件及同 ID 同 inode 的生命周期残留链接；登记失败
    /// 或正文不匹配不 unlink。删除当前绑定只解除绑定并保留模式；其他会话
    /// 随后按 ID 得到 tombstone 错误，不从快照补回。已提交 tombstone 后的
    /// unlink 失败报 `Delete incomplete`，可显式重试且残留不被纳管成新 ID。
    pub fn delete_listed_plan(&mut self, selection: PlanSelection) -> Result<PlanListing> {
        self.journal.check()?;
        ensure!(valid_id(&selection.id), "Invalid plan selection.");
        let workspace = self.tools.plan_workspace();
        let lease = PlanLease::acquire(workspace, &selection.id)?;
        let (identity, primary) = {
            let _lock = PlanCatalogLock::acquire(workspace)?;
            let snapshot = plan_catalog::load_catalog(workspace)?;
            if let Some(entry) = snapshot
                .catalog
                .entries
                .iter()
                .find(|entry| entry.id == selection.id && !entry.deleted)
            {
                let path = path_for(&entry.file_name, entry.location);
                ensure!(path == selection.path, "{CHANGED}");
                let target = workspace.target(Path::new(&path), false)?;
                let bytes = target.read_bounded(PLAN_MAX_BYTES)?;
                ensure!(bytes.len() <= PLAN_MAX_BYTES, "Plan file is too large.");
                let content = std::str::from_utf8(&bytes)
                    .map_err(|_| anyhow!("Plan file is not valid UTF-8."))?;
                ensure!(content == selection.content, "{CHANGED}");
                // CAS：先提交墓碑（记录删除身份），再 unlink；登记失败不删文件。
                let identity = target
                    .stat_optional()?
                    .context("Plan file vanished; refresh the list.")
                    .map(|(device, inode)| PlanFileIdentity { device, inode })?;
                let mut catalog = snapshot.catalog;
                let entry = catalog
                    .entries
                    .iter_mut()
                    .find(|entry| entry.id == selection.id && !entry.deleted)
                    .expect("active entry still present");
                entry.deleted = true;
                entry.deleted_file_identity = Some(identity);
                plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog)?;
                (identity, path)
            } else if let Some(entry) = snapshot
                .catalog
                .entries
                .iter()
                .find(|entry| entry.id == selection.id && entry.deleted)
            {
                // 删除残留重试：仅当路径上的文件身份与墓碑一致。
                let identity = entry
                    .deleted_file_identity
                    .ok_or_else(|| anyhow!("{DELETED_PLAN}"))?;
                let path = path_for(&entry.file_name, entry.location);
                ensure!(path == selection.path, "{CHANGED}");
                match workspace
                    .target(Path::new(&path), false)?
                    .stat_optional()?
                    .map(|(device, inode)| PlanFileIdentity { device, inode })
                {
                    Some(current) if current == identity => {
                        let content = read_plan_bytes(workspace, Path::new(&path))?;
                        ensure!(content == selection.content, "{CHANGED}");
                        (identity, path)
                    }
                    _ => bail!("{DELETED_PLAN}"),
                }
            } else {
                bail!("{NOT_FOUND}");
            }
        };
        let mut failures: Vec<String> = Vec::new();
        remove_matching(workspace, Path::new(&primary), identity, &mut failures);
        // 同 inode 的生命周期残留链接：只清理与被删文件确认同 inode 的名字。
        for location in [
            PlanLocation::Drafts,
            PlanLocation::Plans,
            PlanLocation::Done,
        ] {
            let names = stored_names(workspace, location).unwrap_or_default();
            for name in names {
                let path = path_for(&name, location);
                if path == primary {
                    continue;
                }
                if let Ok(Some(current)) = plan_catalog::file_identity(workspace, Path::new(&path))
                    && current == identity
                {
                    remove_matching(workspace, Path::new(&path), identity, &mut failures);
                }
            }
        }
        ensure!(
            failures.is_empty(),
            "Delete incomplete; retry deleting this plan from /plan-list."
        );
        // 删除当前绑定：只解除 plan/review 绑定并保留模式；旧会话按 ID
        // 报已删除，不从快照补回。
        if self
            .plan_lease
            .as_ref()
            .is_some_and(|lease| lease.id() == selection.id)
        {
            self.plan_lease = None;
        }
        if self
            .workflow
            .plan
            .as_ref()
            .is_some_and(|document| document.id == selection.id)
        {
            let previous = self.workflow.clone();
            self.workflow.plan = None;
            self.workflow.review = None;
            if self.workflow.todo_plan_id.as_deref() == Some(selection.id.as_str()) {
                self.workflow.todos.clear();
                self.workflow.todo_plan_id = None;
            }
            if let Err(error) = self.journal.write_workflow(&self.workflow) {
                self.workflow = previous;
                return Err(error);
            }
        }
        drop(lease);
        self.list_plans()
    }
}

/// 目录直接子项中通过 stored 命名规则的 `.md` 名称；供删除残留清扫使用，
/// 缺失目录为空。
fn stored_names(workspace: &Workspace, location: PlanLocation) -> Result<Vec<String>> {
    let dir = match workspace.directory(
        &[
            OsString::from(PP_DIR_NAME),
            OsString::from(location.dir_name()),
        ],
        false,
    ) {
        Ok(dir) => dir,
        Err(error) if is_missing(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut names = Vec::new();
    for entry in Dir::new(dir)
        .map_err(std::io::Error::from)
        .context("opening plan directory")?
    {
        let entry = entry
            .map_err(std::io::Error::from)
            .context("reading plan directory entries")?;
        let raw = entry.file_name();
        if raw.to_bytes() == b"." || raw.to_bytes() == b".." {
            continue;
        }
        let Ok(name) = raw.to_str() else {
            continue;
        };
        if validate_stored_file_name(name).is_ok() {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// 身份确认后移除单个文件；缺失视为已完成，不同 inode 绝不动，其他失败
/// 记入 failures 由调用方报 `Delete incomplete`。
fn remove_matching(
    workspace: &Workspace,
    path: &Path,
    identity: PlanFileIdentity,
    failures: &mut Vec<String>,
) {
    let outcome = (|| -> Result<()> {
        let target = workspace.target(path, false)?;
        match target
            .stat_optional()?
            .map(|(device, inode)| PlanFileIdentity { device, inode })
        {
            Some(current) if current == identity => target.unlink(),
            Some(_) => Ok(()),
            None => Ok(()),
        }
    })();
    if let Err(error) = outcome {
        failures.push(format!("{}: {error:#}", path.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{entry, fixture_agent, registered, write_plan};
    use super::super::plan_display_title;
    use super::*;
    use crate::agent::tests::{err_message, seed_session};
    use pp_core::plan::PlanDocument;
    use pp_core::plan::PlanStatus;
    use pp_core::plan::WorkMode;
    use pp_core::plan::WorkflowState;

    fn bound_workflow(id: &str, file_name: &str, content: &str) -> WorkflowState {
        let mut document = PlanDocument::new().unwrap();
        document.id = id.to_owned();
        document.revision = 1;
        document.title = plan_display_title(content, file_name);
        document.content = content.to_owned();
        document.status = PlanStatus::Draft;
        document.origin_prompt = "list fixture".to_owned();
        document.file_name = Some(file_name.to_owned());
        document.location = PlanLocation::Drafts;
        WorkflowState {
            mode: WorkMode::Plan,
            plan: Some(document),
            ..Default::default()
        }
    }

    #[test]
    fn rename_updates_catalog_binding_and_keeps_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        let content = "# Rename\n\n- [ ] task\n";
        write_plan(&root, PlanLocation::Drafts, "rename-me.md", content);
        let listing = agent.list_plans().unwrap();
        let old = entry(&listing, "rename-me.md").clone();
        agent.workflow = bound_workflow(&old.id, "rename-me.md", content);
        seed_session(&mut agent, "rename fixture");

        let selection = agent.read_listed_plan(&old.id).unwrap();

        // 非法名称被拒绝，原文件不变。
        let error =
            err_message(agent.rename_listed_plan(selection.clone(), "Bad Name.md".to_owned()));
        assert!(error.contains("kebab-case"));
        assert!(root.join(".pp/drafts/rename-me.md").exists());

        // 重名被拒绝。
        write_plan(&root, PlanLocation::Plans, "taken-name.md", "# taken\n");
        agent.list_plans().unwrap();
        let error =
            err_message(agent.rename_listed_plan(selection.clone(), "taken-name.md".to_owned()));
        assert!(error.contains("already exists"));
        assert!(root.join(".pp/drafts/rename-me.md").exists());
        std::fs::remove_file(root.join(".pp/plans/taken-name.md")).unwrap();

        // 同名：无操作成功。
        agent
            .rename_listed_plan(selection.clone(), "rename-me.md".to_owned())
            .unwrap();

        // 正常重命名：旧路径消失，字节与 ID 保持，登记与绑定同步。
        let listing = agent
            .rename_listed_plan(selection.clone(), "renamed-plan.md".to_owned())
            .unwrap();
        assert!(!root.join(".pp/drafts/rename-me.md").exists());
        let renamed = root.join(".pp/drafts/renamed-plan.md");
        assert_eq!(std::fs::read_to_string(&renamed).unwrap(), content);
        assert_eq!(entry(&listing, "renamed-plan.md").id, old.id);
        let registered = registered(&agent, "renamed-plan.md");
        assert_eq!(registered.id, old.id);
        assert_eq!(registered.location, PlanLocation::Drafts);
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().file_name.as_deref(),
            Some("renamed-plan.md")
        );
        assert_eq!(agent.workflow.plan.as_ref().unwrap().content, content);
        let session_id = agent.session_id().unwrap().to_owned();
        assert_eq!(
            crate::agent::tests::journal_workflow(&dir.path().join(".pp/agent"), &session_id)
                .plan
                .unwrap()
                .file_name
                .as_deref(),
            Some("renamed-plan.md")
        );

        // 旧 selection 的路径已失效，不能用相同正文授权新路径上的操作。
        assert!(err_message(agent.verify_selection(&selection)).contains("Plan changed"));
        assert!(err_message(agent.delete_listed_plan(selection.clone())).contains("Plan changed"));
        assert!(
            err_message(agent.rename_listed_plan(selection, "stale-rename.md".to_owned()))
                .contains("Plan changed")
        );
        let refreshed = agent.read_listed_plan(&old.id).unwrap();
        assert_eq!(refreshed.path, ".pp/drafts/renamed-plan.md");
        assert!(agent.verify_selection(&refreshed).is_ok());
    }

    #[test]
    fn delete_commits_tombstone_before_unlink_and_clears_binding() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        let content = "# Delete\n\n- [ ] task\n";
        write_plan(&root, PlanLocation::Drafts, "delete-me.md", content);
        write_plan(&root, PlanLocation::Plans, "leftover-name.md", content);
        let listing = agent.list_plans().unwrap();
        let target = entry(&listing, "delete-me.md").clone();
        agent.workflow = bound_workflow(&target.id, "delete-me.md", content);
        // 模拟旧版本载入详情后留下的计划预览缓存。
        agent.workflow.todos = crate::tools::todo::project_plan_todos(
            agent.workflow.plan.as_ref().unwrap(),
            &[],
            false,
        )
        .unwrap();
        agent.workflow.todo_plan_id = Some(target.id.clone());
        seed_session(&mut agent, "delete fixture");
        let selection = agent.read_listed_plan(&target.id).unwrap();

        // 精确字节不匹配：既不提交墓碑也不删除（registered() 仅匹配活动条目）。
        let mut stale = selection.clone();
        stale.content = "# Stale\n".to_owned();
        assert!(err_message(agent.delete_listed_plan(stale)).contains("Plan changed"));
        assert!(root.join(".pp/drafts/delete-me.md").exists());
        let _active = registered(&agent, "delete-me.md");

        // 删除当前绑定：文件与同 inode 残留链接消失，绑定解除且模式保留。
        let leftover_path = root.join(".pp/plans/leftover-name.md");
        std::fs::remove_file(&leftover_path).unwrap();
        std::fs::hard_link(root.join(".pp/drafts/delete-me.md"), &leftover_path).unwrap();
        let listing = agent.delete_listed_plan(selection.clone()).unwrap();
        assert!(listing.entries.iter().all(|entry| entry.id != target.id));
        assert!(!root.join(".pp/drafts/delete-me.md").exists());
        assert!(!leftover_path.exists());
        assert!(agent.workflow.plan.is_none());
        assert!(agent.workflow.todos.is_empty());
        assert!(agent.workflow.todo_plan_id.is_none());
        assert_eq!(agent.workflow.mode, WorkMode::Plan);
        let workspace = agent.tools.plan_workspace();
        {
            let _lock = PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(workspace).unwrap();
            let tombstone = snapshot
                .catalog
                .entries
                .iter()
                .find(|entry| entry.id == target.id)
                .unwrap();
            assert!(tombstone.deleted);
            assert!(tombstone.deleted_file_identity.is_some());
        }
        // 后续访问按 ID 报已删除，不从快照补回。
        assert!(err_message(agent.read_listed_plan(&target.id)).contains("Plan was deleted"));
        assert!(err_message(agent.verify_selection(&selection)).contains("Plan was deleted"));
    }

    #[test]
    fn delete_retries_confirmed_leftover_without_readopting() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        let content = "# Retry\n\n- [ ] task\n";
        write_plan(&root, PlanLocation::Drafts, "retry-me.md", content);
        let listing = agent.list_plans().unwrap();
        let id = entry(&listing, "retry-me.md").id.clone();
        let selection = agent.read_listed_plan(&id).unwrap();

        // 手工提交墓碑但不 unlink（模拟首次删除的登记成功/删除失败）。
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
        // 残留仍能重新读取以确认删除；同 inode 的外部正文修改使旧确认失效。
        assert_eq!(agent.read_listed_plan(&id).unwrap(), selection);
        std::fs::write(root.join(&selection.path), "# Updated retry\n").unwrap();
        assert!(err_message(agent.delete_listed_plan(selection.clone())).contains("Plan changed"));
        assert!(root.join(&selection.path).exists());
        let selection = agent.read_listed_plan(&id).unwrap();
        // 重试删除：同 inode 残留被清理，登记保持墓碑，不纳管成新 ID。
        agent.delete_listed_plan(selection).unwrap();
        assert!(!root.join(".pp/drafts/retry-me.md").exists());
        let workspace = agent.tools.plan_workspace();
        {
            let _lock = PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(workspace).unwrap();
            let tombstone = snapshot
                .catalog
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .unwrap();
            assert!(tombstone.deleted);
        }
    }

    #[test]
    fn management_respects_live_plan_lease() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        write_plan(&root, PlanLocation::Drafts, "busy-plan.md", "# Busy\n");
        let listing = agent.list_plans().unwrap();
        let id = entry(&listing, "busy-plan.md").id.clone();
        let selection = agent.read_listed_plan(&id).unwrap();

        // 另一个真实 PlanLease 持有者：重命名/删除非阻塞拒绝；列表与预览仍可读。
        let workspace = agent.tools.plan_workspace();
        let holder = PlanLease::acquire(workspace, &id).unwrap();
        // 同一 ID 的第二个 lease 立即冲突（非阻塞，不等待）。
        assert!(err_message(PlanLease::acquire(workspace, &id)).contains("Plan is in use"));
        let rename_error =
            err_message(agent.rename_listed_plan(selection.clone(), "renamed-plan.md".to_owned()));
        assert!(rename_error.contains("Plan is in use"));
        let delete_error = err_message(agent.delete_listed_plan(selection));
        assert!(delete_error.contains("Plan is in use"));
        assert!(root.join(".pp/drafts/busy-plan.md").exists());
        assert!(agent.list_plans().is_ok());
        assert!(agent.read_listed_plan(&id).is_ok());
        drop(holder);
        // 释放后可再管理。
        let selection = agent.read_listed_plan(&id).unwrap();
        agent
            .rename_listed_plan(selection, "renamed-plan.md".to_owned())
            .unwrap();
        assert!(root.join(".pp/drafts/renamed-plan.md").exists());
    }
}
