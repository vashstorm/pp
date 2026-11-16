//! 上下文用量估算与会话累计输出的清理边界。
use super::{Agent, Session};
use anyhow::Result;
use async_openai::types::chat::ChatCompletionRequestMessage;
use async_openai::types::responses::InputParam;
use pp_core::context::{ContextUsage, estimate_tokens};
use pp_core::events::AgentEvent;

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
        Ok(ContextUsage {
            estimated_tokens,
            context_window: self.current_model.context_window,
            last_request: self.last_request,
            session_output_tokens: self.session_output_tokens,
            auto_compact: self.auto_compact,
            threshold_percent: self.auto_compact_threshold_percent,
        })
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
