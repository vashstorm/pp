//! Context usage estimation and the cleanup boundary for session cumulative output.
use super::{Agent, Session};
use anyhow::Result;
use async_openai::types::chat::ChatCompletionRequestMessage;
use async_openai::types::responses::InputParam;
use pp_core::context::{
    ContextUsage, MessageBytes, estimate_bytes, estimate_tokens, estimate_tokens_from_bytes,
};
use pp_core::events::AgentEvent;

/// Incremental byte accounting for the native request: request skeleton (model, tool definitions, …) plus per-message bytes.
///
/// Only new messages are serialized one by one; the skeleton is re-estimated every time, so changes to the
/// tool table, model and request configuration take effect automatically. Wholesale history replacement
/// (compaction, tree navigation, cross-protocol conversion, restore) must call [`Self::invalidate`];
/// appending messages needs no invalidation—appends only touch the tail.
#[derive(Default)]
pub(in crate::agent) struct SessionEstimate {
    messages: MessageBytes,
    /// Number of messages counted for usage: Completion skips system/developer, Response counts every item.
    session_messages: u64,
    /// Cached request skeleton byte count; invalidated by [`Self::invalidate`] or a skeleton fingerprint change.
    skeleton: Option<(SkeletonKey, u64)>,
    /// The next accounting pass must drop all accumulated values and recompute.
    stale: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SkeletonKey {
    protocol: u8,
    model_len: usize,
    tools_len: usize,
    instructions_len: usize,
}

impl SkeletonKey {
    fn of(session: &Session) -> Self {
        match session {
            Session::Completion(request) => Self {
                protocol: 0,
                model_len: request.model.len(),
                tools_len: request.tools.as_ref().map_or(0, Vec::len),
                instructions_len: 0,
            },
            Session::Response(request) => Self {
                protocol: 1,
                model_len: request.model.as_deref().map_or(0, str::len),
                tools_len: request.tools.as_ref().map_or(0, Vec::len),
                instructions_len: request.instructions.as_deref().map_or(0, str::len),
            },
        }
    }
}

impl SessionEstimate {
    /// Drop the accumulated values after a wholesale history replacement; the next [`Self::tokens`] recomputes fully.
    pub(in crate::agent) fn invalidate(&mut self) {
        self.stale = true;
        self.skeleton = None;
    }

    /// Return the current estimated token count of the native request; only the request skeleton and new messages are serialized.
    ///
    /// The metric matches [`Agent::context_usage`] exactly: an empty session (Completion with only
    /// system/developer, Response without input items) yields 0; otherwise native JSON bytes / 3.
    pub(in crate::agent) fn tokens(&mut self, session: &mut Session) -> Result<u64> {
        let count = match &*session {
            Session::Completion(request) => request.messages.len(),
            Session::Response(request) => match &request.input {
                InputParam::Items(items) => items.len(),
                // At runtime only array items are appended; text input bypasses the incremental cache to stay consistent with the full estimate.
                InputParam::Text(text) => {
                    self.invalidate();
                    return Ok(if text.is_empty() {
                        0
                    } else {
                        estimate_tokens(&**request)?
                    });
                }
            },
        };
        if self.stale || self.messages.count() > count {
            self.messages.clear();
            self.session_messages = 0;
            self.stale = false;
        }
        match &*session {
            Session::Completion(request) => {
                for message in &request.messages[self.messages.count()..] {
                    self.messages.push(estimate_bytes(message)?);
                    if !matches!(
                        message,
                        ChatCompletionRequestMessage::System(_)
                            | ChatCompletionRequestMessage::Developer(_)
                    ) {
                        self.session_messages += 1;
                    }
                }
            }
            Session::Response(request) => {
                let InputParam::Items(items) = &request.input else {
                    unreachable!("item input is checked above");
                };
                for item in &items[self.messages.count()..] {
                    self.messages.push(estimate_bytes(item)?);
                    self.session_messages += 1;
                }
            }
        }
        if self.session_messages == 0 {
            return Ok(0);
        }
        let key = SkeletonKey::of(session);
        let skeleton = match self.skeleton {
            Some((cached_key, bytes)) if cached_key == key => bytes,
            _ => {
                let bytes = Self::skeleton_bytes(session)?;
                self.skeleton = Some((key, bytes));
                bytes
            }
        };
        Ok(estimate_tokens_from_bytes(
            skeleton.saturating_add(self.messages.total()),
        ))
    }

    /// Request bytes with an empty message array (including the `[]` brackets).
    ///
    /// The request structure cannot be serialized piecewise, so the message array is temporarily moved out;
    /// moving it out and back are both O(1), and a serialization failure still puts it back before returning the error.
    fn skeleton_bytes(session: &mut Session) -> Result<u64> {
        match session {
            Session::Completion(request) => {
                let messages = std::mem::take(&mut request.messages);
                let bytes = estimate_bytes(&**request);
                request.messages = messages;
                bytes
            }
            Session::Response(request) => {
                let input = std::mem::replace(&mut request.input, InputParam::Items(Vec::new()));
                let bytes = estimate_bytes(&**request);
                request.input = input;
                bytes
            }
        }
    }
}

fn calibrate_tokens(
    raw_tokens: u64,
    last_request: Option<pp_core::context::TokenUsage>,
    last_request_raw_tokens: Option<u64>,
) -> u64 {
    let (Some(usage), Some(last_raw)) = (last_request, last_request_raw_tokens) else {
        return raw_tokens;
    };
    if last_raw == 0 || usage.input_tokens == 0 {
        return raw_tokens;
    }
    let within_bounds = u128::from(usage.input_tokens) * 10 >= u128::from(last_raw) * 4
        && u128::from(usage.input_tokens) <= u128::from(last_raw) * 3;
    if !within_bounds {
        return raw_tokens;
    }
    if raw_tokens >= last_raw {
        usage.input_tokens.saturating_add(raw_tokens - last_raw)
    } else {
        usage.input_tokens.saturating_sub(last_raw - raw_tokens)
    }
}

impl Agent {
    pub fn context_usage(&self) -> Result<ContextUsage> {
        // An empty session has not sent any context yet; only once a conversation exists do system prompt and tools count toward the full request overhead.
        let estimated_tokens = match &self.session {
            Session::Completion(request)
                if request.messages.iter().all(|message| {
                    matches!(
                        message,
                        ChatCompletionRequestMessage::System(_)
                            | ChatCompletionRequestMessage::Developer(_)
                    )
                }) =>
            {
                0
            }
            Session::Response(request)
                if match &request.input {
                    InputParam::Text(text) => text.is_empty(),
                    InputParam::Items(items) => items.is_empty(),
                } =>
            {
                0
            }
            Session::Completion(request) => estimate_tokens(request)?,
            Session::Response(request) => estimate_tokens(request)?,
        };
        Ok(self.usage_snapshot(estimated_tokens))
    }

    /// Same metric as [`Self::context_usage`], but reuses caller-held incremental accounting.
    ///
    /// `ContextUpdated` and the auto-compaction threshold read this single value, so one turn no longer recomputes it twice.
    pub(in crate::agent) fn context_usage_cached(
        &mut self,
        estimate: &mut SessionEstimate,
    ) -> Result<ContextUsage> {
        let estimated_tokens = estimate.tokens(&mut self.session)?;
        Ok(self.usage_snapshot(estimated_tokens))
    }

    fn usage_snapshot(&self, estimated_tokens: u64) -> ContextUsage {
        ContextUsage {
            estimated_tokens,
            context_window: self.current_model.context_window,
            last_request: self.last_request,
            session_output_tokens: self.session_output_tokens,
            auto_compact: self.auto_compact,
            threshold_percent: self.auto_compact_threshold_percent,
        }
    }

    /// Use the gap between the last server-reported `input_tokens` and the local raw estimate of that
    /// moment to boundedly calibrate the current incremental estimate, which drives auto-prune and auto-compaction thresholds.
    ///
    /// If the ratio between the reported value and the raw estimate falls outside the physically sane
    /// `[0.4, 3.0]` range (anomalous reports or fixed test-stub values), fall back to `raw_tokens` to avoid spurious or missed triggering.
    pub(in crate::agent) fn calibrated_context_tokens(&self, raw_tokens: u64) -> u64 {
        calibrate_tokens(raw_tokens, self.last_request, self.last_request_raw_tokens)
    }

    /// Sync the auto-compaction policy and threshold changed in `/setting` (effective immediately for the next request and `/context`).
    pub fn set_compaction_policy(&mut self, auto_compact: bool, threshold_percent: u8) {
        self.auto_compact = auto_compact;
        self.auto_compact_threshold_percent = threshold_percent;
    }

    /// A session-root change (new, restore, tree navigation, Plan handoff) drops both the last-request
    /// metrics and the cumulative session output; switching model, effort or compaction clears only the last-request metrics.
    pub(in crate::agent) fn clear_session_usage(&mut self) {
        self.last_request = None;
        self.last_request_raw_tokens = None;
        self.session_output_tokens = 0;
    }

    /// Backfill the last normal request usage and cumulative session output from `stats.db` when restoring a session.
    pub(in crate::agent) fn restore_session_usage(&mut self) {
        let Some(id) = self.journal.id() else {
            self.clear_session_usage();
            return;
        };
        let file = self.journal.directory().join(format!("{id}.jsonl"));
        match pp_stats::session_usage(self.journal.stats_database(), &file) {
            Ok(restored) => {
                self.last_request = restored.last_request;
                self.session_output_tokens = restored.session_output_tokens;
            }
            Err(_) => {
                tracing::warn!(event = "session_usage_history_unavailable");
                self.clear_session_usage();
            }
        }
    }

    pub(in crate::agent) fn publish_context(&self, emit: &mut dyn FnMut(AgentEvent)) -> Result<()> {
        emit(AgentEvent::ContextUpdated {
            usage: self.context_usage()?,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::history;
    use crate::agent::turn::{
        append_developer_to, append_input_to, append_user_to, response_input,
    };
    use crate::user_input::{Part, UserInput};
    use pp_config::config::EditMode;
    use pp_config::models::Protocol;
    use serde_json::json;

    fn new_session(protocol: Protocol) -> Session {
        Session::new(
            "fixture/model".into(),
            protocol,
            "system prompt".into(),
            EditMode::Replace,
        )
    }

    /// Full reference value independent of the incremental path: the whole native request serialized once.
    fn full_tokens(session: &Session) -> u64 {
        match session {
            Session::Completion(request) => estimate_tokens(request).unwrap(),
            Session::Response(request) => estimate_tokens(request).unwrap(),
        }
    }

    fn assert_matches_full(estimate: &mut SessionEstimate, session: &mut Session, step: &str) {
        assert_eq!(
            estimate.tokens(session).unwrap(),
            full_tokens(session),
            "{step}"
        );
    }

    fn append_assistant(session: &mut Session, text: &str) {
        match session {
            Session::Completion(request) => request.messages.push(
                serde_json::from_value(json!({"role": "assistant", "content": text})).unwrap(),
            ),
            Session::Response(request) => response_input(request).push(
                serde_json::from_value(json!({
                    "type": "message",
                    "id": "assistant",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": text, "annotations": []}]
                }))
                .unwrap(),
            ),
        }
    }

    fn append_image_input(session: &mut Session) {
        append_input_to(
            session,
            &UserInput {
                source: String::new(),
                display: String::new(),
                parts: vec![
                    Part::Text("read this screenshot".into()),
                    Part::Image(format!("data:image/png;base64,{}", "A".repeat(4_096)).into()),
                ],
                images: Vec::new(),
            },
        );
    }

    /// Tail rollback after a failed tool batch: the message count shrinks, so the cache must discard itself.
    fn pop_last(session: &mut Session) {
        match session {
            Session::Completion(request) => {
                request.messages.pop();
            }
            Session::Response(request) => {
                response_input(request).pop();
            }
        }
    }

    /// Same-length wholesale replacement by compact/tree: the count is unchanged, so only explicit invalidation reveals the swapped content.
    fn rewrite_history(session: &mut Session, protocol: Protocol) {
        match session {
            Session::Completion(request) => {
                let system = request.messages.remove(0);
                let count = request.messages.len();
                let mut messages = vec![system];
                for index in 0..count {
                    messages.push(
                        serde_json::from_value(json!({
                            "role": "user",
                            "content": format!("rewritten {protocol} history item {index}")
                        }))
                        .unwrap(),
                    );
                }
                request.messages = messages;
            }
            Session::Response(request) => {
                let count = response_input(request).len();
                response_input(request).clear();
                for index in 0..count {
                    response_input(request).push(
                        serde_json::from_value(json!({
                            "type": "message",
                            "id": format!("rewritten-{index}"),
                            "role": "assistant",
                            "status": "completed",
                            "content": [{
                                "type": "output_text",
                                "text": format!("rewritten {protocol} history item {index}"),
                                "annotations": []
                            }]
                        }))
                        .unwrap(),
                    );
                }
            }
        }
    }

    #[test]
    fn incremental_estimate_matches_full_serialization() {
        for protocol in [Protocol::Completion, Protocol::Response] {
            let mut session = new_session(protocol);
            let mut estimate = SessionEstimate::default();

            // An empty session has not sent context yet: 0 with only the system prompt and tool definitions.
            assert_eq!(
                estimate.tokens(&mut session).unwrap(),
                0,
                "{protocol} empty session"
            );

            // Append boundaries: user text, input with an image, assistant reply and developer injection.
            append_user_to(&mut session, "first question");
            assert_matches_full(&mut estimate, &mut session, "append user");
            append_image_input(&mut session);
            assert_matches_full(&mut estimate, &mut session, "append image input");
            append_assistant(&mut session, "answer");
            assert_matches_full(&mut estimate, &mut session, "append assistant");
            append_developer_to(&mut session, "hook context".into());
            assert_matches_full(&mut estimate, &mut session, "append developer");

            // Tail rollback: when the count shrinks the cache drops the old accumulation and recomputes.
            pop_last(&mut session);
            assert_matches_full(&mut estimate, &mut session, "roll back tail");

            // compact/tree: a wholesale replacement must be invalidated explicitly, never leaving stale values.
            let before = estimate.tokens(&mut session).unwrap();
            rewrite_history(&mut session, protocol);
            estimate.invalidate();
            let after = estimate.tokens(&mut session).unwrap();
            assert_eq!(after, full_tokens(&session), "{protocol} replaced history");
            assert_ne!(before, after, "{protocol} replacement changes the total");
        }
    }

    #[test]
    fn protocol_conversion_invalidates_the_accumulated_total() {
        let mut completion = new_session(Protocol::Completion);
        append_user_to(&mut completion, "question");
        append_assistant(&mut completion, "answer");
        let mut estimate = SessionEstimate::default();
        assert_matches_full(&mut estimate, &mut completion, "completion before switch");

        let mut response = history::convert(
            &completion,
            "fixture/model".into(),
            Protocol::Response,
            EditMode::Replace,
        )
        .expect("completion history converts to responses");
        estimate.invalidate();
        assert_matches_full(&mut estimate, &mut response, "converted to responses");

        let mut back = history::convert(
            &response,
            "fixture/model".into(),
            Protocol::Completion,
            EditMode::Replace,
        )
        .expect("responses history converts back to completion");
        estimate.invalidate();
        assert_matches_full(&mut estimate, &mut back, "converted back to completion");
    }

    #[test]
    fn text_input_keeps_the_full_estimate_without_caching() {
        let mut session = new_session(Protocol::Response);
        let mut estimate = SessionEstimate::default();
        let Session::Response(request) = &mut session else {
            unreachable!("fixture uses responses");
        };
        request.input = InputParam::Text(String::new());
        assert_eq!(estimate.tokens(&mut session).unwrap(), 0);

        let Session::Response(request) = &mut session else {
            unreachable!("fixture uses responses");
        };
        request.input = InputParam::Text("plain shared input".into());
        assert_matches_full(&mut estimate, &mut session, "text input");
    }

    #[test]
    fn calibrated_tokens_adjusts_within_sane_bounds_and_ignores_outliers() {
        let usage = |input_tokens| {
            Some(pp_core::context::TokenUsage {
                input_tokens,
                output_tokens: 50,
                cached_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            })
        };
        // Plain English (real tokens are about 0.75x of bytes/3): incremental calibration keeps the true baseline plus the new delta.
        assert_eq!(calibrate_tokens(12_000, usage(7_500), Some(10_000)), 9_500);
        // Dense CJK/symbols (real tokens above 1.5x of bytes/3): calibration revises upward so the threshold is not missed.
        assert_eq!(
            calibrate_tokens(11_000, usage(15_000), Some(10_000)),
            16_000
        );
        // Test stubs or anomalous tiny/huge reported values outside the sane [0.4, 3.0] range fall back to the raw estimate.
        assert_eq!(calibrate_tokens(12_000, usage(200), Some(10_000)), 12_000);
        assert_eq!(
            calibrate_tokens(12_000, usage(50_000), Some(10_000)),
            12_000
        );
        // Without server usage, return the raw estimate directly.
        assert_eq!(calibrate_tokens(12_000, None, Some(10_000)), 12_000);
    }
}
