//! 上下文用量估算与会话累计输出的清理边界。
use super::{Agent, Session};
use anyhow::Result;
use async_openai::types::chat::ChatCompletionRequestMessage;
use async_openai::types::responses::InputParam;
use pp_core::context::{
    ContextUsage, MessageBytes, estimate_bytes, estimate_tokens, estimate_tokens_from_bytes,
};
use pp_core::events::AgentEvent;

/// 原生请求的增量字节记账：请求骨架（模型、工具定义等）+ 逐条消息。
///
/// 只有新增消息会逐条序列化；骨架每次重新估算，因此工具表、模型与请求配置的变化
/// 自动生效。整段替换历史（压缩、树导航、跨协议转换、恢复）必须 [`Self::invalidate`]，
/// 追加消息不需要——追加只影响尾部。
#[derive(Default)]
pub(in crate::agent) struct SessionEstimate {
    messages: MessageBytes,
    /// 计入用量的消息数：Completion 跳过 system／developer，Response 计全部 item。
    session_messages: u64,
    /// 缓存的请求骨架字节数；[`Self::invalidate`] 或骨架指纹变化时失效。
    skeleton: Option<(SkeletonKey, u64)>,
    /// 下一次记账必须丢弃全部累计并重算。
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
    /// 历史被整段替换后丢弃累计；下一次 [`Self::tokens`] 全量重算。
    pub(in crate::agent) fn invalidate(&mut self) {
        self.stale = true;
        self.skeleton = None;
    }

    /// 返回原生请求当前的估算 token 数；只序列化请求骨架与新增消息。
    ///
    /// 口径与 [`Agent::context_usage`] 完全相同：空会话（Completion 只有
    /// system／developer，Response 没有 input item）为 0，其余按原生 JSON 字节数 / 3。
    pub(in crate::agent) fn tokens(&mut self, session: &mut Session) -> Result<u64> {
        let count = match &*session {
            Session::Completion(request) => request.messages.len(),
            Session::Response(request) => match &request.input {
                InputParam::Items(items) => items.len(),
                // 运行时只追加数组项；文本输入不进增量缓存，保持与全量估算一致。
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

    /// 消息数组为空时的请求字节数（含 `[]` 方括号）。
    ///
    /// 请求结构不可分片序列化，只能临时移出消息数组；移出与放回都是 O(1)，
    /// 序列化失败也先放回再返回错误。
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
        // 空会话尚未发送上下文；有对话后才计入系统提示和工具等完整请求开销。
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

    /// 与 [`Self::context_usage`] 同口径，但复用调用方持有的增量记账。
    ///
    /// `ContextUpdated` 与自动压缩阈值取这一个数值，同一轮请求不再各自重算。
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

    /// 利用最近一次服务端真实上报的 `input_tokens` 与当时本地原始估算的差值，
    /// 对当前增量估算做有界校准，用于触发自动裁剪与自动压缩阈值判定。
    ///
    /// 若服务端上报值与原始估算比例落在物理合理区间 `[0.4, 3.0]` 之外（例如异常上报或
    /// 测试桩固定值），则直接回退到 `raw_tokens`，避免误触发或漏触发。
    pub(in crate::agent) fn calibrated_context_tokens(&self, raw_tokens: u64) -> u64 {
        calibrate_tokens(raw_tokens, self.last_request, self.last_request_raw_tokens)
    }

    /// 实时同步 `/setting` 修改的自动压缩策略与阈值（下一次请求与 `/context` 立即生效）。
    pub fn set_compaction_policy(&mut self, auto_compact: bool, threshold_percent: u8) {
        self.auto_compact = auto_compact;
        self.auto_compact_threshold_percent = threshold_percent;
    }

    /// 会话根变化（新建、恢复、树导航、Plan 交接）同时丢弃最近请求指标与
    /// 会话累计输出；切模型、effort 和压缩只清最近请求指标。
    pub(in crate::agent) fn clear_session_usage(&mut self) {
        self.last_request = None;
        self.last_request_raw_tokens = None;
        self.session_output_tokens = 0;
    }

    /// 恢复会话时从 `stats.db` 回填最近一次普通请求用量与会话累计输出。
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

    /// 与增量路径无关的全量参考值：整棵原生请求重新序列化一次。
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

    /// 工具批次失败后的尾部回滚：消息条数变少，缓存必须自行丢弃。
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

    /// compact／tree 的同长度整段替换：条数不变，只有显式失效才能发现内容已换。
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

            // 空会话尚未发送上下文：只有系统提示与工具定义时为 0。
            assert_eq!(
                estimate.tokens(&mut session).unwrap(),
                0,
                "{protocol} empty session"
            );

            // append 边界：用户文本、含图片的输入、assistant 回复与 developer 注入。
            append_user_to(&mut session, "first question");
            assert_matches_full(&mut estimate, &mut session, "append user");
            append_image_input(&mut session);
            assert_matches_full(&mut estimate, &mut session, "append image input");
            append_assistant(&mut session, "answer");
            assert_matches_full(&mut estimate, &mut session, "append assistant");
            append_developer_to(&mut session, "hook context".into());
            assert_matches_full(&mut estimate, &mut session, "append developer");

            // 尾部回滚：条数变少时缓存丢弃旧累计并重算。
            pop_last(&mut session);
            assert_matches_full(&mut estimate, &mut session, "roll back tail");

            // compact／tree：整段替换后必须显式失效，不得残留旧值。
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
        // 正常英文（真实 token 约为 bytes/3 的 0.75x）：增量校准保留真实基线 + 新增差值。
        assert_eq!(calibrate_tokens(12_000, usage(7_500), Some(10_000)), 9_500);
        // 密集 CJK/符号（真实 token 高于 bytes/3 的 1.5x）：校准向上修正避免漏触阈值。
        assert_eq!(
            calibrate_tokens(11_000, usage(15_000), Some(10_000)),
            16_000
        );
        // 测试桩或异常极小/极大上报值超出 [0.4, 3.0] 合理区间，回退原始估算。
        assert_eq!(calibrate_tokens(12_000, usage(200), Some(10_000)), 12_000);
        assert_eq!(
            calibrate_tokens(12_000, usage(50_000), Some(10_000)),
            12_000
        );
        // 未收到服务端用量时直接返回原始估算。
        assert_eq!(calibrate_tokens(12_000, None, Some(10_000)), 12_000);
    }
}
