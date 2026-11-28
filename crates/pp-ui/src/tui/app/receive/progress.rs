//! Progress events for the main conversation and the standalone review tab: usage, token
//! speed, retries and the plan progress cache.

use crate::tui::{AgentEvent, App, Instant, ModelSource, Selection};

impl App {
    /// Main conversation progress: App caches usage and token speed, everything else goes to
    /// Transcript.
    pub(super) fn on_agent_event(&mut self, event: AgentEvent) -> bool {
        // The usage snapshot is cached by App for status-line rendering and never enters
        // Transcript.
        match event {
            event @ (AgentEvent::ThinkingDelta { .. }
            | AgentEvent::ReasoningSummaryDelta { .. }
            | AgentEvent::AssistantDelta { .. }) => {
                if let Some(previous) = self.review_stream.take()
                    && let Some(review) = &mut self.review_transcript
                {
                    review.progress(previous);
                }
                if let Some(previous) = self.stream.push(event) {
                    self.conversation.progress(previous);
                }
                if self.stream.is_full() {
                    self.flush_stream();
                }
            }
            AgentEvent::TodoStateUpdated { state } => self.todo_panel.update(state),
            AgentEvent::ContextUpdated { usage } => self.usage = Some(usage),
            AgentEvent::ModelRouteChanged {
                model,
                effort,
                source,
                temporary,
            } => {
                if !temporary {
                    if let Some(choice) =
                        self.choices.iter().find(|c| c.reference == model).cloned()
                    {
                        self.current_model = Some(choice);
                        self.think_level = effort;
                        self.usage = None;
                    }
                    // The source follows the actual route: override/default sync the override
                    // snapshot, while role/fallback only override the current route and leave the
                    // manual override slot untouched.
                    self.current_source = Some(source);
                    self.override_selection = match source {
                        ModelSource::Override => Some(Selection {
                            reference: model.clone(),
                            effort,
                        }),
                        ModelSource::Default => None,
                        ModelSource::Role | ModelSource::Fallback => {
                            self.override_selection.clone()
                        }
                    };
                    if let Some(panel) = &mut self.roles_panel {
                        panel.update_override(self.override_selection.clone());
                    }
                }
                self.conversation.progress(AgentEvent::ModelRouteChanged {
                    model,
                    effort,
                    source,
                    temporary,
                });
            }
            AgentEvent::ModelRequestStarted => self.token_speed.start(Instant::now()),
            AgentEvent::ModelOutputProgress { output_bytes } => {
                self.token_speed.progress(output_bytes, Instant::now())
            }
            AgentEvent::ModelRequestFinished {
                elapsed_ms,
                usage,
                success,
            } => {
                self.retry_finished();
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
            // During a retry only the fixed notice box is updated and no conversation entry is
            // appended.
            AgentEvent::RequestRetrying {
                reason,
                attempt,
                wait_ms,
            } => self.retry_started(reason, attempt, wait_ms),
            // Input acceptance is echoed directly by App; it never enters Transcript progress and
            // records no duplicate history.
            AgentEvent::UserInputAccepted { text, .. } => {
                self.append("You", &text);
            }
            // Input rejected by a hook shows only the reason and the original text and is not
            // disguised as a successful You input.
            AgentEvent::UserInputRejected { text, reason, .. } => {
                let mut body = format!("Input rejected: {reason}");
                if !text.trim().is_empty() {
                    body.push_str("\n\n");
                    body.push_str(&pp_core::text::display_text(&text));
                }
                self.append("Rejected", &body);
            }
            // Execution-time plan progress: only the cache and read-only panel are updated, and
            // the whole body is not appended to Transcript.
            AgentEvent::PlanProgressUpdated {
                plan_id,
                revision,
                content,
                file_name,
                location,
                status,
            } => self.apply_plan_progress(&plan_id, revision, content, file_name, location, status),
            event => self.conversation.progress(event),
        }
        true
    }

    /// Review progress goes only to the standalone review transcript: a separate method keeps
    /// the ordering of both stream buffers.
    pub(super) fn on_review_progress(&mut self, event: AgentEvent) -> bool {
        // Review progress goes only to the standalone review transcript: no usage/main
        // conversation/dispatch updates.
        if let Some(event) = self.stream.take() {
            self.conversation.progress(event);
        }
        if matches!(
            &event,
            AgentEvent::ThinkingDelta { .. }
                | AgentEvent::ReasoningSummaryDelta { .. }
                | AgentEvent::AssistantDelta { .. }
        ) {
            if let Some(previous) = self.review_stream.push(event)
                && let Some(review) = &mut self.review_transcript
            {
                review.progress(previous);
            }
            if self.review_stream.is_full() {
                self.flush_stream();
            }
        } else if let Some(review) = &mut self.review_transcript {
            review.progress(event);
        }
        true
    }
}
