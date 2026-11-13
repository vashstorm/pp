//! 主对话与独立审查页的进度事件：用量、测速、重试与计划进度缓存。

use crate::tui::{AgentEvent, App, Instant, ModelSource, Selection};

impl App {
    /// 主对话进度：用量与 token 测速由 App 缓存，其余交给 Transcript。
    pub(super) fn on_agent_event(&mut self, event: AgentEvent) -> bool {
        // 用量快照由 App 缓存，供状态栏渲染；不进入 Transcript。
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
                    // 来源跟随实际路由：override/default 同步覆盖快照，
                    // role/fallback 只覆盖当前路由，不改变手动覆盖槽。
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
            // 重试期间只更新固定信息框，不追加对话条目。
            AgentEvent::RequestRetrying {
                reason,
                attempt,
                wait_ms,
            } => self.retry_started(reason, attempt, wait_ms),
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
            } => self.apply_plan_progress(&plan_id, revision, content, file_name, location, status),
            event => self.conversation.progress(event),
        }
        true
    }

    /// 审查进度只进独立 review transcript：单独成方法以保留两套流缓冲顺序。
    pub(super) fn on_review_progress(&mut self, event: AgentEvent) -> bool {
        // 审查进度只进独立 review transcript：不更新用量/主对话/派发。
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
