use crate::tui::{
    AgentEvent, App, CompactionOutcome, Instant, Level, PlanEnterAction, PlanLocation,
    PlanOpenIntent, RunOutcome, State, TryRecvError, WorkMode, WorkerCommand, WorkerEvent, ask,
    editor, format_wait, plan, plan_list, plan_location_from_path, skills,
};

impl App {
    pub(in crate::tui) fn receive(&mut self) -> bool {
        let mut changed = self.poll_history();
        changed |= self.receive_auth();
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
                    self.notifications.clear();
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
        for _ in 0..256 {
            let Some(events) = &self.events else {
                break;
            };
            match events.try_recv() {
                Ok(WorkerEvent::DialogFinished { call_id }) => {
                    if self.ask_id.as_deref() == Some(call_id.as_str()) {
                        self.ask_panel = None;
                        self.ask_reply = None;
                        self.ask_id = None;
                        self.ask_cancellation = None;
                        changed = true;
                    }
                }
                Ok(WorkerEvent::ModelsReloaded(result)) => {
                    self.model_refresh_pending = false;
                    match result {
                        Ok(listing) => {
                            self.choices = listing.choices;
                            self.default_model_reference = Some(listing.default_reference);
                            self.favorite_models = listing.favorites;
                            self.model_usage = listing.usage;
                            self.model_roles = listing.roles;
                            self.default_think_level = listing.default_effort;
                            if let Some(panel) = &mut self.roles_panel {
                                panel.update_choices(self.choices.clone());
                            }
                            self.refresh_candidates(false);
                        }
                        Err(error) => self
                            .notify(Level::Error, format!("Could not refresh models: {error:#}")),
                    }
                    changed = true;
                }
                Ok(WorkerEvent::Ask {
                    request,
                    reply,
                    cancellation,
                }) => {
                    if self
                        .ask_cancellation
                        .as_ref()
                        .is_some_and(|token| token.is_cancelled())
                    {
                        self.ask_panel = None;
                        self.ask_reply = None;
                        self.ask_id = None;
                        self.ask_cancellation = None;
                    }
                    if !cancellation.is_cancelled()
                        && (matches!(self.state, State::Running)
                            && self
                                .active_cancellation
                                .as_ref()
                                .is_some_and(|token| !token.is_cancelled())
                            || request.call_id.starts_with("subagent-")
                            || request.call_id == "approval-dialog")
                        && self.ask_panel.is_none()
                        && (self.conversation.has_running_ask(&request.call_id)
                            || request.call_id.starts_with("subagent-")
                            || request.call_id == "approval-dialog")
                    {
                        self.menu = None;
                        self.menu_dismissed = true;
                        self.queue_menu = false;
                        // 进度视图不能挡住 ask 的键盘路由；先让位给 ask 面板。
                        self.plan_panel = None;
                        self.ask_id = Some(request.call_id.clone());
                        self.ask_cancellation = Some(cancellation);
                        self.ask_panel = Some(ask::Panel::new(request));
                        self.ask_reply = Some(reply);
                        self.conversation.waiting_for_input();
                    }
                    changed = true;
                }
                Ok(WorkerEvent::Progress(event)) => {
                    // 用量快照由 App 缓存，供状态栏渲染；不进入 Transcript。
                    match event {
                        AgentEvent::TodoStateUpdated { state } => self.todo_panel.update(state),
                        AgentEvent::ContextUpdated { usage } => self.usage = Some(usage),
                        AgentEvent::ModelRequestStarted => self.token_speed.start(Instant::now()),
                        AgentEvent::ModelOutputProgress { output_bytes } => {
                            self.token_speed.progress(output_bytes, Instant::now())
                        }
                        AgentEvent::ModelRequestFinished {
                            elapsed_ms,
                            usage,
                            success,
                        } => {
                            self.token_speed.finish(
                                elapsed_ms,
                                usage.map(|usage| usage.output_tokens),
                                success,
                            );
                            if success && let Some(snapshot) = &mut self.usage {
                                snapshot.last_request = usage;
                            }
                        }
                        event @ AgentEvent::CompactionFinished { .. } => {
                            self.token_speed.clear();
                            self.conversation.progress(event);
                        }
                        // 请求重试只显示脱敏原因与等待时长，不进入 Transcript。
                        AgentEvent::RequestRetrying {
                            reason,
                            attempt,
                            wait_ms,
                        } => {
                            self.notify(
                                Level::Important,
                                format!(
                                    "Retrying in {} (attempt {attempt}; {reason})",
                                    format_wait(wait_ms)
                                ),
                            );
                        }
                        // 输入确认由 App 直接回显；不进 Transcript progress，不重复记录历史。
                        AgentEvent::UserInputAccepted { text, .. } => {
                            self.append("You", &text);
                        }
                        // 执行期计划进度：只更新缓存与只读面板，不追加整篇正文到 Transcript。
                        AgentEvent::PlanProgressUpdated {
                            plan_id,
                            revision,
                            content,
                            file_name,
                            location,
                            status,
                        } => self.apply_plan_progress(
                            &plan_id, revision, content, file_name, location, status,
                        ),
                        event => self.conversation.progress(event),
                    }
                    changed = true;
                }
                Ok(WorkerEvent::SessionStarted(id)) => {
                    self.notify(Level::Normal, format!("Session: {id}"));
                    self.session_id = Some(id);
                    changed = true;
                }
                Ok(WorkerEvent::ShellCompleted(result)) => {
                    self.active_cancellation = None;
                    self.secondary_status = None;
                    self.state = State::Ready;
                    if let Err(error) = result {
                        if error
                            .chain()
                            .any(|cause| cause.is::<crate::cancellation::CleanupError>())
                        {
                            self.commands = None;
                            self.events = None;
                            self.state = State::Unavailable;
                            self.unavailable = Some(
                                "Execution unavailable: shell cleanup failed; restart pp.".into(),
                            );
                        }
                        self.notify(Level::Error, format!("{error:#}"));
                    }
                    changed = true;
                }
                Ok(WorkerEvent::Completed {
                    result,
                    elapsed,
                    finished_at,
                }) => {
                    if self.token_speed.is_active() {
                        self.token_speed.clear();
                    }
                    self.ask_reply = None;
                    self.ask_panel = None;
                    self.active_cancellation = None;
                    self.subagent_autowake = false;
                    let succeeded = matches!(&result, Ok(RunOutcome::Completed));
                    if !succeeded {
                        self.queue_paused = self.has_pending();
                    }
                    let awaiting_review = matches!(&result, Ok(RunOutcome::AwaitingPlanReview));
                    let label = match result {
                        Ok(RunOutcome::Completed) => "Completed",
                        Ok(RunOutcome::AwaitingPlanReview) => "Plan ready",
                        Ok(RunOutcome::Cancelled) => {
                            self.conversation.cancel();
                            "Cancelled"
                        }
                        Err(error) => {
                            self.conversation.fail();
                            self.notify(Level::Error, format!("{error:#}"));
                            "Failed"
                        }
                    };
                    self.conversation.finish_run(label, elapsed, finished_at);
                    self.state = State::Ready;
                    if awaiting_review {
                        // 模型提交后自动打开审阅；worker 此时已空闲，安全读取磁盘。
                        self.request_plan_read();
                    }
                    changed = true;
                }
                Ok(WorkerEvent::Compacted(result)) => {
                    // 压缩不是普通任务：不收尾 Transcript、不显示任务 Completed。
                    self.active_cancellation = None;
                    self.secondary_status = None;
                    self.state = State::Ready;
                    match result {
                        Ok(CompactionOutcome::Compacted { .. }) => {}
                        Ok(CompactionOutcome::Unchanged) => {
                            self.notify(Level::Normal, "Nothing to compact.".into());
                        }
                        Ok(CompactionOutcome::Cancelled) => {
                            self.notify(Level::Normal, "Context compaction cancelled.".into());
                        }
                        Err(error) => self.notify(Level::Error, format!("{error:#}")),
                    }
                    changed = true;
                }
                Ok(WorkerEvent::Reset) => {
                    self.todo_panel = Default::default();
                    self.close_ask_on_exit();
                    self.history_browser = false;
                    self.conversation = self.conversation.fresh();
                    self.clear_input();
                    self.notifications.clear();
                    self.show_welcome = true;
                    self.state = State::Ready;
                    self.session_id = None;
                    // /new 清掉计划绑定资格；旧计划文件保留，面板不再指向它。
                    self.plan_panel = None;
                    // 新快照紧随其后；先丢弃旧会话用量，避免短暂显示旧占比。
                    self.usage = None;
                    self.token_speed.clear();
                    changed = true;
                }
                Ok(WorkerEvent::ModelSwitched(result)) => {
                    match result {
                        Ok((choice, level)) => {
                            self.current_model = Some(choice);
                            self.think_level = level;
                            // 容量随模型变化；等待紧随的新快照，不显示旧分母。
                            self.usage = None;
                            self.token_speed.clear();
                        }
                        Err(error) => self.notify(Level::Error, format!("{error:#}")),
                    }
                    self.state = State::Ready;
                    changed = true;
                }
                Ok(WorkerEvent::ThinkLevelSet(result)) => {
                    match result {
                        Ok(level) => {
                            self.think_level = level;
                            self.usage = None;
                            self.token_speed.clear();
                        }
                        Err(error) => self.notify(Level::Error, format!("{error:#}")),
                    }
                    self.state = State::Ready;
                    changed = true;
                }
                Ok(WorkerEvent::ModelState(model, effort)) => {
                    self.current_model = Some(model);
                    self.think_level = effort;
                    changed = true;
                }
                Ok(WorkerEvent::ModelRolesSaved(result)) => {
                    if let Some(panel) = &mut self.roles_panel {
                        panel.busy = false;
                    }
                    match result {
                        Ok((roles, default, effort)) => {
                            self.model_roles = roles;
                            self.default_model_reference = Some(default);
                            self.default_think_level = effort;
                            if let Some(panel) = &mut self.roles_panel {
                                panel.saved();
                            }
                            self.notify(Level::Normal, "Role 配置已保存".into());
                        }
                        Err(error) => {
                            self.notify(Level::Error, format!("Role 保存失败：{error:#}"))
                        }
                    }
                    self.state = State::Ready;
                    changed = true;
                }
                Ok(WorkerEvent::DefaultThinkLevelSaved(result)) => {
                    match result {
                        Ok(level) => {
                            self.default_think_level = level;
                            self.notify(
                                Level::Normal,
                                format!(
                                    "Default thinking effort saved: {} (takes effect next launch)",
                                    level.label()
                                ),
                            );
                        }
                        Err(error) => self.notify(
                            Level::Error,
                            format!("Failed to save default thinking effort: {error:#}"),
                        ),
                    }
                    self.state = State::Ready;
                    changed = true;
                }
                Ok(WorkerEvent::FavoriteModelsSaved(result)) => {
                    match result {
                        Ok(favorites) => self.favorite_models = favorites,
                        Err(error) => self.notify(
                            Level::Error,
                            format!("Failed to save favorite models: {error:#}"),
                        ),
                    }
                    self.state = State::Ready;
                    changed = true;
                }
                Ok(WorkerEvent::DefaultModelSaved(result)) => {
                    match result {
                        Ok(reference) => {
                            self.notify(
                                Level::Normal,
                                format!(
                                    "Default model saved: {reference} (takes effect next launch)"
                                ),
                            );
                            self.default_model_reference = Some(reference);
                        }
                        Err(error) => self.notify(
                            Level::Error,
                            format!("Failed to save default model: {error:#}"),
                        ),
                    }
                    self.state = State::Ready;
                    changed = true;
                }
                Ok(WorkerEvent::ChangesRead(view)) => {
                    self.state = State::Ready;
                    if let Some(panel) = &mut self.changes_panel {
                        panel.set_view(view);
                    }
                    changed = true;
                }
                Ok(WorkerEvent::FileUndone(result)) => {
                    match result {
                        Ok(result) => {
                            self.state = State::Ready;
                            if let Some(panel) = &mut self.changes_panel {
                                panel.set_view(result.view);
                            }
                            if let Some(warning) = result.warning {
                                self.notify(Level::Error, warning);
                            }
                        }
                        Err(error) => {
                            // The worker appends an authoritative ChangesRead in FIFO order.
                            // Stay busy until that snapshot arrives; set_view keeps this error.
                            if let Some(panel) = &mut self.changes_panel {
                                panel.set_error(format!("{error:#}"));
                            }
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::TreeRead(result)) => {
                    self.state = State::Ready;
                    match result {
                        Ok(view) if view.tree.nodes.is_empty() => {
                            self.tree_panel = None;
                            self.notify(Level::Normal, "No entries found in session tree.".into());
                        }
                        Ok(view) => {
                            if let Some(panel) = &mut self.tree_panel {
                                panel.set_tree(view);
                            }
                        }
                        Err(error) => {
                            self.tree_panel = None;
                            self.notify(
                                Level::Error,
                                format!("Failed to read session tree: {error:#}"),
                            );
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::TreeNavigated(result)) => {
                    self.state = State::Ready;
                    match result {
                        Ok(navigation) => {
                            let draft = std::mem::replace(&mut self.input, editor());
                            let empty = draft.lines().iter().all(|line| line.trim().is_empty());
                            let error = self.adopt_history(navigation.replay);
                            self.input = draft;
                            if empty && let Some(text) = navigation.draft {
                                self.input.insert_str(text);
                            }
                            self.tree_panel = None;
                            self.usage = None;
                            self.token_speed.clear();
                            if let Some(error) = error {
                                self.notify(Level::Error, error);
                            }
                        }
                        Err(error) => {
                            if let Some(panel) = &mut self.tree_panel {
                                panel.pending = false;
                            }
                            self.notify(
                                Level::Error,
                                format!("Failed to navigate session tree: {error:#}"),
                            );
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::SessionDeleted(result)) => {
                    // 删除仍走 Agent worker 的会话锁与 JSONL 删除；成功后再同步派生索引。
                    self.state = State::Ready;
                    let mut refresh = None;
                    if let Some(panel) = &mut self.session_panel {
                        panel.delete_finished(result.is_ok());
                        if result.is_ok() {
                            let (request_id, cwd, query) = panel.refresh_request();
                            refresh = Some((panel.generation(), request_id, cwd, query));
                        }
                    }
                    match result {
                        Ok(()) => {
                            // 先报告已删除，再展示索引刷新错误；刷新失败清空旧行。
                            self.notify(Level::Normal, "Session deleted.".into());
                            if let Some((generation, request_id, cwd, query)) = refresh
                                && !self.history_sync_and_search(generation, request_id, cwd, query)
                                && let Some(panel) = &mut self.session_panel
                            {
                                panel.fail(
                                    request_id,
                                    "Session was deleted, but the session list could not be refreshed; reopen /resume to retry.".into(),
                                );
                            }
                        }
                        Err(error) => self
                            .notify(Level::Error, format!("Failed to delete session: {error:#}")),
                    }
                    changed = true;
                }
                Ok(WorkerEvent::SessionResumed(result)) => {
                    self.state = State::Ready;
                    match result {
                        Ok(state) => {
                            self.session_id = Some(state.id.clone());
                            self.session_panel = None;
                            // 历史/清理句柄已由 ExecutionPrepared 接管；这里只补通知。
                            self.usage = None;
                            self.token_speed.clear();
                            match self.resume_note.take() {
                                Some(error) => self.notify(
                                    Level::Error,
                                    format!("Resumed session: {}\n{error}", state.id),
                                ),
                                None => self.notify(
                                    Level::Normal,
                                    format!("Resumed session: {}", state.id),
                                ),
                            }
                        }
                        Err(error) => {
                            // 恢复失败：保留列表、选中项与旧对话，可重新选择或取消。
                            if let Some(panel) = &mut self.session_panel {
                                panel.restore_failed();
                            }
                            self.notify(
                                Level::Error,
                                format!("Failed to resume session: {error:#}"),
                            );
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::PlansListed(result)) => {
                    self.state = State::Ready;
                    if let Some(panel) = &mut self.plan_list_panel {
                        match result {
                            Ok(listing) => panel.set_listing(listing),
                            Err(error) => panel.set_error(format!("{error:#}")),
                        }
                    }
                    // 面板在加载中关闭：迟到的列表回复直接丢弃。
                    changed = true;
                }
                Ok(WorkerEvent::ListedPlanRead(result)) => {
                    self.state = State::Ready;
                    let pending = self.plan_list_pending.take();
                    match (result, pending) {
                        (Ok(selection), Some(plan_list::PendingOp::Preview))
                            if self.plan_list_panel.is_some() =>
                        {
                            // 复位面板阶段：预览关闭后列表必须继续接收按键。
                            if let Some(panel) = &mut self.plan_list_panel {
                                panel.set_phase(plan_list::Phase::Ready);
                            }
                            if plan_location_from_path(&selection.path) == Some(PlanLocation::Done)
                            {
                                self.open_plan_list_preview(&selection);
                            } else {
                                self.open_listed_plan_selection(selection, PlanOpenIntent::Execute);
                            }
                        }
                        (Ok(selection), Some(plan_list::PendingOp::Open(intent)))
                            if self.plan_list_panel.is_some() =>
                        {
                            self.open_listed_plan_selection(selection, intent);
                        }
                        (Ok(selection), Some(plan_list::PendingOp::Rename(file_name)))
                            if self.plan_list_panel.is_some() =>
                        {
                            if self.send(
                                WorkerCommand::RenameListedPlan {
                                    selection,
                                    file_name,
                                },
                                State::RenamingPlan,
                            ) && let Some(panel) = &mut self.plan_list_panel
                            {
                                panel.set_phase(plan_list::Phase::Renaming);
                            }
                        }
                        (Ok(selection), Some(plan_list::PendingOp::Delete))
                            if self.plan_list_panel.is_some() =>
                        {
                            if self.send(
                                WorkerCommand::DeleteListedPlan { selection },
                                State::DeletingPlan,
                            ) && let Some(panel) = &mut self.plan_list_panel
                            {
                                panel.set_phase(plan_list::Phase::Deleting);
                            }
                        }
                        // 面板在读取中关闭：丢弃迟到的 selection，不继续任何操作。
                        (Ok(_), _) => {}
                        (Err(error), _) => {
                            if let Some(panel) = &mut self.plan_list_panel {
                                panel.set_error(format!("{error:#}"));
                            }
                            self.notify(Level::Error, format!("{error:#}"));
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::ListedPlanOpened(result)) => {
                    self.state = State::Ready;
                    let execute_review = matches!(
                        self.plan_list_pending.take(),
                        Some(plan_list::PendingOp::Open(PlanOpenIntent::Execute))
                    );
                    match result {
                        Ok(preparation) => {
                            // 与 ExecutionPrepared 相同的安装顺序：先清理句柄与
                            // 会话状态，再展示；不自动请求模型。
                            let path = preparation
                                .workflow
                                .plan
                                .as_ref()
                                .and_then(|plan| plan.path())
                                .unwrap_or_else(|| "Not saved yet".to_owned());
                            self.install_mcp_cleanup(preparation.cleanup);
                            self.plan_panel = None;
                            self.set_workflow(preparation.workflow);
                            let mut note = None;
                            if let Some(records) = preparation.replay {
                                note = self.adopt_history(records);
                            }
                            // 清单已由 workflow 安装到固定区；旧显示事件只消费，
                            // 不在新会话的对话流中重复生成 Preview 卡片。
                            if let Some(snapshot) = preparation.todo_snapshot {
                                self.conversation.progress(snapshot);
                            }
                            self.session_id = preparation.session_id;
                            self.usage = None;
                            self.token_speed.clear();
                            if execute_review {
                                // 执行前审阅：重新安全读取并签发 token；
                                // 列表保持打开，以便关闭菜单时返回列表。
                                // 先复位面板阶段，否则 Opening 会吞掉后续按键。
                                if let Some(panel) = &mut self.plan_list_panel {
                                    panel.set_phase(plan_list::Phase::Ready);
                                }
                                self.request_plan_read();
                            } else {
                                // 继续规划：关闭列表，提示继续输入。
                                self.plan_list_panel = None;
                                self.plan_list_pending = None;
                                if let Some(error) = note {
                                    self.notify(Level::Error, error);
                                }
                                self.notify(
                                    Level::Normal,
                                    format!("Opened plan draft {path}; describe the next planning input."),
                                );
                            }
                        }
                        Err(error) => {
                            if let Some(panel) = &mut self.plan_list_panel {
                                panel.set_error(format!("{error:#}"));
                            }
                            self.notify(Level::Error, format!("{error:#}"));
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::ListedPlanRenamed(result)) => {
                    self.state = State::Ready;
                    if let Some(panel) = &mut self.plan_list_panel {
                        match result {
                            Ok(listing) => panel.set_listing(listing),
                            Err(error) => panel.set_error(format!("{error:#}")),
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::ListedPlanDeleted(result)) => {
                    self.state = State::Ready;
                    if let Some(panel) = &mut self.plan_list_panel {
                        match result {
                            Ok(listing) => panel.set_listing(listing),
                            Err(error) => panel.set_error(format!("{error:#}")),
                        }
                    }
                    // 随后的 Workflow 快照统一同步绑定、进度与 TODO。
                    changed = true;
                }
                Ok(WorkerEvent::WorkflowChanged(result)) => {
                    let from_approval = matches!(self.state, State::PreparingExecution);
                    let from_enter = matches!(self.state, State::ChangingPlan);
                    if from_enter || from_approval {
                        let _ = self.input_queue.close();
                        self.active_cancellation = None;
                        self.secondary_status = None;
                    }
                    let after_enter = self.plan_after_enter.take();
                    // 进入转换会携带 after_enter；退出转换保持 None，
                    // 据此区分进入失败（重试进入）与退出失败（保持退出方向）。
                    let was_enter = after_enter.is_some();
                    self.state = State::Ready;
                    match result {
                        Ok(workflow) => {
                            self.plan_retry_enter = false;
                            self.set_workflow(workflow);
                            // `n` 在 Plan 模式下解除绑定后的重进：离开成功立即
                            // 重新进入 Plan 待命，不发送模型请求。
                            let reentered = self.plan_reenter_standby
                                && self.workflow.mode == WorkMode::Execute;
                            if reentered {
                                self.plan_reenter_standby = false;
                                self.plan_after_enter = Some(PlanEnterAction::Standby);
                                self.start_plan_transition(
                                    |cancellation| WorkerCommand::EnterPlan { cancellation },
                                    State::ChangingPlan,
                                );
                            }
                            if !reentered {
                                match after_enter {
                                    // 进入成功只提示待命；首条用户任务才会建立绑定。
                                    Some(PlanEnterAction::Standby) => self.notify(
                                        Level::Normal,
                                        "Plan mode is active. Describe the task to research and plan."
                                            .into(),
                                    ),
                                    Some(PlanEnterAction::Prompt(prompt)) => {
                                        self.queue_paused = false;
                                        if self.send_task(Some(prompt.clone())) {
                                            self.append("You", &prompt);
                                        }
                                    }
                                    None => {}
                                }
                            }
                        }
                        Err(error) => {
                            self.plan_reenter_standby = false;
                            self.notify(Level::Error, format!("{error:#}"));
                            if let Some(PlanEnterAction::Prompt(prompt)) = after_enter
                                && self.input.is_empty()
                            {
                                self.input.insert_str(format!("/plan {prompt}"));
                                self.refresh_candidates(true);
                            }
                            // 失败也可能已持久化 Plan（随后的 Workflow 快照纠正显示）；
                            // 仅进入失败时下一次 /plan 优先重试关闭 MCP；退出失败
                            // 保持离开方向，不把重试误导成再次进入。
                            if from_enter && was_enter {
                                self.plan_retry_enter = true;
                            }
                            if from_approval || (from_enter && self.plan_panel.is_some()) {
                                // 审批或保存退出失败均安全刷新正文与新 token。
                                self.request_plan_read();
                            }
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::PlanRead(result)) => {
                    self.state = State::Ready;
                    match result {
                        Ok(review) => {
                            let reviewed = self.plan_review_pending.take().is_some();
                            self.open_plan_panel(review);
                            // 审查完成后的刷新：焦点落在正文 Review 标签；
                            // 外部修订则面板标注旧版报告。绝不自动启动执行。
                            if reviewed && let Some(panel) = &mut self.plan_panel {
                                panel.focus_review_body();
                            }
                        }
                        Err(error) => {
                            if let Some(report) = self.plan_review_pending.take() {
                                // 报告已生成但安全读取失败：展示最终报告与错误；
                                // 无 token 的只读面板禁用旧执行 token，直到再次安全读取成功。
                                if let Some(document) = self.workflow.plan.clone() {
                                    self.plan_panel = Some(plan::Panel::degraded(document, report));
                                }
                            }
                            self.notify(Level::Error, format!("{error:#}"));
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::ReviewProgress(event)) => {
                    // 审查进度只进独立 review transcript：不更新用量/主对话/派发。
                    if let Some(review) = &mut self.review_transcript {
                        review.progress(event);
                    }
                    changed = true;
                }
                Ok(WorkerEvent::PlanReviewed(result)) => {
                    // 审查终态：无论成败都保持 Plan/Ready 并回六项菜单；
                    // 报告只在成功时替换，失败/取消保留上次完整报告。
                    self.state = State::Ready;
                    self.active_cancellation = None;
                    self.secondary_status = None;
                    self.review_transcript = None;
                    let _ = self.input_queue.close();
                    match result {
                        Ok(report) => {
                            // 报告先入 UI 快照，随后 ReadPlan 安全刷新正文/token/报告绑定。
                            self.workflow.review = Some(report.clone());
                            self.plan_review_pending = Some(report);
                            self.request_plan_read();
                        }
                        Err(error) => {
                            self.notify(Level::Error, format!("{error:#}"));
                            self.request_plan_read();
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::Workflow(workflow)) => {
                    self.set_workflow(workflow);
                    changed = true;
                }
                Ok(WorkerEvent::McpSnapshot(view)) => {
                    self.mcp_view = view;
                    changed = true;
                }
                Ok(WorkerEvent::McpManaged { result, view, show }) => {
                    self.mcp_view = view;
                    self.active_cancellation = None;
                    self.secondary_status = None;
                    self.state = State::Ready;
                    match result {
                        Ok(()) => {
                            if show {
                                self.mcp_panel = Some(crate::tui::mcp::Panel::list(&self.mcp_view));
                                self.menu = None;
                            } else {
                                self.notify(
                                    Level::Normal,
                                    "MCP 状态已更新；可用 /mcp list 查看".into(),
                                );
                            }
                        }
                        Err(error) => {
                            if error.is::<crate::cancellation::CleanupError>() {
                                self.state = State::Unavailable;
                            }
                            self.notify(Level::Error, format!("{error:#}"));
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::Cleanup(cleanup)) => {
                    // 错误路径上 registry 可能被保留/部分更新；刷新退出清理句柄。
                    self.install_mcp_cleanup(cleanup);
                    changed = true;
                }
                Ok(WorkerEvent::ExecutionPrepared {
                    workflow,
                    session_id,
                    replay,
                    cleanup,
                    todo_snapshot,
                    reply,
                }) => {
                    // 一次性交接：先安装退出清理句柄并更新模式/会话，再 ACK；
                    // ACK 后 worker 才允许 continue_session。
                    self.install_mcp_cleanup(cleanup);
                    self.plan_panel = None;
                    self.set_workflow(workflow);
                    let mut note = None;
                    if let Some(records) = replay {
                        // None：Keep/off 不重绘；Some(空)：Fresh 新会话；Some(records)：恢复。
                        note = self.adopt_history(records);
                    }
                    // 清单在替换 workflow 时已安装到固定区；旧显示快照不再
                    // 生成对话卡片，ACK 前仍完成事件消费以保留交接顺序。
                    if let Some(snapshot) = todo_snapshot {
                        self.conversation.progress(snapshot);
                    }
                    self.session_id = session_id;
                    self.usage = None;
                    self.token_speed.clear();
                    let confirmed = reply.send(()).is_ok();
                    match self.state {
                        State::PreparingExecution if confirmed => {
                            self.secondary_status = None;
                            self.state = State::Running;
                            self.conversation.begin_run();
                            self.append_execution_echo();
                            // 列表审批进入执行：关闭列表与待处理操作。
                            self.plan_list_panel = None;
                            self.plan_list_pending = None;
                        }
                        State::Resuming => {
                            self.resume_note = note;
                            self.state = State::Ready;
                        }
                        State::ChangingPlan => {
                            let _ = self.input_queue.close();
                            self.active_cancellation = None;
                            self.secondary_status = None;
                            if self.workflow.mode == WorkMode::Execute {
                                self.plan_list_panel = None;
                                self.plan_list_pending = None;
                            }
                            self.state = State::Ready;
                        }
                        _ => {
                            self.state = State::Ready;
                        }
                    }
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.fail_worker();
                    return true;
                }
            }
        }
        changed |= self.refresh_queue();
        changed |= self.dispatch_next(false);
        if changed {
            self.escape_tap.reset();
            self.refresh_candidates(false);
        }
        changed
    }
}
