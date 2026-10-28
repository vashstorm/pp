//! Worker-side Plan mode lifecycle: mode entry/exit with MCP lifecycle
//! safety, safe external plan import, review-token validation, and the two
//! post-approval execution handoffs.
//!
//! Every permission change is persisted through the journal's transactional
//! workflow record before or as part of the transition, and a failed MCP
//! cleanup keeps the fail-closed `plan_cleanup_error` gate so no request can
//! start with half-removed capabilities.

use super::plan_sync::{
    import_plan_content, reconcile_plan_state, sync_plan_todos, sync_restored_todos,
};
use super::{
    Agent, ExecutionPreparation, Session,
    persistence::{Metadata, SessionJournal},
};
use crate::{
    agent::input::InputQueue,
    cancellation::{self, CancellationToken},
    mcp::Registry,
    plan::{
        self, ExecutionContext, PlanDocument, PlanLocation, PlanReview, PlanStatus, ReviewToken,
        WorkMode, WorkflowState,
    },
    project_instructions,
    tools::plan_catalog,
};
use anyhow::{Result, anyhow, bail, ensure};

const NOT_CURRENT: &str = "Plan review is no longer current.";
const CHANGED: &str = "Plan changed; review the new revision before executing.";
const NO_TASKS: &str = "Plan must contain at least one Markdown TODO item.";
const PENDING: &str = "Apply pending input before approving the plan.";
const NOT_PLAN: &str = "Plan mode is not active.";
const NO_PLAN: &str = "No saved plan.";
const NO_ORIGIN: &str = "Enter the task before reviewing this plan.";
const TASKS_CHANGED: &str = "Plan tasks changed; re-plan before completing.";

/// 生命周期搬迁的待清理端点：源/目标各自的 (文件名, 位置)。
type PlanMoveEndpoints = ((String, PlanLocation), (String, PlanLocation));

pub(super) enum SavedInstructions {
    Completion(async_openai::types::chat::ChatCompletionRequestSystemMessageContent),
    Response(Option<String>),
}

impl SavedInstructions {
    pub(super) fn take(session: &mut Session) -> Self {
        match session {
            Session::Completion(request) => {
                let Some(async_openai::types::chat::ChatCompletionRequestMessage::System(message)) =
                    request.messages.first_mut()
                else {
                    unreachable!("session has system instructions");
                };
                Self::Completion(std::mem::replace(&mut message.content, "".into()))
            }
            Session::Response(request) => Self::Response(request.instructions.take()),
        }
    }

    pub(super) fn restore(self, session: &mut Session) {
        match (self, session) {
            (Self::Completion(content), Session::Completion(request)) => {
                let Some(async_openai::types::chat::ChatCompletionRequestMessage::System(message)) =
                    request.messages.first_mut()
                else {
                    unreachable!("session has system instructions");
                };
                message.content = content;
            }
            (Self::Response(instructions), Session::Response(request)) => {
                request.instructions = instructions
            }
            _ => unreachable!("approval does not change protocol"),
        }
    }
}

impl Agent {
    /// 新来源按块渲染；旧会话继续使用不可拆分的基础指令。
    pub(super) fn compose_prompt(
        &self,
        instructions: &str,
        mode: &str,
        model: &str,
        tool_names: Vec<String>,
    ) -> Result<String> {
        self.compose_prompt_snapshot(instructions, mode, model, tool_names)
            .map(|(system, _, _)| system)
    }

    pub(super) fn compose_prompt_snapshot(
        &self,
        instructions: &str,
        mode: &str,
        model: &str,
        tool_names: Vec<String>,
    ) -> Result<(String, Vec<crate::skills::SkillHeader>, String)> {
        let project = project_instructions::load(self.tools.cwd())?;
        let skills = self
            .tools
            .skills
            .lock()
            .map_err(|_| anyhow!("skill registry unavailable"))?
            .headers();
        let summary = crate::skills::Skills::format_summary(&skills);
        let system = if let Some(source) = &self.prompt_source {
            let context = crate::system_prompt::RenderContext::new(
                self.tools.cwd(),
                model,
                self.tools.edit_mode(),
                mode,
                tool_names,
            );
            source.compose(
                &context,
                self.tools.limits,
                &project,
                &summary,
                instructions,
            )
        } else {
            let mut system = project_instructions::compose(
                &self.base_system,
                self.tools.cwd(),
                &project,
                &summary,
            );
            if !instructions.is_empty() {
                system.push_str("\n\n");
                system.push_str(instructions);
            }
            Ok(system)
        }?;
        Ok((system, skills, project))
    }

    /// 候选会话使用目标模式的工具清单，逐请求刷新使用原生请求里的实际清单。
    pub(super) fn compose_system(&self, workflow: &WorkflowState) -> Result<String> {
        self.compose_prompt(
            &workflow.instructions(),
            workflow.mode_name(),
            &self.current_model.model,
            self.tools
                .all_definitions(workflow.mode)
                .into_iter()
                .map(|tool| tool.name)
                .collect(),
        )
    }

    /// Persist a workflow change; on failure restore the whole `previous`
    /// state so callers keep one transactional rollback idiom.
    pub(super) fn commit_workflow(&mut self, previous: WorkflowState) -> Result<()> {
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            return Err(error);
        }
        Ok(())
    }

    /// 恢复协调入口：计划读写、审阅、审批、进度与恢复访问前按 ID 同步
    /// 实际路径。revision-0/未绑定直接通过；已绑定文档不 clone 整篇正文。
    pub(crate) fn reconcile_plan_location(&mut self) -> Result<()> {
        reconcile_plan_state(
            self.tools.plan_workspace(),
            &mut self.journal,
            &mut self.workflow,
        )
    }

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
}

/// 按 ID 协调结果：绑定需采纳登记、登记需首次建立，或已一致。

/// 按 ID 协调计划身份：登记表始终优先，把绑定文档同步到 canonical
/// 名称/位置；纯路径/状态变化不递增正文 revision，但使旧 path token
/// 失效。未登记的 revision>=1 旧绑定按迁移规则首次登记：Draft/Ready 的
/// 旧 Plans 绑定保留旧名迁回 Drafts（安全搬迁），Executed 仍在 Plans，
/// Abandoned 保留原位置只读，历史 Executed 登记到 Done 时只读同步
/// Completed，不重新授权。墓碑、路径/正文缺失一律报错，绝不从历史
/// 快照恢复，也不让历史 Draft/Ready 绑定把已提升的位置退回 drafts。

/// 绑定文档的身份三元组：登记协调是否改变了名称/位置/状态。

/// 只读预览投影：unchecked→pending、checked→completed，无 active/blocker，
/// 从不归一化激活，也从不继承运行态。Plan 模式下的来源会话（含 Fresh 批准
/// 后留在源会话的 Executed 绑定）一律走这里，不伪装成执行中。

/// Step 4 共享同步入口：把当前绑定计划的实际 Markdown 同步为会话 todo
/// 投影。适用范围固定为 Plan 模式有绑定、Execute+Executed，或仍绑定当前
/// 只读 Completed/Abandoned 计划；已用 init 脱离旧计划的独立普通清单
/// （来源为空且非空）绝不因此被覆盖。行为要点：
///
/// - 实际文件优先：先按 ID 协调登记路径，再安全读取；内容未变但投影或
///   来源缺失时仍初始化并持久化（旧会话/交接遗漏的自愈）。
/// - 旧 Executed 记录缺 approved_tasks：先以已登记正文冻结一次性迁移
///   基线并持久化，之后才读磁盘；不把迁移后的磁盘内容冒充批准集合。
/// - Execute+Executed 的磁盘任务文本/数量/顺序与批准向量不同（无论勾选
///   状态）时拒绝安装，保留最后可信的正文与运行态，阻止继续执行/提醒/
///   归档；说明与勾选变化正常同步。
/// - 执行投影只在 Execute+Executed 归一化唯一 active；预览与结束计划
///   始终无 active。revision-0 绑定从不读盘，预览为空。
/// - 无变化不写 journal、不发事件；正文/路径/状态变化发布
///   PlanProgressUpdated，投影/正文/来源的真实变化才按
///   `publish_snapshot` 发布 TodoSnapshot（先持久化 workflow，再
///   journal.event/check 后 emit）。

/// 恢复只携带一次同步新生成的快照；replay 已包含相同快照时不重复追加。

/// 启动恢复与交互式 resume 共用同一文件同步和快照去重边界。

/// Register on-disk plan bytes as a new revision against the bound workflow.
/// Returns `false` when the file matches the registered snapshot. Draft/Ready
/// documents become Ready only when the imported content has a genuine
/// Markdown task list; Executed/Abandoned documents remain read-only after
/// importing progress. The whole workflow is rolled back when the journal
/// rejects the write.

impl Agent {
    /// Stop every managed MCP process so Plan mode cannot reach them. On
    /// failure the registry, its slots and the UI's exit handle stay in place
    /// and `plan_cleanup_error` blocks run/continue until `/plan` retries.
    /// `pub(super)`：列表打开的 Plan 候选会话（plan_open）复用同一门禁。
    pub(super) fn ensure_plan_mcp_shutdown(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        cancellation::check(cancellation)?;
        if let Err(error) = self.tools.mcp.shutdown() {
            let message = format!(
                "MCP servers could not be stopped for Plan mode: {error:#}; run /plan again to retry."
            );
            self.plan_cleanup_error = Some(message.clone());
            bail!("{message}");
        }
        self.plan_cleanup_error = None;
        self.tools.mcp = Registry::default();
        self.session
            .set_tools(self.tools.all_definitions(WorkMode::Plan));
        Ok(())
    }

    fn abort_execution_preparation(
        &mut self,
        mut registry: Registry,
        error: anyhow::Error,
    ) -> anyhow::Error {
        match registry.shutdown() {
            Ok(()) => error,
            Err(cleanup) => {
                self.plan_cleanup_error = Some(format!(
                    "Prepared MCP services could not be stopped: {cleanup:#}; run /plan again to retry."
                ));
                self.tools.mcp = registry;
                cleanup.context(format!("Execution preparation also failed: {error:#}"))
            }
        }
    }

    /// `/plan` toggle 的进入方向：切到 Plan 待命（Plan+None），不绑定计划
    /// 文档；工作流权威先持久化，之后才关闭 MCP 能力。已在 Plan 时只重试
    /// 一次失败的关闭（`/plan <prompt>` 继续当前草稿也复用该入口）。
    pub(crate) fn enter_plan(&mut self, cancellation: &CancellationToken) -> Result<WorkflowState> {
        cancellation::check(cancellation)?;
        if self.workflow.mode == WorkMode::Plan {
            self.ensure_plan_mcp_shutdown(cancellation)?;
            return Ok(self.workflow.clone());
        }
        self.journal.check()?;
        // 旧 Executed/Completed 绑定一并解除；文件与登记保留，由 /plan-list 管理。
        let previous = std::mem::replace(
            &mut self.workflow,
            WorkflowState {
                mode: WorkMode::Plan,
                plan: None,
                ..Default::default()
            },
        );
        self.commit_workflow(previous)?;
        self.ensure_plan_mcp_shutdown(cancellation)?;
        Ok(self.workflow.clone())
    }

    /// Register on-disk plan bytes as a new revision. Returns `false` when the
    /// file matches the registered snapshot. Draft/Ready documents become
    /// Ready only when the imported content has a genuine Markdown task list;
    /// Executed/Abandoned documents remain read-only after importing progress.
    fn import_plan_revision(&mut self, content: String, new_title: &str) -> Result<bool> {
        import_plan_content(&mut self.journal, &mut self.workflow, content, new_title)
    }

    /// Safe plan review snapshot: reads the current file through the NOFOLLOW
    /// workspace handle, imports external edits as a new revision, and issues
    /// a review token bound to the current session, plan and exact bytes. A
    /// missing or unreadable file is an error; the logged snapshot is never a
    /// fallback. Executed/Abandoned plans safely refresh their displayed
    /// snapshot without regaining approval eligibility. The shared todo sync
    /// reconciles the registered path, imports the revision and rebuilds the
    /// read-only projection; a detached standalone list is never replaced.
    pub(crate) fn read_plan(&mut self) -> Result<PlanReview> {
        self.journal.check()?;
        let document = self
            .workflow
            .plan
            .clone()
            .ok_or_else(|| anyhow!("{NO_PLAN}"))?;
        let report = self.workflow.review.clone();
        if document.revision == 0 {
            // revision-0 绑定尚未首存：没有可解析的文件路径，从不从
            // 历史快照或猜测路径导入。
            bail!("{NO_PLAN}");
        }
        // 按 ID 协调登记路径、导入外部修订并刷新只读投影；墓碑、搬迁与
        // 首次登记在此暴露。
        sync_plan_todos(
            &self.tools,
            &mut self.journal,
            &mut self.workflow,
            &mut |_| {},
            true,
        )?;
        let document = self.workflow.plan.as_ref().expect("plan bound").clone();
        let session_id = self
            .journal
            .id()
            .ok_or_else(|| anyhow!("{NO_ORIGIN}"))?
            .to_owned();
        Ok(PlanReview {
            token: ReviewToken {
                session_id,
                path: document.path().ok_or_else(|| anyhow!("{NO_PLAN}"))?,
                plan_id: document.id.clone(),
                revision: document.revision,
                content: document.content.clone(),
            },
            document,
            report,
        })
    }

    /// `/plan` toggle 的退出方向：回到 Execute 并解除当前计划/审阅绑定。
    /// 已保存的草稿文件和登记原样保留（不再标记 Abandoned），旧稿经
    /// /plan-list 继续。未首存草稿仅在用户确认放弃同一绑定后允许退出。
    pub(crate) fn leave_plan(
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
        self.ensure_plan_mcp_shutdown(cancellation)?;
        let config = self
            .catalog
            .resolve(&self.current_model.reference)
            .map_err(|_| anyhow!("current model is unavailable"))?;
        let registry =
            Registry::connect(&config.settings.mcp_servers, self.tools.cwd(), cancellation)?;
        let cleanup = match registry.cleanup() {
            Ok(cleanup) => cleanup,
            Err(error) => return Err(self.abort_execution_preparation(registry, error)),
        };
        let previous = self.workflow.clone();
        self.workflow.mode = WorkMode::Execute;
        // 解除绑定但不删除任何文件；空待命与已保存草稿都直接清除。
        // 计划 todo 来源与投影一并解除，不恢复任何隐藏的旧普通清单。
        self.workflow.plan = None;
        self.workflow.review = None;
        self.workflow.todos = Vec::new();
        self.workflow.todo_plan_id = None;
        let mut definitions = self.tools.all_definitions(WorkMode::Execute);
        definitions.extend(registry.definitions());
        self.session.set_tools(definitions);
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            self.session
                .set_tools(self.tools.all_definitions(WorkMode::Plan));
            return Err(self.abort_execution_preparation(registry, error));
        }
        self.tools.mcp = registry;
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: None,
            cleanup,
            todo_snapshot: None,
        })
    }

    pub(super) fn refresh_plan_progress(
        &mut self,
        emit: &mut dyn FnMut(crate::events::AgentEvent),
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
    pub(super) fn archive_completed_plan(
        &mut self,
        emit: &mut dyn FnMut(crate::events::AgentEvent),
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
        emit(crate::events::AgentEvent::PlanProgressUpdated {
            plan_id: document.id.clone(),
            revision: document.revision,
            content: document.content.clone(),
            file_name: document.file_name.clone(),
            location: Some(document.location),
            status: Some(document.status),
        });
        Ok(true)
    }

    /// Verify a review token against the current session, plan revision,
    /// queue and exact on-disk bytes. Returns the on-disk content when it
    /// differs from the token; the caller decides whether that is an error
    /// (`validate_plan_token`) or an import before the same error
    /// (`check_plan_token_for_approval`) — no model request ever runs on a
    /// mismatch.
    fn plan_token_status(&self, token: &ReviewToken, input: &InputQueue) -> Result<Option<String>> {
        if let Some(error) = &self.plan_cleanup_error {
            bail!("{error}");
        }
        ensure!(self.workflow.mode == WorkMode::Plan, "{NOT_CURRENT}");
        let document = self
            .workflow
            .plan
            .as_ref()
            .ok_or_else(|| anyhow!("{NOT_CURRENT}"))?;
        ensure!(
            document.status == PlanStatus::Ready
                && token.plan_id == document.id
                && token.revision == document.revision
                && token.content == document.content,
            "{NOT_CURRENT}"
        );
        // 重命名/移位/删除后旧路径 token 不能继续授权；与登记表解析出的
        // 实际路径（由按 ID 协调同步）精确比较。
        ensure!(
            document.path().is_some_and(|path| path == token.path),
            "{NOT_CURRENT}"
        );
        self.journal.check()?;
        ensure!(
            self.journal.id() == Some(token.session_id.as_str()),
            "{NOT_CURRENT}"
        );
        ensure!(input.is_empty()?, "{PENDING}");
        let content = self.tools.read_plan(document)?;
        Ok((content != token.content).then_some(content))
    }

    /// Shared by the isolated reviewer: a changed file is reported with the
    /// exact changed message so the caller refreshes via `read_plan`.
    pub(crate) fn validate_plan_token(
        &mut self,
        token: &ReviewToken,
        input: &InputQueue,
    ) -> Result<()> {
        // 校验前按 ID 协调：登记重命名/搬迁使旧 path token 失效。
        self.reconcile_plan_location()?;
        ensure!(self.plan_token_status(token, input)?.is_none(), "{CHANGED}");
        Ok(())
    }

    /// Approval variant of the token check: a changed file is imported as the
    /// next revision before the exact changed message is returned, so the
    /// worker has regenerated the current token state and the UI only needs
    /// to read the refreshed snapshot.
    fn check_plan_token_for_approval(
        &mut self,
        token: &ReviewToken,
        input: &InputQueue,
    ) -> Result<()> {
        self.reconcile_plan_location()?;
        if let Some(content) = self.plan_token_status(token, input)? {
            let title = self
                .workflow
                .plan
                .as_ref()
                .expect("plan bound")
                .title
                .clone();
            self.import_plan_revision(content, &title)?;
            bail!("{CHANGED}");
        }
        Ok(())
    }

    /// 安全核对已保存正文后保留草稿，再退出 Plan；不发起模型请求。
    pub(crate) fn save_plan_and_leave(
        &mut self,
        token: ReviewToken,
        input: &InputQueue,
        cancellation: &CancellationToken,
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        ensure!(self.workflow.mode == WorkMode::Plan, "{NOT_CURRENT}");
        ensure!(input.is_empty()?, "{PENDING}");
        let review = self.read_plan()?;
        ensure!(review.token == token, "{CHANGED}");
        ensure!(
            matches!(
                review.document.status,
                PlanStatus::Draft | PlanStatus::Ready
            ),
            "{NOT_CURRENT}"
        );
        let previous = self.workflow.clone();
        self.workflow
            .plan
            .as_mut()
            .expect("saved plan bound")
            .status = PlanStatus::Draft;
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            return Err(error);
        }
        self.leave_plan(cancellation, None)
    }

    /// 批准已审阅计划并准备执行交接；可先在 Plan 权限下压缩上下文。
    /// 提交前可取消地连接候选 MCP，再校验 token；失败时停止新 registry，
    /// 保持 Plan/Ready，禁止从未完成的交接发起实施请求。
    pub(crate) fn approve_plan(
        &mut self,
        token: ReviewToken,
        context: ExecutionContext,
        input: &InputQueue,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(crate::events::AgentEvent),
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        ensure!(self.workflow.mode == WorkMode::Plan, "{NOT_CURRENT}");
        // 审批访问前按 ID 协调实际路径；墓碑、搬迁与首次登记在此暴露。
        self.reconcile_plan_location()?;
        // Finish any pending Plan cleanup before touching approval state.
        self.ensure_plan_mcp_shutdown(cancellation)?;
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
                attempted |= matches!(&event, crate::events::AgentEvent::CompactionStarted { .. });
                emit(event);
            })?;
            ensure!(
                !matches!(outcome, super::context::CompactionOutcome::Cancelled),
                "Plan execution cancelled during context compaction."
            );
            ensure!(
                !attempted || !matches!(outcome, super::context::CompactionOutcome::Unchanged),
                "Context compaction did not reduce history; review the plan before executing."
            );
            cancellation::check(cancellation)?;
            self.check_plan_token_for_approval(&token, input)?;
        }
        let config = self
            .catalog
            .resolve(&self.current_model.reference)
            .map_err(|_| anyhow!("current model is unavailable"))?;
        let registry =
            Registry::connect(&config.settings.mcp_servers, self.tools.cwd(), cancellation)?;
        let cleanup = match registry.cleanup() {
            Ok(cleanup) => cleanup,
            Err(error) => return Err(self.abort_execution_preparation(registry, error)),
        };
        // Re-check queue and file after preparation; the approved token bytes
        // remain the execution snapshot.
        if let Err(error) = self.check_plan_token_for_approval(&token, input) {
            return Err(self.abort_execution_preparation(registry, error));
        }
        // 批准必须持有计划 lease（锁顺序 lease → 登记表锁），成功后转交
        // 执行任务持有到 finish_task；失败出口立即释放。
        if let Err(error) = self.ensure_task_plan_lease() {
            self.plan_lease = None;
            return Err(self.abort_execution_preparation(registry, error));
        }
        // 两种批准都以经过 ReviewToken 验证的当前完整正文冻结有序任务
        // 集合（重复文本按序保留）；执行进度导入不得覆盖该字段。列表
        // 重新执行在打开时清空，因此这里冻结的是本次审阅集合。
        {
            let document = self.workflow.plan.as_mut().expect("ready plan bound");
            document.approved_tasks = Some(
                plan::plan_tasks(&token.content)
                    .into_iter()
                    .map(|task| task.text)
                    .collect(),
            );
        }
        // 第二次校验通过后发布 Drafts→Plans；失败保留源链接，返回真实错误。
        let pending_move = match self.promote_approved_plan(&token) {
            Ok(pending) => pending,
            Err(error) => {
                self.plan_lease = None;
                return Err(self.abort_execution_preparation(registry, error));
            }
        };
        match context {
            ExecutionContext::Keep | ExecutionContext::Compact => {
                self.prepare_keep_execution(&token, pending_move, registry, cleanup)
            }
            ExecutionContext::Fresh => {
                self.prepare_fresh_execution(&token, pending_move, registry, cleanup)
            }
        }
    }

    /// Keep-context handoff: append exactly one execution user message to the
    /// current session and commit it together with the Execute workflow as
    /// one atomic journal group. Failure restores the Plan/Ready state and
    /// the native history; the plan file is never rolled back.
    fn prepare_keep_execution(
        &mut self,
        token: &ReviewToken,
        pending_move: Option<PlanMoveEndpoints>,
        registry: Registry,
        cleanup: crate::mcp::Cleanup,
    ) -> Result<ExecutionPreparation> {
        let document = self
            .workflow
            .plan
            .as_ref()
            .expect("ready plan bound")
            .clone();
        debug_assert_eq!(document.content, token.content);
        let prompt = crate::plan::execution_prompt(&document, ExecutionContext::Keep);
        // 三种批准交接一致：从批准正文重建执行投影并激活首个未勾选项；
        // 投影与 Execute workflow 一起进入同一个 handoff 事务。
        let mut todos = crate::tools::todo::project_plan_todos(
            &PlanDocument {
                status: PlanStatus::Executed,
                ..document.clone()
            },
            &[],
            false,
        )?;
        crate::tools::todo::normalize_in_progress(&mut todos);
        let todo_plan_id = Some(document.id.clone());
        let previous_workflow = self.workflow.clone();
        let previous_system = SavedInstructions::take(&mut self.session);
        let previous_pending = self.pending_instructions.take();
        self.append_user(&prompt);
        self.workflow.mode = WorkMode::Execute;
        if let Some(plan) = &mut self.workflow.plan {
            plan.status = PlanStatus::Executed;
        }
        self.workflow.todos = todos;
        self.workflow.todo_plan_id = todo_plan_id;
        let mut definitions = self.tools.all_definitions(WorkMode::Execute);
        definitions.extend(registry.definitions());
        self.session.set_tools(definitions);
        let prepared = (|| -> Result<()> {
            // Switch the system prompt to the Execute progress rules before
            // the synced commit so the stored state matches the handoff.
            self.refresh_instructions()?;
            let metadata = Metadata {
                cwd: self.tools.cwd(),
                model_reference: &self.current_model.reference,
                think_level: self.think_level,
                edit_mode: self.tools.edit_mode(),
                tool_limits: self.tools.limits,
                base_system: &self.base_system,
                prompt_source: self.prompt_source.as_ref(),
            };
            self.journal.commit_plan_handoff(
                &metadata,
                (&self.session).into(),
                &self.workflow,
                &prompt,
            )
        })();
        if let Err(error) = prepared {
            self.workflow = previous_workflow;
            self.remove_last_user();
            previous_system.restore(&mut self.session);
            self.pending_instructions = previous_pending;
            self.session
                .set_tools(self.tools.all_definitions(WorkMode::Plan));
            // 交接失败：已发布的目标链接与登记保留，源链接尚未移除；
            // 旧 path token 失效，重试从 Plans 幂等重入。
            return Err(self.abort_execution_preparation(registry, error));
        }
        // 批准初始化快照先记入目标 journal；再随交接发布给 UI。
        let todo_snapshot = super::turn::todo_snapshot(&self.workflow);
        if let Some(event) = &todo_snapshot {
            self.journal.event(event);
            self.journal.check()?;
        }
        self.finish_approved_move(pending_move, token);
        self.tools.mcp = registry;
        self.pending_instructions = None;
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: None,
            cleanup,
            todo_snapshot,
        })
    }

    /// Fresh-context handoff: prepare an empty current-model session, a blank
    /// journal fork and an Execute workflow carrying only the approved plan,
    /// then commit the target checkpoint and the source handoff record. Only
    /// when both commits succeed is the working session replaced; the
    /// planning session stays in Plan mode with an Executed (read-only) plan.
    fn prepare_fresh_execution(
        &mut self,
        token: &ReviewToken,
        pending_move: Option<PlanMoveEndpoints>,
        registry: Registry,
        cleanup: crate::mcp::Cleanup,
    ) -> Result<ExecutionPreparation> {
        let document = self
            .workflow
            .plan
            .as_ref()
            .expect("ready plan bound")
            .clone();
        debug_assert_eq!(document.content, token.content);
        // 候选执行 workflow 从批准正文重建执行投影并激活首个未勾选项；
        // 与目标 checkpoint 一起进入同一个 handoff 事务。
        let candidate_plan = PlanDocument {
            status: PlanStatus::Executed,
            ..document.clone()
        };
        let todos = crate::tools::todo::project_plan_todos(&candidate_plan, &[], false)?;
        let candidate_workflow = WorkflowState {
            mode: WorkMode::Execute,
            plan: Some(candidate_plan),
            todos,
            todo_plan_id: Some(document.id.clone()),
            ..Default::default()
        };
        let prompt = crate::plan::execution_prompt(&document, ExecutionContext::Fresh);
        let previous = self.workflow.clone();
        // Reload current project and skill rules; the candidate session reuses
        // the current model, thinking level and edit mode, never the planning
        // native history.
        let prepared = (|| -> Result<(Session, SessionJournal)> {
            let mut definitions = self.tools.all_definitions(WorkMode::Execute);
            definitions.extend(registry.definitions());
            let system = self.compose_prompt(
                &candidate_workflow.instructions(),
                "execute",
                &self.current_model.model,
                definitions.iter().map(|tool| tool.name.clone()).collect(),
            )?;
            let mut session = Session::new(
                self.current_model.model.clone(),
                self.current_model.protocol,
                system,
                self.tools.edit_mode(),
            );
            session.set_think_level(self.think_level);
            session.set_tools(definitions);
            super::turn::append_user_to(&mut session, &prompt);
            let metadata = Metadata {
                cwd: self.tools.cwd(),
                model_reference: &self.current_model.reference,
                think_level: self.think_level,
                edit_mode: self.tools.edit_mode(),
                tool_limits: self.tools.limits,
                base_system: &self.base_system,
                prompt_source: self.prompt_source.as_ref(),
            };
            let mut journal = self.journal.fork_empty();
            // 目标执行会话先提交，随后才是源规划会话的 handoff 记录。
            journal.commit_plan_handoff(
                &metadata,
                (&session).into(),
                &candidate_workflow,
                &prompt,
            )?;
            // 批准初始化快照记入目标 journal；失败整体放弃候选。
            if let Some(event) = super::turn::todo_snapshot(&candidate_workflow) {
                journal.event(&event);
                journal.check()?;
            }
            Ok((session, journal))
        })();
        let (session, journal) = match prepared {
            Ok(pair) => pair,
            Err(error) => {
                // The candidate session file may remain in the session list;
                // the planning session is untouched and stays Plan/Ready.
                return Err(self.abort_execution_preparation(registry, error));
            }
        };
        // Source handoff record: plan executed, planning session stays in Plan
        // mode for read-only review; bare /plan toggles out without rebinding.
        if let Some(plan) = &mut self.workflow.plan {
            plan.status = PlanStatus::Executed;
        }
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            // 两个 journal 未全部成功：源链接保留，待按 ID 协调。
            return Err(self.abort_execution_preparation(registry, error));
        }
        self.session = session;
        self.journal = journal;
        self.workflow = candidate_workflow;
        self.tools.clear_changes();
        self.last_request = None;
        self.pending_instructions = None;
        self.pending_project_load = None;
        self.reported_project_load = None;
        self.tools.mcp = registry;
        self.finish_approved_move(pending_move, token);
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: Some(Vec::new()),
            cleanup,
            // 空 replay 的新会话总是携带批准初始化快照；源规划会话保持
            // 只读预览，绝不伪装成已执行。
            todo_snapshot: super::turn::todo_snapshot(&self.workflow),
        })
    }

    /// Resume a saved session in the worker, adapting the managed MCP
    /// registry to the target workflow mode through the same safe cleanup
    /// path: entering a Plan session stops the current registry first, and a
    /// Plan session never connects MCP at all.
    pub(crate) fn resume(
        &mut self,
        id: &str,
        cancellation: &CancellationToken,
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        let mut restored = self.prepare_resume(id)?;
        let target_mode = restored.checkpoint.workflow.mode;
        match (self.workflow.mode, target_mode) {
            (WorkMode::Plan, WorkMode::Plan) => {}
            (WorkMode::Plan, WorkMode::Execute) => {
                // The Plan registry is empty; finish any pending cleanup, then
                // connect ordinary execution MCP.
                self.ensure_plan_mcp_shutdown(cancellation)?;
                let config = self
                    .catalog
                    .resolve(&self.current_model.reference)
                    .map_err(|_| anyhow!("current model is unavailable"))?;
                self.tools.mcp = Registry::connect(
                    &config.settings.mcp_servers,
                    self.tools.cwd(),
                    cancellation,
                )?;
            }
            (WorkMode::Execute, WorkMode::Plan) => {
                // Stop the managed registry before the planning session can
                // rely on the Plan gate; failure aborts the resume.
                self.ensure_plan_mcp_shutdown(cancellation)?;
            }
            (WorkMode::Execute, WorkMode::Execute) => {}
        }
        let cleanup = self.tools.mcp.cleanup()?;
        let todo_snapshot = sync_restored_todos(
            &self.tools,
            &mut restored.journal,
            &mut restored.checkpoint.workflow,
            &restored.replay,
        )?;
        restored.journal.history_replaced();
        self.session = restored.checkpoint.session;
        self.workflow = restored.checkpoint.workflow;
        self.session
            .set_tools(self.tools.all_definitions(self.workflow.mode));
        self.journal = restored.journal;
        self.pending_project_load = None;
        self.reported_project_load = super::turn::reported_project_load(&restored.replay);
        self.replay = restored.replay;
        self.tools.clear_changes();
        if let Some(debug) = &mut self.debug {
            debug.set_prompt_recorded(true);
        }
        self.last_request = None;
        self.pending_instructions = None;
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: Some(std::mem::take(&mut self.replay)),
            cleanup,
            todo_snapshot,
        })
    }
}

#[cfg(test)]
mod tests;
