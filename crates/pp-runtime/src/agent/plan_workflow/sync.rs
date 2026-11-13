use super::super::Agent;
use super::super::plan_sync::sync_plan_todos;
use super::{NO_PLAN, TASKS_CHANGED};
use crate::tools::plan_catalog;
use anyhow::{Result, anyhow, ensure};
use pp_core::plan;
use pp_core::plan::PlanLocation;
use pp_core::plan::PlanStatus;
use pp_core::plan::WorkMode;
use pp_core::plan::WorkflowState;

impl Agent {
    /// Persist a workflow change; on failure restore the whole `previous`
    /// state so callers keep one transactional rollback idiom.
    pub(in crate::agent) fn commit_workflow(&mut self, previous: WorkflowState) -> Result<()> {
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            return Err(error);
        }
        Ok(())
    }

    pub(in crate::agent) fn refresh_plan_progress(
        &mut self,
        emit: &mut dyn FnMut(pp_core::events::AgentEvent),
    ) -> Result<()> {
        if self.workflow.mode != WorkMode::Execute
            || !self
                .workflow
                .plan
                .as_ref()
                .is_some_and(|plan| plan.status == PlanStatus::Executed)
        {
            return Ok(());
        }
        sync_plan_todos(
            &self.tools,
            &mut self.journal,
            &mut self.workflow,
            emit,
            false,
        )
    }

    /// 成功收尾的自动归档：仅 Execute + Executed + Plans 可进入。安全读取
    /// 当前磁盘文件并同步进度后，要求 total > 0 且 completed == total，
    /// 且当前任务文本向量精确等于批准时冻结的 approved_tasks；任务被
    /// 删除、替换、增项或重排都不能通过。未全部勾选或零任务静默跳过
    /// （返回 `Ok(false)`），保留 Executed/Plans；任务集合与批准集合不一致
    /// 返回 `Plan tasks changed; re-plan before completing.`，同样不搬迁。
    /// 搬迁复用第二阶段协议（锁顺序 PlanLease → 登记表
    /// 锁，任务 lease 由 finish_task 持有）：发布 Done 链接并 CAS 更新登记
    /// → 提交 workflow Completed（纯路径/状态变化，不递增正文 revision）
    /// → 移除 plans 源链接。登记已更新但 workflow 提交失败时保留源链接并
    /// 返回真实错误，下一次按 ID 协调到 Done/Completed；提交后的清理警告
    /// 不撤回状态。返回是否完成归档。
    pub(in crate::agent) fn archive_completed_plan(
        &mut self,
        emit: &mut dyn FnMut(pp_core::events::AgentEvent),
    ) -> Result<bool> {
        if self.workflow.mode != WorkMode::Execute {
            return Ok(false);
        }
        let Some(document) = self.workflow.plan.as_ref() else {
            return Ok(false);
        };
        if document.status != PlanStatus::Executed || document.location != PlanLocation::Plans {
            return Ok(false);
        }
        // 归档前安全读取当前磁盘文件并同步进度；失败保留 Executed/Plans。
        self.refresh_plan_progress(emit)?;
        let document = self.workflow.plan.as_ref().expect("executed plan bound");
        let (completed, total) = plan::task_progress(&document.content);
        if total == 0 || completed != total {
            return Ok(false);
        }
        let tasks: Vec<String> = plan::plan_tasks(&document.content)
            .into_iter()
            .map(|task| task.text)
            .collect();
        // 旧记录的迁移基线已由 refresh_plan_progress 建立；仍然缺失或与
        // 当前任务集合不一致是违反归档合同的错误，不猜测批准集合。
        ensure!(
            document.approved_tasks.as_deref() == Some(tasks.as_slice()),
            "{TASKS_CHANGED}"
        );
        let file_name = document
            .file_name
            .clone()
            .ok_or_else(|| anyhow!("{NO_PLAN}"))?;
        let id = document.id.clone();
        let expected = document.content.as_bytes().to_vec();
        // 发布 Done 链接并原子更新登记；失败保留 Executed/Plans 与源链接。
        let warning = plan_catalog::publish_plan_move(
            self.tools.plan_workspace(),
            &id,
            (file_name.as_str(), PlanLocation::Plans),
            (file_name.as_str(), PlanLocation::Done),
            &expected,
        )?;
        if let Some(warning) = warning {
            tracing::warn!(event = "plan_archive_warning", warning = %warning);
        }
        // 登记已指向 Done：提交 Completed 绑定；失败保留源链接并返回真实
        // 错误，下一次按 ID 协调完成收尾，不声称已回滚。
        let previous = self.workflow.clone();
        {
            let document = self.workflow.plan.as_mut().expect("executed plan bound");
            document.status = PlanStatus::Completed;
            document.location = PlanLocation::Done;
        }
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            return Err(error.context(
                "Plan archival committed to the registry but the session state was not updated; the source link remains.",
            ));
        }
        // 状态已提交：移除 plans 源链接；目录同步失败只作为警告，不撤回。
        if let Err(error) = plan_catalog::commit_plan_move(
            self.tools.plan_workspace(),
            (file_name.as_str(), PlanLocation::Plans),
            (file_name.as_str(), PlanLocation::Done),
            &expected,
        ) {
            tracing::warn!(event = "plan_move_cleanup_failed", error = %error);
        }
        let document = self.workflow.plan.as_ref().expect("completed plan bound");
        emit(pp_core::events::AgentEvent::PlanProgressUpdated {
            plan_id: document.id.clone(),
            revision: document.revision,
            content: document.content.clone(),
            file_name: document.file_name.clone(),
            location: Some(document.location),
            status: Some(document.status),
        });
        Ok(true)
    }
}
