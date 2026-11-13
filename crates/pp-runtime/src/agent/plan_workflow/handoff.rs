use super::super::Agent;
use super::super::ExecutionPreparation;
use super::super::{context, turn};
use super::{NO_PLAN, NO_TASKS, NOT_CURRENT, NOT_PLAN};
use crate::agent::input::InputQueue;
use crate::tools::plan_catalog;
use anyhow::{Result, anyhow, ensure};
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::plan;
use pp_core::plan::ExecutionContext;
use pp_core::plan::PlanLocation;
use pp_core::plan::PlanStatus;
use pp_core::plan::ReviewToken;
use pp_core::plan::WorkMode;
use pp_core::plan::WorkflowState;

/// 生命周期搬迁的待清理端点：源/目标各自的 (文件名, 位置)。
pub(super) type PlanMoveEndpoints = ((String, PlanLocation), (String, PlanLocation));

impl Agent {
    /// 审批后的 Drafts→Plans 交接：发布目标链接 → CAS 更新登记 → 提交
    /// 绑定（纯路径变化，不递增 revision）→ 交接 journal 成功后由调用方
    /// 移除源链接。已在 Plans 的计划只核对/更新登记，不重复搬迁。返回
    /// 待清理的源/目标身份。
    fn promote_approved_plan(&mut self, token: &ReviewToken) -> Result<Option<PlanMoveEndpoints>> {
        let document = self.workflow.plan.as_ref().expect("ready plan bound");
        if document.location == PlanLocation::Plans {
            return Ok(None);
        }
        let file_name = document
            .file_name
            .clone()
            .ok_or_else(|| anyhow!("{NO_PLAN}"))?;
        let source = (file_name.clone(), document.location);
        let target = (file_name, PlanLocation::Plans);
        {
            let workspace = self.tools.plan_workspace();
            plan_catalog::publish_plan_move(
                workspace,
                &document.id,
                (source.0.as_str(), source.1),
                (target.0.as_str(), target.1),
                token.content.as_bytes(),
            )?;
        }
        let previous = self.workflow.clone();
        self.workflow
            .plan
            .as_mut()
            .expect("ready plan bound")
            .location = PlanLocation::Plans;
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            return Err(error);
        }
        Ok(Some((source, target)))
    }

    /// 交接 journal 提交成功后的源链接清理；失败不撤回已提交状态，残留
    /// 源链接交给按 ID 协调做同 inode 确认清理，不冒报回滚。
    fn finish_approved_move(&self, pending_move: Option<PlanMoveEndpoints>, token: &ReviewToken) {
        let Some((source, target)) = pending_move else {
            return;
        };
        let workspace = self.tools.plan_workspace();
        if let Err(error) = plan_catalog::commit_plan_move(
            workspace,
            (source.0.as_str(), source.1),
            (target.0.as_str(), target.1),
            token.content.as_bytes(),
        ) {
            tracing::warn!(event = "plan_move_cleanup_failed", error = %error);
        }
    }

    /// `/plan` toggle 的退出方向：回到 Execute 并解除当前计划/审阅绑定。
    /// 已保存的草稿文件和登记原样保留（不再标记 Abandoned），旧稿经
    /// /plan-list 继续。未首存草稿仅在用户确认放弃同一绑定后允许退出。
    pub fn leave_plan(
        &mut self,
        cancellation: &CancellationToken,
        discard_plan_id: Option<&str>,
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        ensure!(self.workflow.mode == WorkMode::Plan, "{NOT_PLAN}");
        if let Some(id) = discard_plan_id {
            ensure!(
                self.workflow
                    .plan
                    .as_ref()
                    .is_some_and(|plan| plan.id == id && plan.revision == 0),
                "Plan changed; confirm leaving again."
            );
        }
        if let Some(plan) = &self.workflow.plan
            && plan.revision == 0
            && !plan.origin_prompt.trim().is_empty()
        {
            ensure!(
                discard_plan_id == Some(plan.id.as_str()),
                "Confirm discarding the unsaved draft before leaving Plan mode."
            );
        }
        self.journal.check()?;
        let model = self.default_execution_model()?;
        self.ensure_plan_mcp_access(cancellation)?;
        self.ensure_execute_mcp_access(cancellation)?;
        // 解除绑定，保留磁盘上的计划和登记。
        let workflow = WorkflowState::default();
        let session = self.execution_session(&model, &workflow, true)?;
        let metadata = model.metadata(&self.tools, &self.base_system, self.prompt_source.as_ref());
        cancellation::check(cancellation)?;
        self.journal
            .commit_plan_exit(&metadata, (&session).into(), &workflow)?;
        self.session = session;
        self.workflow = workflow;
        self.install_execution_model(model);
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: None,
            todo_snapshot: None,
        })
    }

    /// 批准已审阅计划并准备执行交接；可先在 Plan 权限下压缩上下文。
    /// 提交前可取消地按执行权限补连 MCP，再校验 token；失败时保持
    /// Plan/Ready，禁止从未完成的交接发起实施请求。
    pub fn approve_plan(
        &mut self,
        token: ReviewToken,
        context: ExecutionContext,
        input: &InputQueue,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(pp_core::events::AgentEvent),
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        ensure!(self.workflow.mode == WorkMode::Plan, "{NOT_CURRENT}");
        // 审批访问前按 ID 协调实际路径；墓碑、搬迁与首次登记在此暴露。
        self.reconcile_plan_location()?;
        // 审批前先按 Plan 权限补连缺失服务，再触碰审批状态。
        self.ensure_plan_mcp_access(cancellation)?;
        {
            let document = self
                .workflow
                .plan
                .as_ref()
                .ok_or_else(|| anyhow!("{NOT_CURRENT}"))?;
            ensure!(token.content == document.content, "{NOT_CURRENT}");
            let (_, total) = plan::task_progress(&token.content);
            ensure!(total > 0, "{NO_TASKS}");
        }
        self.check_plan_token_for_approval(&token, input)?;
        if context == ExecutionContext::Compact {
            // 压缩保持 Plan 权限；失败或取消时不交接执行。后续再次核对
            // token 与队列，拒绝压缩期间的外部修订及补充输入。
            let mut attempted = false;
            let outcome = self.compact(cancellation, &mut |event| {
                attempted |= matches!(
                    &event,
                    pp_core::events::AgentEvent::CompactionStarted { .. }
                );
                emit(event);
            })?;
            ensure!(
                !matches!(outcome, context::CompactionOutcome::Cancelled),
                "Plan execution cancelled during context compaction."
            );
            ensure!(
                !attempted || !matches!(outcome, context::CompactionOutcome::Unchanged),
                "Context compaction did not reduce history; review the plan before executing."
            );
            cancellation::check(cancellation)?;
            self.check_plan_token_for_approval(&token, input)?;
        }
        let model = self.default_execution_model()?;
        self.ensure_execute_mcp_access(cancellation)?;
        let fresh = context == ExecutionContext::Fresh;
        let candidate = (|| -> Result<_> {
            let mut workflow = self.workflow.clone();
            workflow.mode = WorkMode::Execute;
            if fresh {
                workflow.review = None;
            }
            let document = workflow.plan.as_mut().expect("ready plan bound");
            document.status = PlanStatus::Executed;
            document.location = PlanLocation::Plans;
            // 从已审阅正文冻结任务文本及顺序，之后的进度导入保留该基线。
            document.approved_tasks = Some(
                plan::plan_tasks(&token.content)
                    .into_iter()
                    .map(|task| task.text)
                    .collect(),
            );
            let prompt_context = if fresh {
                ExecutionContext::Fresh
            } else {
                ExecutionContext::Keep
            };
            let prompt = plan::execution_prompt(document, prompt_context);
            let mut todos = crate::tools::todo::project_plan_todos(document, &[], false)?;
            crate::tools::todo::normalize_in_progress(&mut todos);
            workflow.todos = todos;
            workflow.todo_plan_id = Some(document.id.clone());
            let mut session = self.execution_session(&model, &workflow, !fresh)?;
            turn::append_user_to(&mut session, &prompt);
            // 准备原生请求后再核对输入队列与计划正文，之后才发布计划移动。
            cancellation::check(cancellation)?;
            self.check_plan_token_for_approval(&token, input)?;
            Ok((workflow, session, prompt))
        })();
        let (workflow, session, prompt) = candidate?;
        let committed = (|| -> Result<_> {
            // 按 PlanLease → 登记锁顺序持有到执行任务收尾。
            self.ensure_task_plan_lease()?;
            let pending_move = self.promote_approved_plan(&token)?;
            let metadata =
                model.metadata(&self.tools, &self.base_system, self.prompt_source.as_ref());
            let mut fork = fresh.then(|| self.journal.fork_empty());
            let target = fork.as_mut().unwrap_or(&mut self.journal);
            target.commit_plan_handoff(&metadata, (&session).into(), &workflow, &prompt)?;
            if fresh {
                // 源规划会话仍在 Plan，已批准的计划在源会话中只读。
                let mut source = self.workflow.clone();
                source.plan.clone_from(&workflow.plan);
                self.journal.write_workflow(&source)?;
            }
            Ok((pending_move, fork))
        })();
        let (pending_move, fork) = match committed {
            Ok(committed) => committed,
            Err(error) => {
                self.plan_lease = None;
                return Err(error);
            }
        };
        self.session = session;
        self.workflow = workflow;
        self.install_execution_model(model);
        if let Some(journal) = fork {
            self.reset_lsp_diagnostics();
            self.journal = journal;
            self.tools.artifacts = Default::default();
            self.tools.reset_pipe_store();
            if let Some(id) = self.journal.id() {
                let _ = self.tools.artifacts.bind(self.journal.directory(), id);
            }
            self.tools.clear_changes();
            self.clear_session_usage();
            self.clear_session_cost();
            self.pending_project_load.clear();
            self.reported_project_load.clear();
            self.tools.reset_project_instructions([]);
        }
        self.finish_approved_move(pending_move, &token);
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: fresh.then(Vec::new),
            todo_snapshot: turn::todo_snapshot(&self.workflow),
        })
    }
}
