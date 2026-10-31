use crate::tui::{
    App, CancellationToken, Level, PlanEnterAction, PlanLocation, PlanOpenIntent, PlanSelection,
    PlanStatus, State, WorkMode, WorkerCommand, changes, plan, plan_list, plan_location_from_path,
    tree,
};

impl App {
    // 有待处理消息时禁止改动会话历史；/tree 与 submit 的 New/Resume/Tree 入口共用。
    pub(in crate::tui) fn pending_blocks_session_change(&mut self) -> bool {
        if !self.has_pending() {
            return false;
        }
        self.notify(
            Level::Error,
            "Recall pending messages before changing session history.".into(),
        );
        true
    }

    pub(in crate::tui) fn open_tree(&mut self) {
        self.escape_tap.reset();
        if self.pending_blocks_session_change() {
            return;
        }
        if !matches!(self.state, State::Ready)
            || self.skills_pending
            || self.skill_panel.is_some()
            || self.session_panel.is_some()
            || self.ask_panel.is_some()
            || self.tree_panel.is_some()
            || self.changes_panel.is_some()
        {
            return;
        }
        self.menu = None;
        self.file_query = None;
        self.tree_panel = Some(tree::Panel::new());
        self.send(WorkerCommand::ReadTree, State::ReadingTree);
    }

    pub(in crate::tui) fn open_changes(&mut self) {
        self.escape_tap.reset();
        if !matches!(self.state, State::Ready)
            || self.skills_pending
            || self.skill_panel.is_some()
            || self.session_panel.is_some()
            || self.ask_panel.is_some()
            || self.tree_panel.is_some()
            || self.changes_panel.is_some()
            || self.plan_panel.is_some()
            || self.model_panel.is_some()
        {
            return;
        }
        self.close_pending_menu();
        self.menu = None;
        self.file_query = None;
        self.changes_panel = Some(changes::Panel::new(self.workflow.mode == WorkMode::Plan));
        self.send(WorkerCommand::ReadChanges, State::ReadingChanges);
    }

    pub(in crate::tui) fn apply_changes_action(&mut self, action: changes::Action) {
        match action {
            changes::Action::None => {}
            changes::Action::Close => {
                self.changes_panel = None;
                self.menu_dismissed = true;
            }
            changes::Action::Refresh if matches!(self.state, State::Ready) => {
                self.send(WorkerCommand::ReadChanges, State::ReadingChanges);
            }
            changes::Action::Undo(token) if matches!(self.state, State::Ready) => {
                self.send(WorkerCommand::UndoFile(token), State::UndoingFile);
            }
            changes::Action::Message(message) => {
                if let Some(panel) = &mut self.changes_panel {
                    panel.set_error(message);
                }
            }
            changes::Action::Refresh | changes::Action::Undo(_) => {}
        }
    }

    /// /plan-list 打开前提：Ready、无 pending、无其它 modal；被拒时保留输入。
    pub(in crate::tui) fn plan_list_entry_ready(&mut self) -> bool {
        if !matches!(self.state, State::Ready) {
            self.notify(
                Level::Error,
                "Cancel the current task before changing Plan mode.".into(),
            );
            return false;
        }
        if self.has_pending() {
            self.notify(
                Level::Error,
                "Apply pending input before changing Plan mode.".into(),
            );
            return false;
        }
        if self.ask_panel.is_some()
            || self.skill_panel.is_some()
            || self.skills_pending
            || self.session_panel.is_some()
            || self.tree_panel.is_some()
            || self.changes_panel.is_some()
            || self.plan_panel.is_some()
            || self.model_panel.is_some()
            || self.queue_menu
        {
            self.notify(
                Level::Error,
                "Close the open panel before changing Plan mode.".into(),
            );
            return false;
        }
        true
    }

    pub(in crate::tui) fn open_plan_list(&mut self) {
        self.escape_tap.reset();
        self.file_query = None;
        self.menu = None;
        self.menu_dismissed = true;
        self.plan_list_pending = None;
        self.plan_list_panel = Some(plan_list::Panel::new());
        if !self.send(WorkerCommand::ListPlans, State::ListingPlans) {
            self.plan_list_panel = None;
        }
    }

    pub(in crate::tui) fn close_plan_list(&mut self) {
        self.plan_list_panel = None;
        self.plan_list_pending = None;
        self.menu_dismissed = true;
    }

    pub(in crate::tui) fn apply_plan_list_action(&mut self, action: plan_list::Action) {
        match action {
            plan_list::Action::None => {}
            plan_list::Action::Close => self.close_plan_list(),
            plan_list::Action::Refresh => {
                if matches!(self.state, State::Ready)
                    && let Some(panel) = &mut self.plan_list_panel
                {
                    panel.set_loading();
                    self.send(WorkerCommand::ListPlans, State::ListingPlans);
                }
            }
            plan_list::Action::Preview(id) => {
                if !matches!(self.state, State::Ready) {
                    return;
                }
                self.plan_list_pending = Some(plan_list::PendingOp::Preview);
                if !self.send(
                    WorkerCommand::ReadListedPlan { id },
                    State::ReadingListedPlan,
                ) {
                    self.plan_list_pending = None;
                } else if let Some(panel) = &mut self.plan_list_panel {
                    panel.set_reading();
                }
            }
            plan_list::Action::Open(id, intent) => {
                if !matches!(self.state, State::Ready) {
                    return;
                }
                self.plan_list_pending = Some(plan_list::PendingOp::Open(intent));
                if !self.send(
                    WorkerCommand::ReadListedPlan { id },
                    State::ReadingListedPlan,
                ) {
                    self.plan_list_pending = None;
                } else if let Some(panel) = &mut self.plan_list_panel {
                    panel.set_reading();
                }
            }
            plan_list::Action::Rename(id, file_name) => {
                if !matches!(self.state, State::Ready) {
                    return;
                }
                self.plan_list_pending = Some(plan_list::PendingOp::Rename(file_name));
                if !self.send(
                    WorkerCommand::ReadListedPlan { id },
                    State::ReadingListedPlan,
                ) {
                    self.plan_list_pending = None;
                } else if let Some(panel) = &mut self.plan_list_panel {
                    panel.set_reading();
                }
            }
            plan_list::Action::Delete(id) => {
                if !matches!(self.state, State::Ready) {
                    return;
                }
                self.plan_list_pending = Some(plan_list::PendingOp::Delete);
                if !self.send(
                    WorkerCommand::ReadListedPlan { id },
                    State::ReadingListedPlan,
                ) {
                    self.plan_list_pending = None;
                } else if let Some(panel) = &mut self.plan_list_panel {
                    panel.set_reading();
                }
            }
            plan_list::Action::NewPlan => self.plan_list_new_plan(),
            plan_list::Action::Message(message) => {
                if let Some(panel) = &mut self.plan_list_panel {
                    panel.set_error(message);
                }
            }
        }
    }

    pub(in crate::tui) fn open_listed_plan_selection(
        &mut self,
        selection: PlanSelection,
        intent: PlanOpenIntent,
    ) {
        let cancellation = CancellationToken::new();
        if self.send(
            WorkerCommand::OpenListedPlan {
                selection,
                intent,
                cancellation: cancellation.clone(),
            },
            State::OpeningListedPlan,
        ) {
            self.plan_list_pending = Some(plan_list::PendingOp::Open(intent));
            self.active_cancellation = Some(cancellation);
            if let Some(panel) = &mut self.plan_list_panel {
                panel.set_opening();
            }
        }
    }

    /// `n` 新建规划：保留已保存草稿，关闭列表并进入 Plan+None 待命；
    /// 不分配 ID、不写正文、不调模型；未落盘草稿复用裸 /plan 放弃确认。
    pub(in crate::tui) fn plan_list_new_plan(&mut self) {
        if self.workflow.mode == WorkMode::Plan
            && let Some(plan) = &self.workflow.plan
            && plan.revision == 0
            && !plan.origin_prompt.trim().is_empty()
        {
            self.request_plan_leave(true);
            return;
        }
        self.close_plan_list();
        if self.workflow.mode == WorkMode::Execute || self.plan_retry_enter {
            self.plan_after_enter = Some(PlanEnterAction::Standby);
            self.start_plan_transition(
                |cancellation| WorkerCommand::EnterPlan { cancellation },
                State::ChangingPlan,
            );
            return;
        }
        // 已在 Plan 模式：先解除当前绑定（LeavePlan→Execute+None），
        // 成功后由 WorkflowChanged 立即重新进入 Plan 待命。
        self.request_plan_leave(true);
    }

    /// Done 只读预览：以 selection 构造展示用文档，不发审批 token。
    pub(in crate::tui) fn open_plan_list_preview(&mut self, selection: &PlanSelection) {
        let location = plan_location_from_path(&selection.path).unwrap_or(PlanLocation::Drafts);
        let file_name = selection
            .path
            .rsplit('/')
            .next()
            .unwrap_or("plan.md")
            .to_owned();
        let status = match location {
            PlanLocation::Drafts => PlanStatus::Draft,
            PlanLocation::Plans => PlanStatus::Ready,
            PlanLocation::Done => PlanStatus::Completed,
        };
        let document = crate::plan::PlanDocument {
            id: selection.id.clone(),
            revision: 1,
            title: file_name.trim_end_matches(".md").to_owned(),
            content: selection.content.clone(),
            status,
            origin_prompt: String::new(),
            file_name: Some(file_name),
            location,
            approved_tasks: None,
        };
        self.plan_progress = crate::plan::task_progress(&document.content);
        match &mut self.plan_panel {
            Some(panel) if panel.document().id == document.id => panel.set_progress(&document),
            _ => self.plan_panel = Some(plan::Panel::progress(document)),
        }
        self.menu = None;
        self.menu_dismissed = true;
        self.file_query = None;
    }
}
