//! 任务入口：计划租约、运行编排与任务收尾边界。
use super::{
    Agent,
    input::InputQueue,
    persistence::Metadata,
    turn::{self, append_user_to, log_task_finished},
};
use anyhow::{Result, bail, ensure};
use pp_core::cancellation::{CancellationToken, Cancelled, RunOutcome};
use pp_core::events::AgentEvent;

impl Agent {
    pub(in crate::agent) fn connect_pending_plan_mcp(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if self.workflow.mode == pp_core::plan::WorkMode::Plan {
            let result = self
                .tools
                .mcp
                .connect_enabled(cancellation, crate::mcp::Access::ReadOnly);
            if let Err(error) = result {
                if error.is::<pp_core::cancellation::CleanupError>() {
                    self.plan_cleanup_error = Some(format!(
                        "MCP cleanup failed for Plan mode: {error:#}; run /plan again to retry."
                    ));
                }
                return Err(error);
            }
            self.session
                .set_tools(self.tools.all_definitions(self.workflow.mode));
        }
        Ok(())
    }

    pub(in crate::agent) fn ensure_task_plan_lease(&mut self) -> Result<()> {
        let Some(document) = &self.workflow.plan else {
            self.plan_lease = None;
            return Ok(());
        };
        if self.workflow.mode == pp_core::plan::WorkMode::Execute
            && matches!(
                document.status,
                pp_core::plan::PlanStatus::Completed | pp_core::plan::PlanStatus::Abandoned
            )
            && self.workflow.todo_plan_id.is_none()
        {
            self.plan_lease = None;
            return Ok(());
        }
        if self
            .plan_lease
            .as_ref()
            .is_some_and(|lease| lease.id() == document.id)
        {
            return Ok(());
        }
        self.plan_lease = None;
        self.plan_lease = Some(crate::tools::plan_catalog::PlanLease::acquire(
            self.tools.plan_workspace(),
            &document.id,
        )?);
        // 取得 lease 后按 ID 协调（锁顺序 lease → 登记表锁）：登记表
        // 重命名/移位/首次登记在任务与计划写工具访问前同步到绑定。
        self.reconcile_plan_location()
    }

    pub(in crate::agent) fn prepare_plan_task(&mut self, prompt: &str) -> Result<()> {
        if self.workflow.mode != pp_core::plan::WorkMode::Plan {
            return Ok(());
        }
        self.journal.check()?;
        // 裸 /plan 只待命：首条非空任务才分配文档并绑定 origin。
        let created = self.workflow.plan.is_none() && !prompt.trim().is_empty();
        let before_creation = created.then(|| self.workflow.clone());
        if created {
            self.workflow.plan = Some(pp_core::plan::PlanDocument::new()?);
        }
        if self.workflow.plan.is_none() {
            return Ok(());
        }
        let plan = self.workflow.plan.as_mut().expect("plan allocated");
        ensure!(
            !matches!(
                plan.status,
                pp_core::plan::PlanStatus::Executed | pp_core::plan::PlanStatus::Abandoned
            ),
            "This plan has already been handed off; use /plan to toggle."
        );
        // 列表打开的 Plans 也可继续讨论；接受讨论后统一降为 Draft，
        // 只开放规划工具，重新提交并明确批准前不得实施。
        let old_status = plan.status;
        let set_origin = plan.origin_prompt.is_empty() && !prompt.trim().is_empty();
        let persist = created || set_origin || old_status == pp_core::plan::PlanStatus::Ready;
        let previous = before_creation.or_else(|| persist.then(|| self.workflow.clone()));
        let plan = self.workflow.plan.as_mut().expect("plan allocated");
        if set_origin {
            plan.origin_prompt = prompt.to_owned();
        }
        if plan.status == pp_core::plan::PlanStatus::Ready {
            plan.status = pp_core::plan::PlanStatus::Draft;
        }
        if created {
            self.workflow.todo_plan_id = self.workflow.plan.as_ref().map(|plan| plan.id.clone());
            self.workflow.todos.clear();
        }
        if let Some(previous) = previous {
            self.commit_workflow(previous)?;
        }
        Ok(())
    }

    pub fn run(
        &mut self,
        prompt: &str,
        cancellation: &CancellationToken,
        input: Option<&InputQueue>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        self.run_user(prompt, None, cancellation, input, emit)
    }

    /// UserPromptSubmit 的结论：拒绝或 `continue:false` 直接失败，允许时返回
    /// 待注入的上下文。子 Agent 的任务文本不是人类输入，不经过这里。
    fn user_prompt_hook(
        &mut self,
        prompt: &str,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Vec<String>> {
        let Some(effect) = self.hook_user_input(prompt, cancellation, emit)? else {
            return Ok(Vec::new());
        };
        if let Some(reason) = effect.stop_reason {
            bail!("{reason}");
        }
        if let Some(reason) = effect.deny_reason {
            bail!("{reason}");
        }
        Ok(effect.additional_context)
    }

    pub fn input_limits(&self) -> pp_config::config::ToolLimits {
        self.tools.limits
    }

    pub fn run_input(
        &mut self,
        prepared: &crate::user_input::UserInput,
        cancellation: &CancellationToken,
        input: Option<&InputQueue>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        self.run_user(&prepared.display, Some(prepared), cancellation, input, emit)
    }

    fn run_user(
        &mut self,
        prompt: &str,
        prepared: Option<&crate::user_input::UserInput>,
        cancellation: &CancellationToken,
        input: Option<&InputQueue>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        tracing::info!(
            event = "task_started",
            pid = std::process::id(),
            operation = "run"
        );
        let started = std::time::Instant::now();
        let result = (|| -> Result<RunOutcome> {
            self.journal.check()?;
            if let Some(error) = &self.plan_cleanup_error {
                bail!("{error}");
            }
            ensure!(!prompt.trim().is_empty(), "prompt must not be blank");
            // 先检查后提交：拒绝或故障时不追加 user、不准备计划、不请求模型。
            let hook_context = self.user_prompt_hook(prompt, cancellation, emit)?;
            self.connect_pending_plan_mcp(cancellation)?;
            self.apply_runtime_models()?;
            self.restore_resolved(emit)?;
            self.ensure_task_plan_lease()?;
            self.prepare_plan_task(prompt)?;
            self.ensure_task_plan_lease()?;
            self.refresh_instructions()?;
            self.begin_task_status(prompt, cancellation);
            self.publish_context(emit)?;
            self.tools.begin_changes_task();
            if let Some(prepared) = prepared {
                turn::append_input_to(&mut self.session, prepared);
            } else {
                self.append_user(prompt);
            }
            if !hook_context.is_empty() {
                // 上下文跟随这条输入一起提交，恢复时只重放已保存的历史。
                turn::append_developer_to(&mut self.session, hook_context.join("\n\n"));
            }
            self.run_loop(Some(prompt), input, cancellation, emit)
        })();
        self.finish_task("run", started, result, cancellation, input, emit)
    }

    pub fn continue_session(
        &mut self,
        cancellation: &CancellationToken,
        input: Option<&InputQueue>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        tracing::info!(
            event = "task_started",
            pid = std::process::id(),
            operation = "continue"
        );
        let started = std::time::Instant::now();
        let result = (|| -> Result<RunOutcome> {
            self.journal.check()?;
            if let Some(error) = &self.plan_cleanup_error {
                bail!("{error}");
            }
            self.connect_pending_plan_mcp(cancellation)?;
            self.ensure_task_plan_lease()?;
            self.prepare_plan_task("")?;
            self.ensure_task_plan_lease()?;
            self.continue_task_status();
            self.run_loop(None, input, cancellation, emit)
        })();
        self.finish_task("continue", started, result, cancellation, input, emit)
    }

    // run 与 continue_session 共用的任务收尾：关闭输入队列；上下文发布失败
    // 不得掩盖取消或真实任务错误。
    pub(in crate::agent) fn finish_task(
        &mut self,
        operation: &str,
        started: std::time::Instant,
        result: Result<RunOutcome>,
        cancellation: &CancellationToken,
        input: Option<&InputQueue>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        self.finish_task_title();
        if self.task_status.view.is_none() {
            self.set_task_accounting(None);
        }
        let result = self.close_queue(input, result);
        let mut result = self.finish_run(result, cancellation);
        if let Err(error) = self.refresh_plan_progress(emit) {
            let error = match result {
                Err(original) => error.context(format!("Task also failed: {original:#}")),
                Ok(RunOutcome::Cancelled) => {
                    error.context("Task was cancelled; file changes were not rolled back.")
                }
                _ => error,
            };
            self.journal.error(&format!("{error:#}"));
            result = Err(error);
        }
        // 成功边界自动归档：仅当 close_queue、finish_run（含后台清理）与
        // 进度同步全部成功、结果确为 Completed 且取消未置位时尝试；计划
        // 未全部勾选或零任务时静默跳过（保留 Executed/Plans），任务集合
        // 与批准集合不一致及真实 IO/登记/journal 失败把结果转为错误。
        if matches!(&result, Ok(RunOutcome::Completed))
            && !cancellation.is_cancelled()
            && let Err(error) = self.archive_completed_plan(emit)
        {
            let error = error.context("Completed plan was not archived.");
            self.journal.error(&format!("{error:#}"));
            result = Err(error);
        }
        if !matches!(&result, Ok(RunOutcome::Completed)) {
            let _ = self.publish_context(emit);
        }
        log_task_finished(operation, started, &result);
        self.plan_lease = None;
        result
    }

    // 每条退出路径都关闭队列；关闭失败只在任务本身成功时成为主错误。
    fn close_queue(
        &self,
        input: Option<&InputQueue>,
        result: Result<RunOutcome>,
    ) -> Result<RunOutcome> {
        match input {
            None => result,
            Some(queue) => match (result, queue.close()) {
                (result, Ok(())) => result,
                (Ok(_), Err(error)) => Err(error),
                (Err(original), Err(_)) => Err(original),
            },
        }
    }

    pub(in crate::agent) fn append_user(&mut self, prompt: &str) {
        append_user_to(&mut self.session, prompt);
    }

    pub(in crate::agent) fn finish_run(
        &mut self,
        result: Result<RunOutcome>,
        cancellation: &CancellationToken,
    ) -> Result<RunOutcome> {
        let mut result = result;
        if cancellation.is_cancelled() || result.is_err() {
            self.cancel_lsp_diagnostics();
        }
        if cancellation.is_cancelled() {
            if let Some(binding) = &self.tools.subagent
                && let Err(cleanup) = binding.cancel_owned()
            {
                result = Err(cleanup);
            }
            self.client.cancel_pending_io();
        }
        let result = match self.tools.stop_background_tasks() {
            Ok(()) => result,
            Err(cleanup) => Err(match result {
                Err(original) if !original.is::<Cancelled>() => {
                    cleanup.context(format!("Task also failed: {original:#}"))
                }
                _ => cleanup,
            }),
        };
        let result = if !matches!(
            &result,
            Ok(RunOutcome::Completed | RunOutcome::AwaitingPlanReview)
        ) {
            if let Some(binding) = &self.tools.subagent {
                match binding.reap_owned() {
                    Ok(()) => result,
                    Err(cleanup) => Err(cleanup.context(format!(
                        "Parent run also stopped: {}",
                        result
                            .as_ref()
                            .err()
                            .map_or(String::new(), |e| format!("{e:#}"))
                    ))),
                }
            } else {
                result
            }
        } else {
            result
        };
        match result {
            Err(error) if error.is::<Cancelled>() => {
                self.journal.check()?;
                self.journal.cancelled()?;
                Ok(RunOutcome::Cancelled)
            }
            Err(error) => {
                self.journal.error(&format!("{error:#}"));
                self.journal.check()?;
                Err(error)
            }
            Ok(outcome) => {
                self.journal.check()?;
                Ok(outcome)
            }
        }
    }

    pub(in crate::agent) fn persist_pending_input(&mut self, user: Option<&str>) -> Result<()> {
        let metadata = Metadata {
            role_route: self.role_route.as_ref(),
            cwd: self.tools.cwd(),
            model_reference: &self.current_model.reference,
            think_level: self.think_level,
            edit_mode: self.tools.edit_mode(),
            tool_limits: self.tools.limits,
            base_system: &self.base_system,
            prompt_source: self.prompt_source.as_ref(),
        };
        self.journal.sent(
            &metadata,
            (&self.session).into(),
            &self.workflow,
            user,
            self.pending_instructions.as_deref(),
        )?;
        self.pending_instructions = None;
        Ok(())
    }
}
