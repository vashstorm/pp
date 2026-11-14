use super::format::{Message, NativeAppendRef};
use super::*;
use anyhow::ensure;

impl SessionJournal {
    pub(in crate::agent) fn sent(
        &mut self,
        metadata: &Metadata<'_>,
        session: SessionRef<'_>,
        workflow: &WorkflowState,
        user: Option<&str>,
        instructions: Option<&str>,
    ) -> Result<()> {
        self.check()?;
        self.ensure_active()?;
        let first = !self
            .active
            .as_ref()
            .expect("active journal after ensure_active")
            .header_written;
        if self.pending_checkpoint || first {
            self.checkpoint(metadata, session, workflow)?;
        } else {
            if self.pending_tools {
                let tools = match session {
                    SessionRef::Completion(request) => serde_json::to_value(&request.tools)?,
                    SessionRef::Response(request) => serde_json::to_value(&request.tools)?,
                };
                self.custom(CustomRef::ToolDefinitions { tools: &tools }, false)?;
                self.pending_tools = false;
            }
            if let Some(system) = instructions {
                self.custom(CustomRef::Instructions { system }, false)?;
            }
            self.commit(session)?;
        }
        if let Some(text) = user {
            let result = now_ms().and_then(|timestamp| {
                self.record(
                    PayloadRef::Message {
                        message: &Message::User {
                            content: text,
                            timestamp,
                        },
                    },
                    true,
                )
            });
            self.latch(result)?;
        }
        Ok(())
    }
    pub(in crate::agent) fn commit(&mut self, session: SessionRef<'_>) -> Result<()> {
        self.check()?;
        if self.active.is_none() {
            return Ok(());
        }
        let end = session.len();
        ensure!(
            self.committed <= end,
            "persisting session: invalid history cursor"
        );
        if end > self.committed {
            // Move only these small display buffers, never the native request/history.
            let model_reference = std::mem::take(&mut self.model_reference);
            let thinking = std::mem::take(&mut self.thinking);
            let result = (|| -> Result<()> {
                let timestamp = now_ms()?;
                let provider = model_reference
                    .split_once('/')
                    .map_or(model_reference.as_str(), |(provider, _)| provider);
                let (messages, append) = match session {
                    SessionRef::Completion(request) => {
                        let slice = &request.messages[self.committed..];
                        (
                            format::completion(
                                slice,
                                provider,
                                &request.model,
                                &thinking,
                                timestamp,
                            ),
                            NativeAppendRef::Completion { messages: slice },
                        )
                    }
                    SessionRef::Response(request) => {
                        let InputParam::Items(items) = &request.input else {
                            unreachable!()
                        };
                        let slice = &items[self.committed..];
                        (
                            format::response(
                                slice,
                                provider,
                                request.model.as_deref().unwrap_or_default(),
                                timestamp,
                            ),
                            NativeAppendRef::Response { items: slice },
                        )
                    }
                };
                let captured = self.capture.active();
                if captured {
                    let reason = if messages
                        .iter()
                        .any(|m| matches!(m, Message::ToolResult { .. }))
                    {
                        "toolUse"
                    } else {
                        "stop"
                    };
                    self.write_display(reason, Some(&model_reference))?;
                }
                for message in messages.iter().filter(|_| !captured) {
                    self.record(PayloadRef::Message { message }, false)?;
                }
                self.custom(CustomRef::NativeAppend(append), true)?;
                self.committed = end;
                if let Some(key) = self.pending_stats.take()
                    && let Some(id) = self.id()
                {
                    pp_stats::link(&self.stats_db, &key, &self.dir.join(format!("{id}.jsonl")));
                }
                Ok(())
            })();
            self.model_reference = model_reference;
            self.thinking = thinking;
            self.thinking.clear();
            self.latch(result)?;
        }
        Ok(())
    }
    pub(in crate::agent) fn event(&mut self, event: &AgentEvent) {
        if matches!(
            event,
            AgentEvent::ContextUpdated { .. }
                | AgentEvent::ToolArgumentsDelta { .. }
                | AgentEvent::ToolOutputSnapshot { .. }
                | AgentEvent::ToolPipelineSnapshot { .. }
                | AgentEvent::ToolShellSnapshot { .. }
                | AgentEvent::TodoStateUpdated { .. }
                | AgentEvent::PlanProgressUpdated { .. }
                | AgentEvent::CompactionStarted { .. }
                | AgentEvent::CompactionFinished { .. }
                | AgentEvent::UserInputAccepted { .. }
                | AgentEvent::UserInputRejected { .. }
                | AgentEvent::HookDiagnostic { .. }
                | AgentEvent::ModelRequestStarted
                | AgentEvent::ModelOutputProgress { .. }
                | AgentEvent::ModelRequestFinished { .. }
                | AgentEvent::RequestRetrying { .. }
        ) {
            return;
        }
        if matches!(
            event,
            AgentEvent::ModelAttemptFailed { .. } | AgentEvent::AssistantStarted
        ) {
            let result = self.write_display("error", None);
            let _ = self.latch(result);
        }
        if matches!(event, AgentEvent::AssistantStarted) {
            let id = self
                .active
                .as_ref()
                .and_then(|active| active.tail.clone())
                .unwrap_or_default();
            self.capture.start(id);
        }
        match event {
            AgentEvent::ToolStarted { id, name, .. } => {
                self.tool_started
                    .insert(id.clone(), (name.clone(), Instant::now()));
            }
            AgentEvent::ToolFinished { id, result } => {
                if let Some((tool, started)) = self.tool_started.remove(id) {
                    self.tool_finished(&tool, id, result, started.elapsed());
                }
            }
            _ => {}
        }
        if !matches!(
            event,
            AgentEvent::AssistantStarted | AgentEvent::ModelAttemptFailed { .. }
        ) && self.capture.event(event)
        {
            return;
        }
        let _ = self.custom(CustomRef::Event { event }, false);
    }
    fn write_display(&mut self, reason: &str, model_reference: Option<&str>) -> Result<()> {
        self.check()?;
        let mut messages = self.capture.messages(
            reason,
            model_reference.unwrap_or(&self.model_reference),
            self.capture_thinking,
        )?;
        for message in &mut messages {
            if message["role"] == "assistant" {
                message["model"] = self.wire_model.clone().into();
            }
        }
        for message in &messages {
            self.record(PayloadRef::DisplayMessage { message }, false)?;
        }
        Ok(())
    }

    pub(in crate::agent) fn cancelled(&mut self) -> Result<()> {
        self.pending_stats = None;
        self.write_display("aborted", None)?;
        self.custom(CustomRef::Cancelled {}, true)
    }
    pub(in crate::agent) fn error(&mut self, message: &str) {
        self.pending_stats = None;
        let result = self.write_display("error", None);
        let _ = self.latch(result);
        let _ = self.custom(CustomRef::Error { message }, true);
    }
    pub(in crate::agent) fn think(&mut self, level: ThinkLevel) -> Result<()> {
        self.record(
            PayloadRef::ThinkingLevelChange {
                thinking_level: level,
            },
            true,
        )
    }
}
