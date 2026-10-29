use crate::tui::{
    Agent, App, Context, EscapeTap, InputQueue, Instant, Level, NotificationOptions, Notifications,
    Path, Result, ResumeState, State, ThinkLevel, Transcript, WorkerCommand, WorkerEvent,
    WorkflowState, commands, editor, files, merge_startup_notice, mpsc, publish_prepared,
    publish_transition_failure, push_runtime_snapshot, push_workflow_snapshot, request_ask, skills,
    statusline, thread, usage_snapshot,
};

impl App {
    #[cfg(test)]
    pub(in crate::tui) fn new(agent: Result<Agent>) -> Result<Self> {
        let skills = agent
            .as_ref()
            .map(|agent| agent.skills())
            .unwrap_or_else(|_| crate::skills::Skills::default().shared());
        Self::new_with_skills(
            agent,
            skills,
            commands::FileCommands::default(),
            Path::new("."),
            NotificationOptions::default(),
        )
    }

    pub(in crate::tui) fn new_with_skills(
        agent: Result<Agent>,
        skills: crate::skills::SharedSkills,
        file_commands: commands::FileCommands,
        cwd: &Path,
        notification_options: NotificationOptions,
    ) -> Result<Self> {
        let diagnostics = skills
            .lock()
            .map_err(|_| anyhow::anyhow!("skill registry unavailable"))?
            .diagnostics
            .clone();
        let mut app = Self {
            auth_worker: super::super::auth::Worker::new()?,
            auth_panel: None,
            auth_cancellation: None,
            auth_pending: false,
            bootstrap: None,
            model_refresh_pending: false,
            native_render: false,
            normal_snapshot: None,
            history_browser: false,
            conversation: Transcript::with_read_preview(
                agent.as_ref().is_ok_and(Agent::read_tool_result_preview),
            ),
            notifications: Notifications::new(notification_options),
            show_welcome: true,
            input: editor(),
            shell_mode: false,
            input_history: Vec::new(),
            history_index: None,
            history_draft: None,
            state: State::Unavailable,
            next_shell_id: 0,
            active_cancellation: None,
            input_queue: InputQueue::default(),
            queue_paused: false,
            queue_menu: false,
            queue_menu_revision: None,
            queue_snapshot: None,
            ask_panel: None,
            ask_reply: None,
            commands: None,
            events: None,
            unavailable: None,
            choices: Vec::new(),
            current_model: None,
            default_model_reference: None,
            favorite_models: Vec::new(),
            think_level: ThinkLevel::None,
            default_think_level: ThinkLevel::default(),
            menu: None,
            menu_dismissed: false,
            model_picker_draft: None,
            model_panel: None,
            secondary_status: None,
            skill_worker: skills::Worker::new(skills)?,
            skill_panel: None,
            skills_pending: false,
            session_panel: None,
            tree_panel: None,
            changes_panel: None,
            plan_list_panel: None,
            plan_list_pending: None,
            plan_reenter_standby: false,
            plan_leave_confirmation: None,
            escape_tap: EscapeTap::default(),
            session_id: None,
            usage: None,
            token_speed: statusline::TokenSpeed::default(),
            mcp_cleanup: None,
            background_cleanup: None,
            file_commands,
            file_worker: files::Worker::new(cwd)?,
            file_paths: Vec::new(),
            file_directory: String::new(),
            file_query: None,
            files_pending: false,
            workflow: WorkflowState::default(),
            plan_panel: None,
            plan_progress: (0, 0),
            resume_note: None,
            plan_retry_enter: false,
            plan_after_enter: None,
            review_transcript: None,
            plan_review_pending: None,
        };
        let mut startup_notice = None;
        match agent {
            Ok(agent) => app.install_agent(agent)?,
            Err(error) => {
                let error = format!("{error:#}");
                startup_notice = Some((
                    Level::Error,
                    format!(
                        "{error}\nConfiguration unavailable; model tasks are disabled. Use /login to authorize a subscription, or fix the config files. /quit still exits cleanly."
                    ),
                ));
                app.unavailable = Some(error);
            }
        }
        if !diagnostics.is_empty() {
            merge_startup_notice(
                &mut startup_notice,
                format!(
                    "Skill scan issues: {}. Use /skills for details.",
                    diagnostics.len()
                ),
            );
        }
        if !app.file_commands.diagnostics.is_empty() {
            merge_startup_notice(
                &mut startup_notice,
                format!(
                    "Command scan issues: {}. Use /help for details.",
                    app.file_commands.diagnostics.len()
                ),
            );
        }
        if let Some((level, message)) = startup_notice {
            app.notify(level, message);
        }
        Ok(app)
    }
    pub(in crate::tui) fn install_agent(&mut self, mut agent: Agent) -> Result<()> {
        // 取 replay、模型字段与 MCP 清理句柄必须发生在 agent move 进 worker 之前。
        let session_id = agent.session_id().map(str::to_owned);
        let replay = agent.take_replay();
        let current_model = agent.current_model().clone();
        let think_level = agent.think_level();
        let default_think_level = agent.default_think_level();
        let mcp_cleanup = agent.mcp_cleanup()?;
        self.background_cleanup = Some(agent.background_cleanup());
        self.choices = agent.model_choices();
        self.favorite_models = agent.favorite_models().to_vec();
        self.default_model_reference = Some(agent.default_model_reference().to_owned());
        let (commands, receiver) = mpsc::channel();
        let (sender, events) = mpsc::sync_channel(256);
        let ask_events = sender.clone();
        agent.set_ask_handler(Box::new(move |request, cancellation| {
            request_ask(&ask_events, request, cancellation)
        }));
        let usage = agent.context_usage().ok();
        // 经 set_workflow 走统一入口：恢复的计划正文必须同步种子进度缓存，
        // 否则 /plan view 正文未变时不重算，标题停留 0/0。
        self.set_workflow(agent.workflow().clone());
        // Deliberately detached: quitting must not wait for HTTP or foreground tools.
        thread::Builder::new()
            .name("pp-agent".into())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    let started = Instant::now();
                    let mut emit = |event| {
                        let _ = sender.send(WorkerEvent::Progress(event));
                    };
                    // 状态切换成功后紧随一份新用量快照，与动作保持 FIFO 顺序。
                    let mut snapshots: Vec<WorkerEvent> = Vec::new();
                    let event = match command {
                        WorkerCommand::ReloadModels => {
                            WorkerEvent::ModelsReloaded(agent.reload_models().map(|()| {
                                crate::tui::ModelListing {
                                    choices: agent.model_choices(),
                                    default_reference: agent.default_model_reference().to_owned(),
                                    favorites: agent.favorite_models().to_vec(),
                                }
                            }))
                        }
                        WorkerCommand::Shell {
                            command,
                            call_id,
                            cancellation,
                        } => {
                            let result =
                                agent.run_shell(&command, &call_id, &cancellation, &mut emit);
                            let cleanup_failed = result.as_ref().is_err_and(|error| {
                                error
                                    .chain()
                                    .any(|cause| cause.is::<crate::cancellation::CleanupError>())
                            });
                            if sender.send(WorkerEvent::ShellCompleted(result)).is_err()
                                || cleanup_failed
                            {
                                break;
                            }
                            continue;
                        }
                        WorkerCommand::Run {
                            prompt,
                            cancellation,
                            input,
                        } => {
                            let outcome =
                                agent.run(&prompt, &cancellation, Some(&input), &mut emit);
                            // 计划工具可能修改 workflow；终态后同步一份快照。
                            push_workflow_snapshot(&agent, &mut snapshots);
                            WorkerEvent::completed(started, outcome)
                        }
                        WorkerCommand::Continue {
                            cancellation,
                            input,
                        } => {
                            let outcome =
                                agent.continue_session(&cancellation, Some(&input), &mut emit);
                            push_workflow_snapshot(&agent, &mut snapshots);
                            WorkerEvent::completed(started, outcome)
                        }
                        WorkerCommand::Compact { cancellation } => {
                            let result = agent.compact(&cancellation, &mut emit);
                            // 终态事件之前先发布最终用量快照；
                            // 成功、失败、取消、未变化都会刷新状态栏。
                            push_runtime_snapshot(&agent, &mut snapshots);
                            WorkerEvent::Compacted(result)
                        }
                        WorkerCommand::Reset => {
                            agent.reset();
                            push_runtime_snapshot(&agent, &mut snapshots);
                            WorkerEvent::Reset
                        }
                        WorkerCommand::SetThinkLevel(level) => {
                            let result = agent.set_think_level(level).map(|()| agent.think_level());
                            if result.is_ok() {
                                push_runtime_snapshot(&agent, &mut snapshots);
                            }
                            WorkerEvent::ThinkLevelSet(result)
                        }
                        WorkerCommand::SaveDefaultThinkLevel(level) => {
                            WorkerEvent::DefaultThinkLevelSaved(
                                agent.save_default_think_level(level).map(|()| level),
                            )
                        }
                        WorkerCommand::SaveDefaultModel(reference) => {
                            WorkerEvent::DefaultModelSaved(
                                agent.save_default_model(&reference).map(|()| reference),
                            )
                        }
                        WorkerCommand::ToggleFavoriteModel(reference) => {
                            WorkerEvent::FavoriteModelsSaved(
                                agent.toggle_favorite_model(&reference),
                            )
                        }
                        WorkerCommand::SwitchModel(reference) => {
                            let result = agent
                                .reload_models()
                                .and_then(|()| agent.switch_model(&reference))
                                .map(|()| (agent.current_model().clone(), agent.think_level()));
                            if result.is_ok() {
                                push_runtime_snapshot(&agent, &mut snapshots);
                            }
                            WorkerEvent::ModelSwitched(result)
                        }
                        WorkerCommand::ReadChanges => WorkerEvent::ChangesRead(agent.changes()),
                        WorkerCommand::UndoFile(token) => {
                            let result = agent.undo_file(&token);
                            if result.is_err() {
                                snapshots.push(WorkerEvent::ChangesRead(agent.changes()));
                            }
                            WorkerEvent::FileUndone(result)
                        }
                        WorkerCommand::ReadTree => WorkerEvent::TreeRead(agent.session_tree()),
                        WorkerCommand::NavigateTree(id) => {
                            let result = agent.navigate_tree(&id);
                            if result.is_ok() {
                                push_runtime_snapshot(&agent, &mut snapshots);
                            }
                            WorkerEvent::TreeNavigated(result)
                        }
                        WorkerCommand::ListSessions => WorkerEvent::SessionsListed(
                            crate::config::global_dir()
                                .map(|dir| dir.join("sessions"))
                                .and_then(|dir| crate::agent::persistence::list(&dir)),
                        ),
                        WorkerCommand::DeleteSession(id) => {
                            let result = crate::config::global_dir()
                                .map(|dir| dir.join("sessions"))
                                .and_then(|dir| crate::agent::persistence::delete(&dir, &id));
                            WorkerEvent::SessionDeleted(id, result)
                        }
                        WorkerCommand::Resume { id, cancellation } => {
                            let result = agent.resume(&id, &cancellation);
                            match result {
                                Ok(preparation) => {
                                    // 先交接清理句柄并等待 UI 确认，再补会话通知。
                                    let prepared =
                                        publish_prepared(&sender, preparation, &cancellation);
                                    WorkerEvent::SessionResumed(match prepared {
                                        Ok(()) => Ok(ResumeState { id }),
                                        Err(error) => Err(error),
                                    })
                                }
                                Err(error) => WorkerEvent::SessionResumed(Err(error)),
                            }
                        }
                        WorkerCommand::EnterPlan { cancellation } => {
                            let result = agent.enter_plan(&cancellation);
                            if result.is_ok() {
                                if let Some(event) = usage_snapshot(&agent) {
                                    snapshots.push(event);
                                }
                            } else {
                                // 转换失败也可能已持久化 Plan；发布真实模式避免假 EXEC，
                                // 并刷新退出清理句柄以覆盖可能被保留的 registry。
                                publish_transition_failure(&agent, &mut snapshots);
                            }
                            WorkerEvent::WorkflowChanged(result)
                        }
                        WorkerCommand::ReadPlan => WorkerEvent::PlanRead(agent.read_plan()),
                        WorkerCommand::ListPlans => WorkerEvent::PlansListed(agent.list_plans()),
                        WorkerCommand::ReadListedPlan { id } => {
                            WorkerEvent::ListedPlanRead(agent.read_listed_plan(&id))
                        }
                        WorkerCommand::OpenListedPlan {
                            selection,
                            intent,
                            cancellation,
                        } => WorkerEvent::ListedPlanOpened(agent.open_listed_plan(
                            &selection,
                            intent,
                            &cancellation,
                        )),
                        WorkerCommand::RenameListedPlan {
                            selection,
                            file_name,
                        } => WorkerEvent::ListedPlanRenamed(
                            agent.rename_listed_plan(selection, file_name),
                        ),
                        WorkerCommand::DeleteListedPlan { selection } => {
                            WorkerEvent::ListedPlanDeleted(agent.delete_listed_plan(selection))
                        }
                        command @ (WorkerCommand::LeavePlan { .. }
                        | WorkerCommand::SavePlanAndLeave { .. }) => {
                            let (result, cancellation) = match command {
                                WorkerCommand::LeavePlan {
                                    cancellation,
                                    discard_plan_id,
                                } => (
                                    agent.leave_plan(&cancellation, discard_plan_id.as_deref()),
                                    cancellation,
                                ),
                                WorkerCommand::SavePlanAndLeave {
                                    token,
                                    cancellation,
                                    input,
                                } => (
                                    agent.save_plan_and_leave(token, &input, &cancellation),
                                    cancellation,
                                ),
                                _ => unreachable!(),
                            };
                            match result {
                                Ok(preparation) => {
                                    match publish_prepared(&sender, preparation, &cancellation) {
                                        Ok(()) => WorkerEvent::WorkflowChanged(Ok(agent
                                            .workflow()
                                            .clone())),
                                        Err(error) => {
                                            publish_transition_failure(&agent, &mut snapshots);
                                            WorkerEvent::WorkflowChanged(Err(error))
                                        }
                                    }
                                }
                                Err(error) => {
                                    publish_transition_failure(&agent, &mut snapshots);
                                    WorkerEvent::WorkflowChanged(Err(error))
                                }
                            }
                        }
                        WorkerCommand::ApprovePlan {
                            token,
                            context,
                            cancellation,
                            input,
                        } => {
                            match agent.approve_plan(
                                token,
                                context,
                                &input,
                                &cancellation,
                                &mut emit,
                            ) {
                                Ok(preparation) => {
                                    match publish_prepared(&sender, preparation, &cancellation) {
                                        // UI 确认后才启动模型执行。
                                        Ok(()) => WorkerEvent::completed(
                                            started,
                                            agent.continue_session(
                                                &cancellation,
                                                Some(&input),
                                                &mut emit,
                                            ),
                                        ),
                                        Err(error) => {
                                            publish_transition_failure(&agent, &mut snapshots);
                                            WorkerEvent::WorkflowChanged(Err(error))
                                        }
                                    }
                                }
                                Err(error) => {
                                    // 审批失败可能已在 worker 内导入新修订（计划变更）。
                                    publish_transition_failure(&agent, &mut snapshots);
                                    WorkerEvent::WorkflowChanged(Err(error))
                                }
                            }
                        }
                        WorkerCommand::ReviewPlan {
                            token,
                            cancellation,
                            input,
                        } => {
                            // 审查进度走独立事件流；不发布普通 Progress/用量快照。
                            let mut review_emit = |event| {
                                let _ = sender.send(WorkerEvent::ReviewProgress(event));
                            };
                            let result =
                                agent.review_plan(token, &input, &cancellation, &mut review_emit);
                            // 成功时报告已由 write_workflow 登记；
                            // 失败/取消保留旧报告。快照纠正显示不引入新权限。
                            push_workflow_snapshot(&agent, &mut snapshots);
                            WorkerEvent::PlanReviewed(result)
                        }
                    };
                    // Report new IDs even on task failure, before Completed.
                    if let Some(id) = agent.take_session_notice()
                        && sender.send(WorkerEvent::SessionStarted(id)).is_err()
                    {
                        break;
                    }
                    if sender.send(event).is_err() {
                        break;
                    }
                    for event in snapshots {
                        if sender.send(event).is_err() {
                            break;
                        }
                    }
                }
            })
            .context("starting agent worker")?;
        self.commands = Some(commands);
        self.events = Some(events);
        self.state = State::Ready;
        self.mcp_cleanup = Some(mcp_cleanup);
        self.usage = usage;
        if let Some(id) = session_id {
            let choices = std::mem::take(&mut self.choices);
            let error = self.adopt_session(
                replay,
                choices,
                current_model,
                think_level,
                default_think_level,
            );
            self.session_id = Some(id.clone());
            let notice = match error {
                Some(error) => (Level::Error, format!("Resumed session: {id}\n{error}")),
                None => (Level::Normal, format!("Resumed session: {id}")),
            };
            self.notify(notice.0, notice.1);
        } else {
            self.current_model = Some(current_model);
            self.think_level = think_level;
            self.default_think_level = default_think_level;
        }
        self.unavailable = None;
        Ok(())
    }
}
