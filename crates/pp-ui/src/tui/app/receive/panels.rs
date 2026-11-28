//! Panel and session events: tree, session list, subagent root, MCP and LSP.

use crate::tui::{App, Level, State, WorkerEvent, editor};

impl App {
    /// Panel/session event family: it only affects local panels and snapshots; all I/O still
    /// runs in the worker.
    pub(super) fn on_panel_event(&mut self, event: WorkerEvent) -> bool {
        let mut changed = false;
        match event {
            WorkerEvent::SessionStarted(id) => {
                self.notify(Level::Normal, format!("Session: {id}"));
                self.session_id = Some(id);
                changed = true;
            }
            WorkerEvent::PipeStoreBound { store } => {
                // Republishing the same instance is idempotent; only a root switch bumps the
                // epoch, and the panel drops old replies accordingly.
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
                // Deletion still goes through the Agent worker's session lock and JSONL removal;
                // the derived index is synced only after success.
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
                        // Report the deletion first, then show index refresh errors; a failed
                        // refresh clears the stale rows.
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
                        // History, cost and usage were already taken over by ExecutionPrepared;
                        // this only adds the notification.
                        self.token_speed.clear();
                        match self.resume_note.take() {
                            Some(error) => self.notify(Level::Error, error),
                            None => {
                                self.notify(Level::Normal, format!("Resumed session: {}", state.id))
                            }
                        }
                    }
                    Err(error) => {
                        // Resume failed: the list, selection and old conversation are kept so the
                        // user can pick again or cancel.
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
