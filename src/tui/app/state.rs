use crate::tui::{
    App, CancellationToken, ExecutionContext, Instant, Level, ModelChoice, PlanCommand,
    PlanEnterAction, PlanReview, PlanStatus, ReviewToken, State, ThinkLevel, Transcript, WorkMode,
    WorkerCommand, WorkflowState, editor, plan, plan_leave,
};

impl App {
    pub(in crate::tui) fn notify(&mut self, level: Level, message: String) {
        self.notifications.show(level, message, Instant::now());
    }

    pub(in crate::tui) fn append(&mut self, label: &str, text: &str) {
        self.show_welcome = false;
        self.conversation.append(label, text);
    }

    /// 启动恢复时初始化模型字段，再接管可见对话。
    pub(in crate::tui) fn adopt_session(
        &mut self,
        replay: Vec<crate::agent::persistence::ReplayRecord>,
        choices: Vec<ModelChoice>,
        current_model: ModelChoice,
        think_level: ThinkLevel,
        default_think_level: ThinkLevel,
    ) -> Option<String> {
        self.choices = choices;
        self.current_model = Some(current_model);
        self.think_level = think_level;
        self.default_think_level = default_think_level;
        self.adopt_history(replay)
    }

    pub(in crate::tui) fn adopt_history(
        &mut self,
        replay: Vec<crate::agent::persistence::ReplayRecord>,
    ) -> Option<String> {
        self.close_ask_on_exit();
        self.subagent_autowake = false;
        self.token_speed.clear();
        self.history_browser = false;
        self.todo_panel.reset_view();
        self.conversation = self.conversation.fresh();
        self.input_history = Vec::new();
        let error = self.conversation.replay(replay, &mut self.input_history);
        self.input = editor();
        self.shell_mode = false;
        self.history_index = None;
        self.history_draft = None;
        self.model_picker_draft = None;
        self.model_panel = None;
        self.file_paths.clear();
        self.file_directory.clear();
        self.file_query = None;
        self.menu = None;
        self.menu_dismissed = true;
        self.show_welcome = false;
        self.refresh_candidates(false);
        error
    }

    pub(in crate::tui) fn fail_worker(&mut self) {
        self.close_ask_on_exit();
        if self.token_speed.is_active() {
            self.token_speed.clear();
        }
        let message = "Execution unavailable: agent worker channel closed; restart pp.";
        self.state = State::Unavailable;
        self.queue_paused = true;
        let _ = self.input_queue.close();
        self.active_cancellation = None;
        self.secondary_status = None;
        self.commands = None;
        self.events = None;
        self.unavailable = Some(message.into());
        self.tree_panel = None;
        self.changes_panel = None;
        self.plan_list_panel = None;
        self.plan_list_pending = None;
        self.plan_reenter_standby = false;
        self.plan_leave_confirmation = None;
        self.plan_panel = None;
        self.plan_after_enter = None;
        self.review_transcript = None;
        self.plan_review_pending = None;
        self.escape_tap.reset();
        // worker 已死时不再等待恢复回复，会话面板不能继续吞掉按键。
        self.session_panel = None;
        self.close_model_panel();
        self.conversation.fail();
        self.notify(Level::Error, message.into());
    }

    /// 应用 worker 发布的 workflow 快照；仅在正文变化时重算进度。
    pub(in crate::tui) fn set_workflow(&mut self, workflow: WorkflowState) {
        if let Some(document) = &workflow.plan {
            self.update_plan_progress_cache(document);
        } else {
            self.plan_panel = None;
            self.plan_progress = (0, 0);
        }
        self.todo_panel
            .update(crate::events::TodoState::from_workflow(&workflow));
        self.workflow = workflow;
        if let (Some(panel), Some(document)) = (&mut self.plan_panel, &self.workflow.plan) {
            panel.sync_document(document);
        }
    }

    /// 进度 (completed, total) 只随正文变化重算；所有调用点都在把
    /// `document` 写入 `self.workflow.plan` 之前，因此直接与当前绑定比较。
    pub(in crate::tui) fn update_plan_progress_cache(
        &mut self,
        document: &crate::plan::PlanDocument,
    ) {
        if self
            .workflow
            .plan
            .as_ref()
            .is_none_or(|plan| plan.content != document.content)
        {
            self.plan_progress = crate::plan::task_progress(&document.content);
        }
    }

    /// 执行期进度事件：更新匹配计划的快照与只读面板，不追加 Transcript。
    /// 正文 revision/content 变化重算进度；旧事件缺省的名称/位置/状态
    /// 字段保留接收端当前值，归档等纯状态变化在正文相同时也立即生效。
    pub(in crate::tui) fn apply_plan_progress(
        &mut self,
        plan_id: &str,
        revision: u64,
        content: String,
        file_name: Option<String>,
        location: Option<crate::plan::PlanLocation>,
        status: Option<crate::plan::PlanStatus>,
    ) {
        let Some(document) = self.workflow.plan.as_ref() else {
            return;
        };
        if document.id != plan_id {
            return;
        }
        let body_changed = document.revision != revision || document.content != content;
        let state_changed = file_name
            .as_ref()
            .is_some_and(|file_name| Some(file_name) != document.file_name.as_ref())
            || location.is_some_and(|location| location != document.location)
            || status.is_some_and(|status| status != document.status);
        if !body_changed && !state_changed {
            return;
        }
        let mut document = self.workflow.plan.take().expect("plan checked above");
        document.revision = revision;
        document.content = content;
        if let Some(file_name) = file_name {
            document.file_name = Some(file_name);
        }
        if let Some(location) = location {
            document.location = location;
        }
        if let Some(status) = status {
            document.status = status;
        }
        self.update_plan_progress_cache(&document);
        if let Some(panel) = &mut self.plan_panel {
            panel.set_progress(&document);
        }
        self.workflow.plan = Some(document);
    }

    pub(in crate::tui) fn open_plan_panel(&mut self, review: PlanReview) {
        // ReadPlan 的登记结果是权威快照；同步 workflow 中的文档与报告字段。
        let PlanReview {
            token: _,
            document,
            report,
        } = &review;
        self.update_plan_progress_cache(document);
        self.workflow.plan = Some(document.clone());
        self.workflow.review = report.clone();
        match &mut self.plan_panel {
            Some(panel) => panel.set_review(review),
            None => self.plan_panel = Some(plan::Panel::review(review)),
        }
        self.menu = None;
        self.menu_dismissed = true;
        self.file_query = None;
    }

    /// Execute 运行中的只读进度视图：使用 UI 已缓存的最后同步正文，不发 worker 命令。
    pub(in crate::tui) fn open_plan_progress_view(&mut self) {
        let Some(document) = self.workflow.plan.clone() else {
            self.notify(Level::Normal, "No saved plan.".into());
            return;
        };
        if document.content.is_empty() {
            self.notify(Level::Normal, "No saved plan.".into());
            return;
        }
        match &mut self.plan_panel {
            Some(panel) if panel.document().id == document.id => panel.set_progress(&document),
            _ => self.plan_panel = Some(plan::Panel::progress(document)),
        }
    }

    pub(in crate::tui) fn request_plan_read(&mut self) {
        if !matches!(self.state, State::Ready) || self.changes_panel.is_some() {
            return;
        }
        self.send(WorkerCommand::ReadPlan, State::ReadingPlan);
    }

    pub(in crate::tui) fn append_execution_echo(&mut self) {
        let text = match &self.workflow.plan {
            Some(plan) => {
                let path = plan.path().unwrap_or_else(|| "Not saved yet".to_owned());
                format!("Execute the approved plan {path} (r{}).", plan.revision)
            }
            None => "Execute the approved plan.".to_owned(),
        };
        self.append("You", &text);
    }

    /// 模式切换/审批仅允许 Ready、无 pending、无其它 modal；忙碌明确报错，不排队。
    pub(in crate::tui) fn plan_transition_ready(&mut self, pending_message: &str) -> bool {
        if !matches!(self.state, State::Ready) {
            self.notify(
                Level::Error,
                "Cancel the current task before changing Plan mode.".into(),
            );
            return false;
        }
        if self.has_pending() {
            self.notify(Level::Error, pending_message.into());
            return false;
        }
        if self.ask_panel.is_some()
            || self.skill_panel.is_some()
            || self.skills_pending
            || self.session_panel.is_some()
            || self.tree_panel.is_some()
            || self.changes_panel.is_some()
            || self.plan_list_panel.is_some()
            || self.model_panel.is_some()
            || self.queue_menu
            || self.plan_leave_confirmation.is_some()
        {
            self.notify(
                Level::Error,
                "Close the open panel before changing Plan mode.".into(),
            );
            return false;
        }
        true
    }

    pub(in crate::tui) fn start_plan_transition(
        &mut self,
        command: impl FnOnce(CancellationToken) -> WorkerCommand,
        state: State,
    ) {
        let cancellation = CancellationToken::new();
        if self.send(command(cancellation.clone()), state) {
            self.active_cancellation = Some(cancellation);
        }
    }

    pub(in crate::tui) fn apply_plan_command(&mut self, command: PlanCommand<'_>) {
        const PENDING_MODE: &str = "Apply pending input before changing Plan mode.";
        match command {
            // 裸 /plan 切换：Execute 进入 Plan 待命；已在 Plan 则退出并解除绑定。
            PlanCommand::Toggle => {
                if !self.plan_transition_ready(PENDING_MODE) {
                    return;
                }
                if self.workflow.mode == WorkMode::Execute || self.plan_retry_enter {
                    // Execute 进入 Plan，或上一次进入失败（MCP 清理残留）时重试进入。
                    self.plan_after_enter = Some(PlanEnterAction::Standby);
                    self.start_plan_transition(
                        |cancellation| WorkerCommand::EnterPlan { cancellation },
                        State::ChangingPlan,
                    );
                    return;
                }
                self.request_plan_leave(false);
            }
            PlanCommand::Prompt(prompt) => {
                if !self.plan_transition_ready(PENDING_MODE) {
                    return;
                }
                self.plan_after_enter = Some(PlanEnterAction::Prompt(prompt.to_owned()));
                self.start_plan_transition(
                    |cancellation| WorkerCommand::EnterPlan { cancellation },
                    State::ChangingPlan,
                );
            }
            PlanCommand::View => {
                if matches!(self.state, State::Running | State::Cancelling) {
                    // 运行中特许：只读进度视图，不向执行中的 worker 发阻塞读取。
                    self.open_plan_progress_view();
                    return;
                }
                if !self.plan_transition_ready(PENDING_MODE) {
                    return;
                }
                if self.has_saved_plan() {
                    self.request_plan_read();
                } else {
                    self.notify(Level::Normal, "No saved plan.".into());
                }
            }
        }
    }

    /// 未首存的规划需要显式放弃；确认只对当前绑定生效。
    pub(in crate::tui) fn request_plan_leave(&mut self, reenter: bool) {
        if let Some(document) = &self.workflow.plan
            && document.revision == 0
            && !document.origin_prompt.trim().is_empty()
        {
            self.plan_leave_confirmation =
                Some(plan_leave::Panel::new(document.id.clone(), reenter));
            self.menu = None;
            self.file_query = None;
            return;
        }
        self.start_plan_leave(None, reenter);
    }

    pub(in crate::tui) fn start_plan_leave(
        &mut self,
        discard_plan_id: Option<String>,
        reenter: bool,
    ) {
        if reenter {
            self.close_plan_list();
        }
        self.plan_reenter_standby = reenter;
        self.start_plan_transition(
            |cancellation| WorkerCommand::LeavePlan {
                cancellation,
                discard_plan_id,
            },
            State::ChangingPlan,
        );
    }

    pub(in crate::tui) fn apply_plan_leave_action(&mut self, action: plan_leave::Action) {
        match action {
            plan_leave::Action::None => {}
            plan_leave::Action::Cancel => {
                self.plan_leave_confirmation = None;
            }
            plan_leave::Action::Discard => {
                let Some(panel) = self.plan_leave_confirmation.take() else {
                    return;
                };
                if !matches!(self.state, State::Ready) || self.has_pending() {
                    self.notify(
                        Level::Error,
                        "Finish the current task and pending input before leaving Plan mode."
                            .into(),
                    );
                    return;
                }
                if self.workflow.mode != WorkMode::Plan
                    || self
                        .workflow
                        .plan
                        .as_ref()
                        .is_none_or(|plan| plan.id != panel.plan_id || plan.revision != 0)
                {
                    self.notify(Level::Error, "Plan changed; confirm leaving again.".into());
                    return;
                }
                self.start_plan_leave(Some(panel.plan_id), panel.reenter);
            }
        }
    }

    /// 审批/审查共用的前置条件：Ready、无 pending、文档 Ready 且持 token，
    /// 并开启输入队列；成功返回 token 与本次操作的取消令牌。
    pub(in crate::tui) fn plan_action_guard(
        &mut self,
        action: &str,
        ready_only: bool,
    ) -> Option<(ReviewToken, CancellationToken)> {
        if !matches!(self.state, State::Ready) {
            self.notify(
                Level::Error,
                format!("Cancel the current task before {action} the plan."),
            );
            return None;
        }
        if self.has_pending() {
            self.notify(
                Level::Error,
                format!("Apply pending input before {action} the plan."),
            );
            return None;
        }
        let token = {
            let panel = self.plan_panel.as_ref()?;
            if ready_only && panel.document().status != PlanStatus::Ready {
                return None;
            }
            panel.token().cloned()?
        };
        if let Err(error) = self.input_queue.begin_run() {
            self.queue_paused = true;
            self.notify(Level::Error, error.to_string());
            return None;
        }
        Some((token, CancellationToken::new()))
    }

    /// 审批动作：仅在 Ready、无 pending、文档 Ready 时发送；失败经 ReadPlan 安全刷新。
    pub(in crate::tui) fn approve_plan(&mut self, context: ExecutionContext) {
        let Some((token, cancellation)) = self.plan_action_guard("executing", true) else {
            return;
        };
        if self.send(
            WorkerCommand::ApprovePlan {
                token,
                context,
                cancellation: cancellation.clone(),
                input: self.input_queue.clone(),
            },
            State::PreparingExecution,
        ) {
            self.active_cancellation = Some(cancellation);
            self.secondary_status = Some(
                if context == ExecutionContext::Compact {
                    "Compacting context before execution..."
                } else {
                    "Preparing execution..."
                }
                .into(),
            );
        } else {
            let _ = self.input_queue.close();
        }
    }

    pub(in crate::tui) fn save_plan_and_leave(&mut self) {
        let Some((token, cancellation)) = self.plan_action_guard("saving", false) else {
            return;
        };
        if self.send(
            WorkerCommand::SavePlanAndLeave {
                token,
                cancellation: cancellation.clone(),
                input: self.input_queue.clone(),
            },
            State::ChangingPlan,
        ) {
            self.active_cancellation = Some(cancellation);
        } else {
            let _ = self.input_queue.close();
        }
    }

    /// 第 4 项：独立上下文 reviewer。仅 Ready、无 pending、文档 Ready 时发送；
    /// 进度只进独立 review transcript；完成或取消后经 ReadPlan 安全刷新回菜单。
    pub(in crate::tui) fn review_plan(&mut self) {
        let Some((token, cancellation)) = self.plan_action_guard("reviewing", true) else {
            return;
        };
        if self.send(
            WorkerCommand::ReviewPlan {
                token,
                cancellation: cancellation.clone(),
                input: self.input_queue.clone(),
            },
            State::ReviewingPlan,
        ) {
            self.active_cancellation = Some(cancellation);
            // 独立审查页替代审阅面板；主草稿、历史与会话保持不变。
            self.plan_panel = None;
            self.plan_list_panel = None;
            self.plan_list_pending = None;
            self.menu_dismissed = true;
            self.secondary_status = Some("Reviewing plan...".into());
            let mut review = self.conversation.fresh();
            review.begin_run();
            self.review_transcript = Some(review);
        } else {
            let _ = self.input_queue.close();
        }
    }

    pub(in crate::tui) fn review_transcript_tick(&mut self, now: Instant) -> bool {
        self.review_transcript
            .as_mut()
            .is_some_and(|review| review.tick(now))
    }

    /// 已登记至少一个修订的计划：/plan 打开审阅的前提条件。
    pub(in crate::tui) fn has_saved_plan(&self) -> bool {
        self.workflow
            .plan
            .as_ref()
            .is_some_and(|plan| plan.revision >= 1)
    }

    /// Plan/Ready：待审阅的计划会暂停自动派发，需要显式审阅或恢复队列后继续。
    pub(in crate::tui) fn plan_awaiting_review(&self) -> bool {
        self.workflow.mode == WorkMode::Plan
            && self
                .workflow
                .plan
                .as_ref()
                .is_some_and(|plan| plan.status == PlanStatus::Ready)
    }

    /// 当前显示的转录：独立审查页优先于主对话。
    pub(in crate::tui) fn needs_alternate(&self) -> bool {
        self.auth_panel.is_some()
            || self.subagent_panel.is_some()
            || self.history_browser
            || self.skill_panel.is_some()
            || self.session_panel.is_some()
            || self.changes_panel.is_some()
            // 读取完成前保留主屏幕，空历史或读取失败时无需切换屏幕。
            || self.tree_panel.as_ref().is_some_and(|panel| !panel.is_loading())
            || self.plan_list_panel.is_some()
            || self.plan_leave_confirmation.is_some()
            || self.plan_panel.is_some()
            || self.review_transcript.is_some()
            || self.ask_panel.is_some()
            || self.menu.is_some()
            || self.model_panel.is_some()
    }

    pub(in crate::tui) fn active_transcript(&mut self) -> &mut Transcript {
        self.review_transcript
            .as_mut()
            .unwrap_or(&mut self.conversation)
    }
}
