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
    Agent, ExecutionPreparation, ModelChoice, OpenAiClient, Protocol, Session, ThinkLevel, history,
    persistence::{self, Metadata},
};
use crate::agent::input::InputQueue;
use crate::mcp::{Access, Registry};
use crate::project_instructions;
use crate::tools::plan_catalog;
use anyhow::{Result, anyhow, bail, ensure};
use pp_config::config::{ModelRole, RoleRoute};
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::plan;
use pp_core::plan::ExecutionContext;
use pp_core::plan::PlanLocation;
use pp_core::plan::PlanReview;
use pp_core::plan::PlanStatus;
use pp_core::plan::ReviewToken;
use pp_core::plan::WorkMode;
use pp_core::plan::WorkflowState;

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

struct ExecutionModel {
    choice: ModelChoice,
    effort: ThinkLevel,
    route: Option<RoleRoute>,
    client: OpenAiClient,
    same_response_source: bool,
}

impl ExecutionModel {
    fn metadata<'a>(
        &'a self,
        tools: &'a crate::tools::Tools,
        base_system: &'a str,
        prompt_source: Option<&'a crate::system_prompt::Snapshot>,
    ) -> Metadata<'a> {
        Metadata {
            role_route: self.route.as_ref(),
            cwd: tools.cwd(),
            model_reference: &self.choice.reference,
            think_level: self.effort,
            edit_mode: tools.edit_mode(),
            tool_limits: tools.limits,
            base_system,
            prompt_source,
        }
    }
}

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
            _ => unreachable!("saved instructions belong to the current protocol"),
        }
    }
}

impl Agent {
    fn default_execution_model(&self) -> Result<ExecutionModel> {
        let (config, effort, route) = Self::resolve_default_model(&self.catalog)?;
        let same_response_source = self.client.same_source(&config)
            && same_provider(&self.current_model.reference, &config.model_reference);
        let mut client = OpenAiClient::new(&config)?;
        #[cfg(debug_assertions)]
        if let Some(scripts) = self.client.mock_scripts() {
            client.set_mock_scripts(scripts.clone())?;
        }
        client.set_task_accounting(self.task_status.accounting.clone());
        Ok(ExecutionModel {
            choice: ModelChoice {
                reference: config.model_reference,
                model: config.model,
                protocol: config.protocol,
                thinking: config.thinking,
                context_window: config.context_window,
            },
            effort,
            route,
            client,
            same_response_source,
        })
    }

    fn execution_session(
        &self,
        model: &ExecutionModel,
        workflow: &WorkflowState,
        registry: &Registry,
        keep: bool,
    ) -> Result<Session> {
        let definitions = self
            .tools
            .definitions_with_registry(WorkMode::Execute, registry);
        let system = self.compose_prompt(
            &crate::subagent::catalog::workflow_instructions(workflow),
            workflow.mode_name(),
            &model.choice.model,
            definitions.iter().map(|tool| tool.name.clone()).collect(),
        )?;
        let edit_mode = self.tools.edit_mode();
        let mut session = if !keep {
            Session::new(
                model.choice.model.clone(),
                model.choice.protocol,
                String::new(),
                edit_mode,
            )
        } else if model.choice.protocol != self.current_model.protocol
            || model.choice.protocol == Protocol::Response && !model.same_response_source
        {
            history::convert(
                &self.session,
                model.choice.model.clone(),
                model.choice.protocol,
                edit_mode,
            )?
        } else {
            self.session.clone()
        };
        session.set_model(&model.choice.model);
        session.set_instructions(system);
        session.set_tools(definitions);
        session.set_think_level(model.effort);
        Ok(session)
    }

    fn install_execution_model(&mut self, model: ExecutionModel) {
        self.client = model.client;
        self.current_model = model.choice;
        self.think_level = model.effort;
        self.role_route = model.route;
        self.last_request = None;
        self.pending_instructions = None;
    }

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
    ) -> Result<(String, Vec<pp_config::skills::SkillHeader>, String)> {
        let project = project_instructions::load(self.tools.cwd())?;
        let skills = self
            .tools
            .skills
            .lock()
            .map_err(|_| anyhow!("skill registry unavailable"))?
            .headers();
        let summary = pp_config::skills::Skills::format_summary(&skills);
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
            &crate::subagent::catalog::workflow_instructions(workflow),
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

/// 执行交接准备出的 MCP registry。
///
/// `reused` 为真表示沿用 Plan 期已有的连接，只补连需要执行权限的服务；
/// 失败时只需断开本次补连的服务。为假表示按新配置重建过，失败时整体
/// 回收。`Registry` 克隆共享同一批连接跟踪，交给 UI 后旧句柄被 supersede。
pub(super) struct PreparedRegistry {
    pub(super) registry: Registry,
    pub(super) reused: bool,
}

/// 按 ID 协调结果：绑定需采纳登记、登记需首次建立，或已一致。
///
/// 按 ID 协调计划身份：登记表始终优先，把绑定文档同步到 canonical
/// 名称/位置；纯路径/状态变化不递增正文 revision，但使旧 path token
/// 失效。未登记的 revision>=1 旧绑定按迁移规则首次登记：Draft/Ready 的
/// 旧 Plans 绑定保留旧名迁回 Drafts（安全搬迁），Executed 仍在 Plans，
/// Abandoned 保留原位置只读，历史 Executed 登记到 Done 时只读同步
/// Completed，不重新授权。墓碑、路径/正文缺失一律报错，绝不从历史
/// 快照恢复，也不让历史 Draft/Ready 绑定把已提升的位置退回 drafts。
///
/// 绑定文档的身份三元组：登记协调是否改变了名称/位置/状态。
///
/// Plan 模式只同步计划正文，不生成任务投影；任务运行态仅在执行时建立。
///
/// 共享同步入口安全导入实际 Markdown；Plan 只更新正文，Execute 才同步任务。
///
/// 恢复只携带一次同步新生成的快照；replay 已包含相同快照时不重复追加。
///
/// 启动恢复与交互式 resume 共用同一文件同步和快照去重边界。
///
/// Register on-disk plan bytes as a new revision against the bound workflow.
/// Returns `false` when the file matches the registered snapshot. Draft/Ready
/// documents become Ready only when the imported content has a genuine
/// Markdown task list; Executed/Abandoned documents remain read-only after
/// importing progress. The whole workflow is rolled back when the journal
/// rejects the write.
///
impl Agent {
    /// 收尾子 Agent 后，仅保留 Plan 明确授权的服务。回收失败时保留
    /// 连接跟踪和阻断标记，等待用户重试 /plan。
    pub(super) fn ensure_plan_mcp_access(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.stop_subagents()?;
        cancellation::check(cancellation)?;
        let result = if self.plan_cleanup_error.is_some() {
            self.tools.mcp.shutdown()
        } else {
            Ok(())
        };
        let result = result.and_then(|()| self.tools.mcp.restrict_to_read_only());
        self.session
            .set_tools(self.tools.all_definitions(self.workflow.mode));
        if let Err(error) = result {
            if error.is::<pp_core::cancellation::CleanupError>() {
                self.plan_cleanup_error = Some(format!(
                    "MCP cleanup failed for Plan mode: {error:#}; run /plan again to retry."
                ));
            }
            return Err(error);
        }
        self.plan_cleanup_error = None;
        self.connect_pending_plan_mcp(cancellation)
    }

    /// 准备执行交接使用的 MCP registry。
    ///
    /// 配置与 Plan 期一致时复用现有连接，只补连 Plan 期被 suspend 的服务；
    /// 配置真正变化时才整体重建。补连失败在内部先回滚本次新连的服务，
    /// 使 Plan 期连接与工具表保持不变。
    fn prepare_execution_registry(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<PreparedRegistry> {
        // 先校验配置：无效配置在回收任何连接之前就报错。
        let fresh = Registry::read_config(&self.catalog.mcp_path())?;
        cancellation::check(cancellation)?;
        let cwd = self.tools.cwd().to_owned();
        if self.tools.mcp.config_matches(&fresh) {
            // 复用分支：只补连“未连接且需要执行权限”的服务，成功后再克隆
            // registry（共享同一 tracking，Cleanup::supersede 语义不变）。
            if let Err(error) = self
                .tools
                .mcp
                .connect_enabled(&cwd, cancellation, Access::Full)
            {
                return Err(match self.tools.mcp.restrict_to_read_only() {
                    Ok(()) => error,
                    Err(cleanup) => {
                        self.plan_cleanup_error = Some(format!(
                            "MCP cleanup failed before execution: {cleanup:#}; run /plan again to retry."
                        ));
                        cleanup.context(format!("Execution preparation also failed: {error:#}"))
                    }
                });
            }
            return Ok(PreparedRegistry {
                registry: self.tools.mcp.clone(),
                reused: true,
            });
        }
        // 配置变化：先回收 Plan 期连接，再按新配置连接，避免重复启动服务。
        let mut registry = fresh;
        if let Err(error) = self.tools.mcp.shutdown() {
            self.plan_cleanup_error = Some(format!(
                "MCP cleanup failed before execution: {error:#}; run /plan again to retry."
            ));
            return Err(error);
        }
        if let Err(error) = registry.connect_enabled(&cwd, cancellation, Access::Full) {
            return Err(self.abort_execution_preparation(
                PreparedRegistry {
                    registry,
                    reused: false,
                },
                error,
            ));
        }
        Ok(PreparedRegistry {
            registry,
            reused: false,
        })
    }

    /// 交接失败后的 MCP 回滚。
    ///
    /// 复用分支只断开本次补连的服务，Plan 期已有连接与 Plan 工具表保持不变；
    /// 重建分支沿用整体 shutdown，并把未能回收的连接交回 `self.tools.mcp`，
    /// 保持 Cleanup 快照覆盖。回滚失败仍升级为 typed 清理上下文。
    fn abort_execution_preparation(
        &mut self,
        prepared: PreparedRegistry,
        error: anyhow::Error,
    ) -> anyhow::Error {
        if prepared.reused {
            return match self.tools.mcp.restrict_to_read_only() {
                Ok(()) => error,
                Err(cleanup) => {
                    self.plan_cleanup_error = Some(format!(
                        "MCP cleanup failed before execution: {cleanup:#}; run /plan again to retry."
                    ));
                    cleanup.context(format!("Execution preparation also failed: {error:#}"))
                }
            };
        }
        let mut registry = prepared.registry;
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
    pub fn enter_plan(&mut self, cancellation: &CancellationToken) -> Result<WorkflowState> {
        self.stop_subagents()?;
        cancellation::check(cancellation)?;
        if self.workflow.mode == WorkMode::Plan {
            self.ensure_plan_mcp_access(cancellation)?;
            return Ok(self.workflow.clone());
        }
        self.journal.check()?;
        let selection = self.catalog.role_selection(
            pp_config::config::ModelRole::Plan,
            &self.current_model.reference,
            self.think_level,
        )?;
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
        self.cancel_lsp_diagnostics();
        self.ensure_plan_mcp_access(cancellation)?;
        let previous_route = self.role_route.take();
        self.role_route = if selection.enabled {
            self.catalog.role_route(pp_config::config::ModelRole::Plan)
        } else {
            previous_route.clone()
        };
        let changed = self
            .switch_model_with_effort(&selection.model, Some(selection.effort))
            .and_then(|changed| {
                if !changed && previous_route != self.role_route {
                    self.checkpoint_route()
                } else {
                    Ok(())
                }
            });
        if let Err(error) = changed {
            self.role_route = previous_route;
            return Err(error);
        }
        Ok(self.workflow.clone())
    }

    /// Register on-disk plan bytes as a new revision. Returns `false` when the
    /// file matches the registered snapshot. Draft/Ready documents become
    /// Ready only when the imported content has a genuine Markdown task list;
    /// Executed/Abandoned documents remain read-only after importing progress.
    fn import_plan_revision(&mut self, content: String, new_title: &str) -> Result<bool> {
        import_plan_content(&mut self.journal, &mut self.workflow, content, new_title)
    }

    /// 通过 NOFOLLOW 工作区句柄读取计划，导入外部修订并签发绑定当前
    /// 会话、计划、路径及精确正文的审批 token。文件缺失或不可读时报错，
    /// 不回退到日志快照。Executed/Abandoned 可刷新但不恢复执行授权；
    /// Plan 仅同步正文，Execute 才刷新任务投影，已脱离的普通清单不被替换。
    pub fn read_plan(&mut self) -> Result<PlanReview> {
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
        // 按 ID 协调登记路径并导入外部修订；Plan 不生成 TODO。
        // 墓碑、搬迁与首次登记在此暴露。
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
        let prepared = self.prepare_execution_registry(cancellation)?;
        let handoff = (|| -> Result<_> {
            let cleanup = prepared.registry.cleanup()?;
            // 解除绑定，保留磁盘上的计划和登记。
            let workflow = WorkflowState::default();
            let session = self.execution_session(&model, &workflow, &prepared.registry, true)?;
            let metadata =
                model.metadata(&self.tools, &self.base_system, self.prompt_source.as_ref());
            cancellation::check(cancellation)?;
            self.journal
                .commit_plan_exit(&metadata, (&session).into(), &workflow)?;
            Ok((workflow, session, cleanup))
        })();
        let (workflow, session, cleanup) = match handoff {
            Ok(handoff) => handoff,
            Err(error) => return Err(self.abort_execution_preparation(prepared, error)),
        };
        self.session = session;
        self.workflow = workflow;
        self.install_execution_model(model);
        self.tools.mcp = prepared.registry;
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
    pub(super) fn archive_completed_plan(
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
    pub fn save_plan_and_leave(
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
        self.leave_plan(cancellation, None)
    }

    /// 批准已审阅计划并准备执行交接；可先在 Plan 权限下压缩上下文。
    /// 提交前可取消地连接候选 MCP，再校验 token；失败时停止新 registry，
    /// 保持 Plan/Ready，禁止从未完成的交接发起实施请求。
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
        // Finish any pending Plan cleanup before touching approval state.
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
        let model = self.default_execution_model()?;
        let prepared = self.prepare_execution_registry(cancellation)?;
        let fresh = context == ExecutionContext::Fresh;
        let candidate = (|| -> Result<_> {
            let cleanup = prepared.registry.cleanup()?;
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
            let mut session =
                self.execution_session(&model, &workflow, &prepared.registry, !fresh)?;
            super::turn::append_user_to(&mut session, &prompt);
            // 准备原生请求后再核对输入队列与计划正文，之后才发布计划移动。
            cancellation::check(cancellation)?;
            self.check_plan_token_for_approval(&token, input)?;
            Ok((workflow, session, prompt, cleanup))
        })();
        let (workflow, session, prompt, cleanup) = match candidate {
            Ok(candidate) => candidate,
            Err(error) => return Err(self.abort_execution_preparation(prepared, error)),
        };
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
                return Err(self.abort_execution_preparation(prepared, error));
            }
        };
        self.session = session;
        self.workflow = workflow;
        self.install_execution_model(model);
        self.tools.mcp = prepared.registry;
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
            self.pending_project_load = None;
            self.reported_project_load = None;
        }
        self.finish_approved_move(pending_move, &token);
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: fresh.then(Vec::new),
            cleanup,
            todo_snapshot: super::turn::todo_snapshot(&self.workflow),
        })
    }

    pub(super) fn resumed_session(
        &self,
        checkpoint: persistence::Checkpoint,
        choice: &ModelChoice,
        effort: ThinkLevel,
        definitions: Vec<async_openai::types::chat::FunctionObject>,
    ) -> Result<(Session, WorkflowState)> {
        let same_model = checkpoint.model_reference == choice.reference
            && matches!(&checkpoint.session, Session::Response(request)
                if choice.protocol == Protocol::Response
                    && request.model.as_deref() == Some(choice.model.as_str()));
        let source = if same_model
            || matches!(checkpoint.session, Session::Completion(_))
                && choice.protocol == Protocol::Completion
        {
            checkpoint.session
        } else {
            history::convert(
                &checkpoint.session,
                choice.model.clone(),
                choice.protocol,
                self.tools.edit_mode(),
            )?
        };
        let system = self.compose_prompt(
            &crate::subagent::catalog::workflow_instructions(&checkpoint.workflow),
            checkpoint.workflow.mode_name(),
            &choice.model,
            definitions.iter().map(|tool| tool.name.clone()).collect(),
        )?;
        let mut session = Session::new(
            choice.model.clone(),
            choice.protocol,
            system,
            self.tools.edit_mode(),
        );
        match (&mut session, source) {
            (Session::Completion(target), Session::Completion(source)) => {
                let skip_system = usize::from(matches!(
                    source.messages.first(),
                    Some(super::ChatCompletionRequestMessage::System(_))
                ));
                target
                    .messages
                    .extend(source.messages.into_iter().skip(skip_system));
            }
            (Session::Response(target), Session::Response(source)) => target.input = source.input,
            _ => unreachable!("history converted to the target protocol"),
        }
        session.set_tools(definitions);
        session.set_think_level(effort);
        Ok((session, checkpoint.workflow))
    }

    /// Resume a saved session in the worker, adapting the managed MCP
    /// registry to the target workflow mode through the same safe cleanup
    /// path used by /plan.
    pub fn resume(
        &mut self,
        id: &str,
        cancellation: &CancellationToken,
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        self.stop_subagents()?;
        let persistence::RestoredSession {
            mut checkpoint,
            mut journal,
            replay,
        } = persistence::load(
            &pp_config::config::global_dir()?.join("sessions"),
            self.journal.lock_dir(),
            Some(id),
            true,
        )?;
        let target_mode = checkpoint.workflow.mode;
        let leaving_plan = self.workflow.mode == WorkMode::Plan && target_mode == WorkMode::Execute;
        let stale_plan_route = target_mode == WorkMode::Execute
            && checkpoint
                .role_route
                .as_ref()
                .is_some_and(|route| route.role == ModelRole::Plan);
        let model = if leaving_plan || stale_plan_route {
            Some(self.default_execution_model()?)
        } else {
            None
        };
        let todo_snapshot =
            sync_restored_todos(&self.tools, &mut journal, &mut checkpoint.workflow, &replay)?;
        let registry = if leaving_plan {
            self.ensure_plan_mcp_access(cancellation)?;
            Some(self.prepare_execution_registry(cancellation)?)
        } else {
            if target_mode == WorkMode::Plan {
                self.ensure_plan_mcp_access(cancellation)?;
            }
            None
        };
        let prepared = (|| -> Result<_> {
            let active_registry = registry
                .as_ref()
                .map_or(&self.tools.mcp, |prepared| &prepared.registry);
            let cleanup = active_registry.cleanup()?;
            let definitions = self
                .tools
                .definitions_with_registry(target_mode, active_registry);
            let (choice, effort) = model
                .as_ref()
                .map_or((&self.current_model, self.think_level), |model| {
                    (&model.choice, model.effort)
                });
            let (session, workflow) =
                self.resumed_session(checkpoint, choice, effort, definitions)?;
            cancellation::check(cancellation)?;
            if let Some(model) = &model {
                let metadata =
                    model.metadata(&self.tools, &self.base_system, self.prompt_source.as_ref());
                journal.commit_plan_exit(&metadata, (&session).into(), &workflow)?;
            }
            Ok((session, workflow, cleanup))
        })();
        let (session, workflow, cleanup) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                return Err(match registry {
                    Some(prepared) => self.abort_execution_preparation(prepared, error),
                    None => error,
                });
            }
        };
        if let Some(prepared) = registry {
            self.tools.mcp = prepared.registry;
        }
        if let Some(model) = model {
            self.install_execution_model(model);
        } else {
            journal.history_replaced();
        }
        self.session = session;
        self.reset_lsp_diagnostics();
        self.workflow = workflow;
        self.journal = journal;
        self.tools.artifacts = Default::default();
        // 恢复的会话是新的有效根；不承诺旧临时管道正文跨恢复存在。
        self.tools.reset_pipe_store();
        if let Some(id) = self.journal.id() {
            let _ = self.tools.artifacts.bind(self.journal.directory(), id);
        }
        self.pending_project_load = None;
        self.reported_project_load = super::turn::reported_project_load(&replay);
        self.replay = replay;
        self.tools.clear_changes();
        if let Some(debug) = &mut self.debug {
            debug.set_prompt_recorded(true);
        }
        self.clear_session_usage();
        self.restore_session_cost();
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

fn same_provider(left: &str, right: &str) -> bool {
    left.split_once('/').map(|(provider, _)| provider)
        == right.split_once('/').map(|(provider, _)| provider)
}

#[cfg(test)]
mod tests;
