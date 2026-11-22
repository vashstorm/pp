//! 面板与会话类事件：树、会话列表、子 Agent 根、MCP 与 LSP。

use crate::tui::{App, Level, State, WorkerEvent, editor};

impl App {
    /// 面板／会话事件族：只在本地面板与快照上生效，I/O 仍由 worker 完成。
    pub(super) fn on_panel_event(&mut self, event: WorkerEvent) -> bool {
        let mut changed = false;
        match event {
            WorkerEvent::SessionStarted(id) => {
                self.notify(Level::Normal, format!("Session: {id}"));
                self.session_id = Some(id);
                changed = true;
            }
            WorkerEvent::PipeStoreBound { store } => {
                // 同一实例重复发布是幂等的；换根才递增 epoch，面板据此丢弃旧回复。
                if self.install_pipe_store(Some(store)) {
                    changed = true;
                }
            }

            WorkerEvent::ContextInspected(result) => {
                if matches!(self.state, State::InspectingContext) {
                    self.state = State::Ready;
                }
                match result {
                    Ok(mut report) => {
                        if let Some(settings) = &self.cached_settings {
                            report.auto_compact = settings.auto_compact;
                            report.threshold_percent = settings.auto_compact_threshold_percent;
                        }
                        if let Some(panel) = &mut self.context_panel {
                            panel.set_report(report);
                        }
                    }
                    Err(error) => {
                        let message = format!("Failed to inspect context: {error:#}");
                        if let Some(panel) = &mut self.context_panel {
                            panel.set_error(message.clone());
                        }
                        self.notify(Level::Error, message);
                    }
                }
                changed = true;
            }

            WorkerEvent::TreeRead(result) => {
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
            WorkerEvent::TreeNavigated(result) => {
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

            WorkerEvent::SessionDeleted(result) => {
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
                    Err(error) => {
                        self.notify(Level::Error, format!("Failed to delete session: {error:#}"))
                    }
                }
                changed = true;
            }
            WorkerEvent::SessionResumed(result) => {
                self.state = State::Ready;
                match result {
                    Ok(state) => {
                        self.session_id = Some(state.id.clone());
                        self.session_panel = None;
                        // 历史、费用和用量已由 ExecutionPrepared 接管；这里只补通知。
                        self.token_speed.clear();
                        match self.resume_note.take() {
                            Some(error) => self.notify(Level::Error, error),
                            None => {
                                self.notify(Level::Normal, format!("Resumed session: {}", state.id))
                            }
                        }
                    }
                    Err(error) => {
                        // 恢复失败：保留列表、选中项与旧对话，可重新选择或取消。
                        if let Some(panel) = &mut self.session_panel {
                            panel.restore_failed();
                        }
                        self.notify(Level::Error, format!("Failed to resume session: {error:#}"));
                    }
                }
                changed = true;
            }

            WorkerEvent::SubagentsEnsured(result) => {
                self.subagents_ensured(result);
                changed = true;
            }

            WorkerEvent::McpSnapshot(view) => {
                self.mcp_view = view;
                changed = true;
            }
            WorkerEvent::SandboxStatus(result) => {
                match result {
                    Ok((status, updated)) => {
                        self.append("Sandbox", &status.summary());
                        if updated {
                            self.notify(
                                Level::Normal,
                                format!("Sandbox mode set to {}", status.effective.as_str()),
                            );
                        }
                    }
                    Err(error) => self.notify(Level::Error, format!("{error:#}")),
                }
                changed = true;
            }
            WorkerEvent::Dumped(result) => {
                match result {
                    Ok(output) => {
                        self.append("Dump", &output.summary());
                        self.notify(Level::Normal, output.notification());
                    }
                    Err(error) => self.notify(Level::Error, format!("{error:#}")),
                }
                changed = true;
            }
            WorkerEvent::McpManaged { result, view, show } => {
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
                                "MCP state updated; use /mcp list to view".into(),
                            );
                        }
                    }
                    Err(error) => {
                        if error.is::<pp_core::cancellation::CleanupError>() {
                            self.state = State::Unavailable;
                        }
                        self.notify(Level::Error, format!("{error:#}"));
                    }
                }
                changed = true;
            }

            WorkerEvent::LspWarmupFailed { server, error } => {
                self.notify(
                    Level::Error,
                    format!(
                        "LSP {} unavailable: {}",
                        pp_core::text::display_text(&server),
                        pp_core::text::display_text(&error)
                    ),
                );
                changed = true;
            }
            WorkerEvent::LspPollFailed(error) => {
                self.notify(
                    Level::Error,
                    format!("LSP diagnostics: {}", pp_core::text::display_text(&error)),
                );
                changed = true;
            }
            _ => {}
        }
        changed
    }
}
