mod ask;
mod model;
mod panels;
mod plan;
mod progress;
mod run;

use crate::tui::{
    AgentEvent, App, Instant, Level, Receiver, State, TryRecvError, WorkerEvent, skills,
};

impl App {
    pub(in crate::tui) fn flush_stream(&mut self) {
        if let Some(event) = self.review_stream.take()
            && let Some(review) = &mut self.review_transcript
        {
            review.progress(event);
        }
        if let Some(event) = self.stream.take() {
            self.conversation.progress(event);
        }
    }

    /// 正常 worker 优先；mock 通道独立存在，正常通道缺失时仍可推进。
    fn next_worker_event(&mut self) -> Option<std::result::Result<WorkerEvent, TryRecvError>> {
        match self.events.as_ref().map(Receiver::try_recv) {
            Some(Ok(event)) => return Some(Ok(event)),
            Some(Err(TryRecvError::Disconnected)) => {
                return Some(Err(TryRecvError::Disconnected));
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
        #[cfg(debug_assertions)]
        if let Some(worker) = &self.mock_worker {
            let elapsed = worker.elapsed();
            return match worker.try_recv() {
                Ok(event) => Some(Ok(event)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Ok(WorkerEvent::MockCompleted {
                    result: Err(anyhow::anyhow!(
                        "mock test worker exited without a completion result"
                    )),
                    elapsed,
                    finished_at: chrono::Local::now(),
                })),
            };
        }
        None
    }

    /// 安装根会话临时管道 registry；换实例时递增 epoch 并关闭旧面板。
    pub(in crate::tui) fn install_pipe_store(
        &mut self,
        store: Option<pp_runtime::tools::PipeStore>,
    ) -> bool {
        let changed = match (&self.pipe_store, &store) {
            (Some(current), Some(next)) => !current.same_instance(next),
            (None, None) => false,
            _ => true,
        };
        self.pipe_store = store;
        if changed {
            self.pipe_store_epoch = self.pipe_store_epoch.wrapping_add(1);
            // 旧面板只持有旧 store 的索引；关闭观察线程并释放正文。
            self.close_pipes();
        }
        changed
    }

    pub(in crate::tui) fn receive(&mut self) -> bool {
        let mut changed = self.receive_resources();
        let available = self.resource_input_available() && self.menu.is_none();
        changed |= self.image_view.poll(Instant::now(), available);
        let task_status = self.task_status_view.snapshot();
        if self.task_status != task_status {
            self.task_status = task_status;
            changed = true;
        }
        changed |= self.poll_history();
        changed |= self.poll_model_control();
        changed |= self.receive_auth();
        while let Some(reply) = self
            .stats_worker
            .as_ref()
            .and_then(|worker| worker.replies.try_recv().ok())
        {
            match reply {
                Ok(message) => self.append("Stats", &message),
                Err(error) => self.notify(Level::Error, format!("{error:#}")),
            }
            changed = true;
        }
        while let Ok(reply) = self.skill_worker.replies.try_recv() {
            self.skills_pending = false;
            match reply.result {
                Ok(snapshot) if !reply.saved => {
                    if reply.preview
                        && let Some(panel) = &mut self.skill_panel
                    {
                        panel.apply_preview(snapshot);
                    } else {
                        self.skill_panel = Some(skills::Panel::new(snapshot));
                        if reply.open_sources
                            && let Some(panel) = &mut self.skill_panel
                        {
                            panel.show_sources();
                        }
                    }
                }
                Ok(_) => {
                    self.skill_panel = None;
                    self.menu = None;
                    self.menu_dismissed = true;
                }
                Err(error) => {
                    if let Some(panel) = &mut self.skill_panel {
                        panel.busy = false;
                    }
                    self.notify(Level::Error, format!("{error:#}"));
                }
            }
            changed = true;
        }
        if self.files_pending {
            match self.file_worker.replies.try_recv() {
                Ok(result) => {
                    self.files_pending = false;
                    self.file_paths.clear();
                    match result {
                        Ok(paths) => self.file_paths = paths,
                        Err(error) if self.file_query.is_some() => self.notify(
                            Level::Error,
                            format!("Failed to list project files: {error:#}"),
                        ),
                        Err(_) => {}
                    }
                    // File actions index this snapshot, never the previous list.
                    if self.file_query.is_some() {
                        self.menu = None;
                    }
                    changed = true;
                }
                Err(TryRecvError::Disconnected) => {
                    self.files_pending = false;
                    self.file_paths.clear();
                    self.notify(Level::Error, "File picker unavailable; restart pp.".into());
                    changed = true;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.subagent_autowake
            && matches!(self.state, State::Ready)
            && !self.has_pending()
            && self.ask_panel.is_none()
        {
            let ready = self
                .subagent_control
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|b| !b.deliveries(&[]).is_empty());
            if ready {
                self.subagent_autowake = false;
                self.send_task(None);
                changed = true;
            }
        }
        let receive_started = Instant::now();
        for _ in 0..64 {
            if receive_started.elapsed() >= std::time::Duration::from_millis(2) {
                break;
            }
            // 正常 worker 与隔离 mock worker 共用同一事件预算；任何一侧不存在
            // 都不能阻止另一侧被处理。
            let Some(incoming) = self.next_worker_event() else {
                break;
            };
            if !matches!(
                &incoming,
                Ok(WorkerEvent::Progress(
                    AgentEvent::ThinkingDelta { .. }
                        | AgentEvent::ReasoningSummaryDelta { .. }
                        | AgentEvent::AssistantDelta { .. }
                        | AgentEvent::ModelOutputProgress { .. }
                )) | Ok(WorkerEvent::ReviewProgress(
                    AgentEvent::ThinkingDelta { .. }
                        | AgentEvent::ReasoningSummaryDelta { .. }
                        | AgentEvent::AssistantDelta { .. }
                )) | Err(TryRecvError::Empty)
            ) {
                self.flush_stream();
            }
            match incoming {
                Ok(event) => changed |= self.apply_worker_event(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.fail_worker();
                    return true;
                }
            }
        }
        changed |= self.pipes_tick();
        changed |= self.refresh_queue();
        changed |= self.dispatch_next(false);
        if self.reopen_plan_after_discussion
            && matches!(self.state, State::Ready)
            && !self.has_pending()
        {
            self.reopen_plan_after_discussion = false;
            if self.plan_panel.is_none() && self.has_saved_plan() {
                self.request_plan_read();
                changed = true;
            }
        }
        if changed {
            self.escape_tap.reset();
            self.refresh_candidates(false);
        }
        changed
    }

    /// 按事件族把 worker 事件分派给各自的处理入口；返回是否有可见变化。
    fn apply_worker_event(&mut self, event: WorkerEvent) -> bool {
        match event {
            WorkerEvent::DialogFinished { .. } | WorkerEvent::Ask { .. } => {
                self.on_ask_event(event)
            }
            WorkerEvent::Progress(event) => self.on_agent_event(event),
            WorkerEvent::ReviewProgress(event) => self.on_review_progress(event),
            WorkerEvent::ModelsReloaded(_)
            | WorkerEvent::ModelSwitched(_)
            | WorkerEvent::ModelState { .. }
            | WorkerEvent::ModelRolesSaved(_)
            | WorkerEvent::DefaultThinkLevelSaved(_)
            | WorkerEvent::DefaultModelSaved(_)
            | WorkerEvent::FavoriteModelsSaved(_) => self.on_model_event(event),
            WorkerEvent::ShellCompleted(_)
            | WorkerEvent::Completed { .. }
            | WorkerEvent::Compacted(_)
            | WorkerEvent::Reset => self.on_run_event(event),
            #[cfg(debug_assertions)]
            WorkerEvent::MockPrepared(_) => self.on_run_event(event),
            #[cfg(debug_assertions)]
            WorkerEvent::MockCompleted { .. } => self.on_run_event(event),
            WorkerEvent::SessionStarted(_)
            | WorkerEvent::PipeStoreBound { .. }
            | WorkerEvent::ChangesRead(_)
            | WorkerEvent::FileUndone(_)
            | WorkerEvent::TreeRead(_)
            | WorkerEvent::TreeNavigated(_)
            | WorkerEvent::SessionDeleted(_)
            | WorkerEvent::SessionResumed(_)
            | WorkerEvent::SubagentsEnsured(_)
            | WorkerEvent::McpSnapshot(_)
            | WorkerEvent::McpManaged { .. }
            | WorkerEvent::LspWarmupFailed { .. }
            | WorkerEvent::LspPollFailed(_) => self.on_panel_event(event),
            WorkerEvent::PlansListed(_)
            | WorkerEvent::ListedPlanRead(_)
            | WorkerEvent::ListedPlanOpened(_)
            | WorkerEvent::ListedPlanRenamed(_)
            | WorkerEvent::ListedPlanDeleted(_)
            | WorkerEvent::WorkflowChanged(_)
            | WorkerEvent::PlanRead(_)
            | WorkerEvent::PlanReviewed(_)
            | WorkerEvent::Workflow(_)
            | WorkerEvent::ExecutionPrepared { .. } => self.on_plan_event(event),
        }
    }
}
