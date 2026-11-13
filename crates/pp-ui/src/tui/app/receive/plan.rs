//! 计划类事件：/plan-list 管理、审批转换、审阅终态与执行准备交接。

use crate::tui::{
    App, Level, PlanEnterAction, PlanLocation, PlanOpenIntent, State, WorkMode, WorkerCommand,
    WorkerEvent, plan_list, plan_location_from_path,
};

impl App {
    /// 计划事件族；采纳快照与 ACK 的顺序决定 worker 何时继续。
    pub(super) fn on_plan_event(&mut self, event: WorkerEvent) -> bool {
        let mut changed = false;
        match event {
            WorkerEvent::PlansListed(result) => {
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
            WorkerEvent::ListedPlanRead(result) => {
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
                        if plan_location_from_path(&selection.path) == Some(PlanLocation::Done) {
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

            WorkerEvent::ListedPlanOpened(result) => {
                self.state = State::Ready;
                let execute_review = matches!(
                    self.plan_list_pending.take(),
                    Some(plan_list::PendingOp::Open(PlanOpenIntent::Execute))
                );
                match result {
                    Ok(preparation) => {
                        // 采纳 workflow/replay/todo 快照后展示；不自动请求模型。
                        let path = preparation
                            .workflow
                            .plan
                            .as_ref()
                            .and_then(|plan| plan.path())
                            .unwrap_or_else(|| "Not saved yet".to_owned());
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
                                format!(
                                    "Opened plan draft {path}; describe the next planning input."
                                ),
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
            WorkerEvent::ListedPlanRenamed(result) => {
                self.state = State::Ready;
                if let Some(panel) = &mut self.plan_list_panel {
                    match result {
                        Ok(listing) => panel.set_listing(listing),
                        Err(error) => panel.set_error(format!("{error:#}")),
                    }
                }
                changed = true;
            }
            WorkerEvent::ListedPlanDeleted(result) => {
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

            WorkerEvent::WorkflowChanged(result) => {
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
                        let reentered =
                            self.plan_reenter_standby && self.workflow.mode == WorkMode::Execute;
                        if reentered {
                            self.plan_reenter_standby = false;
                            self.plan_after_enter = Some(PlanEnterAction::Standby);
                            self.start_plan_transition(
                                |cancellation| WorkerCommand::EnterPlan { cancellation },
                                State::ChangingPlan,
                                "Entering Plan mode...",
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
                                Some(PlanEnterAction::Prompt(prompt, prepared)) => {
                                    let display = prepared.as_ref().map_or_else(
                                        || prompt.clone(),
                                        |input| input.display.clone(),
                                    );
                                    self.resources.ready = prepared;
                                    self.queue_paused = false;
                                    if self.send_task(Some(prompt.clone())) {
                                        self.append("You", &display);
                                    }
                                }
                                None => {}
                            }
                        }
                    }
                    Err(error) => {
                        self.plan_reenter_standby = false;
                        self.notify(Level::Error, format!("{error:#}"));
                        if let Some(PlanEnterAction::Prompt(prompt, prepared)) = after_enter
                            && self.input.is_empty()
                        {
                            if let Some(prepared) = prepared {
                                self.resources.set_images(prepared.images);
                            }
                            self.input.insert_str(format!("/plan {prompt}"));
                            self.resources.bind_text(&self.input_text());
                            self.refresh_candidates(true);
                        }
                        // 失败也可能已持久化 Plan（随后的 Workflow 快照纠正显示）；
                        // 仅进入失败时下一次 /plan 优先重试进入；退出失败保持离开方向，
                        // 不把重试误导成再次进入。
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

            WorkerEvent::PlanRead(result) => {
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
                                self.plan_panel =
                                    Some(crate::tui::plan::Panel::degraded(document, report));
                            }
                        }
                        self.notify(Level::Error, format!("{error:#}"));
                    }
                }
                changed = true;
            }

            WorkerEvent::PlanReviewed(result) => {
                // 审查终态：无论成败都保持 Plan/Ready 并回五项菜单；
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
            WorkerEvent::Workflow(workflow) => {
                self.set_workflow(workflow);
                changed = true;
            }

            WorkerEvent::ExecutionPrepared {
                workflow,
                session_id,
                usage,
                replay,
                todo_snapshot,
                reply,
            } => {
                // 一次性交接：先采纳 workflow/replay/todo 快照并更新会话，再 ACK；
                // ACK 后 worker 才允许 continue_session。
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
                self.usage = usage;
                self.task_status = self.task_status_view.snapshot();
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
            _ => {}
        }
        changed
    }
}
