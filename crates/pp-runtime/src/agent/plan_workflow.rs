//! Worker-side Plan mode lifecycle: mode entry/exit with access-scoped MCP
//! handoff, safe external plan import, review-token validation, and the two
//! post-approval execution handoffs.
//!
//! Every permission change is persisted through the journal's transactional
//! workflow record before or as part of the transition, and a failed MCP
//! cleanup keeps the fail-closed `plan_cleanup_error` gate so no request can
//! start with incomplete MCP access.

mod handoff;
mod source;
mod sync;
mod token;

use super::plan_sync::sync_restored_todos;
use super::{
    Agent, ExecutionPreparation, ModelChoice, OpenAiClient, Protocol, Session, ThinkLevel, history,
    persistence::{self, Metadata},
};
use crate::mcp::Access;
use crate::project_instructions;
use anyhow::{Result, anyhow};
use pp_config::config::{ModelRole, RoleRoute};
use pp_config::model_state::Selection;
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::model::ModelSource;
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
    /// 执行期模型：唯一决议出口 `resolve()`（手动覆盖 → 配置默认），不自动套用角色。
    fn default_execution_model(&self) -> Result<ExecutionModel> {
        let (config, selection) = Self::resolve_selection(&self.models)?;
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
                reference: selection.reference,
                model: config.model,
                protocol: config.protocol,
                thinking: config.thinking,
                context_window: config.context_window,
            },
            effort: selection.effort,
            route: None,
            client,
            same_response_source,
        })
    }

    fn execution_session(
        &self,
        model: &ExecutionModel,
        workflow: &WorkflowState,
        keep: bool,
    ) -> Result<Session> {
        let definitions = self.tools.all_definitions(WorkMode::Execute);
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
        self.model_source = self.models.source();
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
    ) -> Result<(
        String,
        Vec<pp_config::skills::SkillHeader>,
        Vec<project_instructions::Document>,
    )> {
        let documents = self.tools.project_documents()?;
        let project =
            self.tools.project_scope_summary() + &project_instructions::render(&documents);
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
            let mut system = project_instructions::compose(&self.base_system, &project, &summary);
            if !instructions.is_empty() {
                system.push_str("\n\n");
                system.push_str(instructions);
            }
            Ok(system)
        }?;
        Ok((system, skills, documents))
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
    /// 收尾子 Agent 后按 Plan 权限补连未连接的服务；已连接进程一律复用，
    /// 不断开、不重读配置。补连或清理失败时保留阻断标记，等待用户重试
    /// /plan。
    pub(super) fn ensure_plan_mcp_access(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.stop_subagents()?;
        cancellation::check(cancellation)?;
        let result = self
            .tools
            .mcp
            .connect_enabled(cancellation, Access::ReadOnly);
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
        Ok(())
    }

    /// 离开 Plan 前按执行权限补连未连接的服务；已连接进程一律复用，不断开、
    /// 不重读配置。`CleanupError` 保留 fail-closed 标记，成功重试后解除。
    pub(super) fn ensure_execute_mcp_access(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        cancellation::check(cancellation)?;
        let result = self.tools.mcp.connect_enabled(cancellation, Access::Full);
        if let Err(error) = result {
            if error.is::<pp_core::cancellation::CleanupError>() {
                self.plan_cleanup_error = Some(format!(
                    "MCP cleanup failed before execution: {error:#}; run /plan again to retry."
                ));
            }
            return Err(error);
        }
        self.plan_cleanup_error = None;
        Ok(())
    }

    /// `/plan` toggle 的进入方向：切到 Plan 待命（Plan+None），不绑定计划
    /// 文档；工作流权威先持久化，之后才切换 Plan 访问权限。已在 Plan 时只
    /// 重试一次失败的补连（`/plan <prompt>` 继续当前草稿也复用该入口）。
    pub fn enter_plan(&mut self, cancellation: &CancellationToken) -> Result<WorkflowState> {
        self.stop_subagents()?;
        cancellation::check(cancellation)?;
        if self.workflow.mode == WorkMode::Plan {
            self.ensure_plan_mcp_access(cancellation)?;
            return Ok(self.workflow.clone());
        }
        self.journal.check()?;
        // 手动覆盖优先：Plan 模式不改用 PLAN 角色，保持用户显式选择。
        let (model, route, source) = match self.models.override_selection() {
            Some(selection) => (selection, None, ModelSource::Override),
            None => {
                let assignment = self.catalog.role_selection(
                    pp_config::config::ModelRole::Plan,
                    &self.current_model.reference,
                    self.think_level,
                )?;
                let route = assignment
                    .enabled
                    .then(|| self.catalog.role_route(pp_config::config::ModelRole::Plan))
                    .flatten();
                let source = if route.is_some() {
                    ModelSource::Role
                } else {
                    self.models.source()
                };
                (
                    Selection {
                        reference: assignment.model,
                        effort: assignment.effort,
                    },
                    route,
                    source,
                )
            }
        };
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
        self.role_route = route;
        let changed = self
            .switch_model_with_effort(&model.reference, Some(model.effort))
            .inspect(|_| {
                self.model_source = source;
            })
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

    /// Resume a saved session in the worker, connecting MCP services for the
    /// target workflow mode only; existing connections and the loaded
    /// configuration are reused (no registry rebuild, no config reload).
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
            self.journal.stats_database(),
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
        // 只按目标模式补连缺失服务；已连接进程与已加载配置原样复用。
        if target_mode == WorkMode::Execute {
            self.ensure_execute_mcp_access(cancellation)?;
        } else {
            self.ensure_plan_mcp_access(cancellation)?;
        }
        let definitions = self.tools.all_definitions(target_mode);
        let (choice, effort) = model
            .as_ref()
            .map_or((&self.current_model, self.think_level), |model| {
                (&model.choice, model.effort)
            });
        let (session, workflow) = self.resumed_session(checkpoint, choice, effort, definitions)?;
        cancellation::check(cancellation)?;
        if let Some(model) = &model {
            let metadata =
                model.metadata(&self.tools, &self.base_system, self.prompt_source.as_ref());
            journal.commit_plan_exit(&metadata, (&session).into(), &workflow)?;
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
        self.pending_project_load.clear();
        self.reported_project_load = super::turn::reported_project_load(&replay);
        self.tools.reset_project_instructions(
            self.reported_project_load
                .keys()
                .map(std::path::PathBuf::from),
        );
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
