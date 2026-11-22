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
        #[cfg(debug_assertions)]
        if let Some(worker) = &self.web_search_worker {
            return match worker.try_recv() {
                Ok(event) => Some(Ok(event)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Ok(WorkerEvent::WebSearchCompleted(Err(
                    anyhow::anyhow!("web search diagnostic worker exited without a result"),
                )))),
            };
        }
        None
    }

    /// 安装根会话临时管道 registry；换实例时递增 epoch 并关闭旧面板。
    pub(in crate::tui) fn install_pipe_store(&mut self, store: Option<pp_bash::PipeStore>) -> bool {
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
        changed |= self.receive_settings();
        changed |= self.receive_themes();
        changed |= self.receive_permissions();
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
            let still_skills = self.skills_pending || self.skill_panel.is_some();
            self.skills_pending = false;
            match reply.result {
                Ok(snapshot) if !reply.saved => {
                    if !reply.preview {
                        self.cached_skills = Some(snapshot.clone());
                    }
                    if reply.preview
                        && let Some(panel) = &mut self.skill_panel
                    {
                        panel.apply_preview(snapshot);
                    } else if still_skills
                        && self.settings_panel.is_none()
                        && self.roles_panel.is_none()
                        && self.themes_panel.is_none()
                    {
                        if let Some(panel) = &mut self.skill_panel {
                            panel.apply_snapshot(snapshot, reply.open_sources);
                        } else {
                            let mut panel = skills::Panel::new(snapshot);
                            if reply.open_sources {
                                panel.show_sources();
                            }
                            self.skill_panel = Some(panel);
                        }
                    }
                }
                Ok(snapshot) => {
                    self.cached_skills = Some(snapshot);
                    self.skill_panel = None;
                    self.menu = None;
                    self.menu_dismissed = true;
                }
                Err(error) => {
                    if let Some(panel) = &mut self.skill_panel {
                        panel.busy = false;
                        panel.loading = false;
                        panel.saving = false;
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
        let mut refresh_candidates = changed;
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
            let is_delta = incoming.as_ref().is_ok_and(Self::is_streaming_delta);
            if !is_delta {
                self.flush_stream();
            }
            match incoming {
                Ok(event) => {
                    let event_changed = self.apply_worker_event(event);
                    changed |= event_changed;
                    refresh_candidates |= event_changed && !is_delta;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.fail_worker();
                    return true;
                }
            }
        }
        changed |= self.pipes_tick();
        let queue_changed = self.refresh_queue() | self.dispatch_next(false);
        changed |= queue_changed;
        refresh_candidates |= queue_changed;
        if self.reopen_plan_after_discussion
            && matches!(self.state, State::Ready)
            && !self.has_pending()
        {
            self.reopen_plan_after_discussion = false;
            if self.plan_panel.is_none() && self.has_saved_plan() {
                self.request_plan_read();
                changed = true;
                refresh_candidates = true;
            }
        }
        if changed {
            self.escape_tap.reset();
            if refresh_candidates {
                self.refresh_candidates(false);
            }
        }
        changed
    }

    fn is_streaming_delta(event: &WorkerEvent) -> bool {
        matches!(
            event,
            WorkerEvent::Progress(
                AgentEvent::ThinkingDelta { .. }
                    | AgentEvent::ReasoningSummaryDelta { .. }
                    | AgentEvent::AssistantDelta { .. }
                    | AgentEvent::ModelOutputProgress { .. }
            ) | WorkerEvent::ReviewProgress(
                AgentEvent::ThinkingDelta { .. }
                    | AgentEvent::ReasoningSummaryDelta { .. }
                    | AgentEvent::AssistantDelta { .. }
            )
        )
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
            #[cfg(debug_assertions)]
            WorkerEvent::WebSearchCompleted(_) => self.on_run_event(event),
            WorkerEvent::SessionStarted(_)
            | WorkerEvent::PipeStoreBound { .. }
            | WorkerEvent::ContextInspected(_)
            | WorkerEvent::TreeRead(_)
            | WorkerEvent::TreeNavigated(_)
            | WorkerEvent::SessionDeleted(_)
            | WorkerEvent::SessionResumed(_)
            | WorkerEvent::SubagentsEnsured(_)
            | WorkerEvent::McpSnapshot(_)
            | WorkerEvent::McpManaged { .. }
            | WorkerEvent::SandboxStatus(_)
            | WorkerEvent::Dumped(_)
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
