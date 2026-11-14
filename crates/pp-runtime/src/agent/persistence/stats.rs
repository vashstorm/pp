use super::*;
use std::sync::atomic::Ordering;

/// `ToolFinished` 已确认、但最终回填模型的文本尚未确定的工具统计。
pub(super) struct PendingToolStat {
    tool: String,
    request_key: String,
    timestamp: i64,
    duration_ms: f64,
    status: pp_stats::ToolStatus,
    error: Option<String>,
}

impl SessionJournal {
    pub(in crate::agent) fn stats_context(
        &self,
        metadata: &Metadata<'_>,
        purpose: &'static str,
        skills: &[pp_config::skills::SkillHeader],
    ) -> pp_stats::RequestContext {
        pp_stats::RequestContext {
            database: self.stats_db.clone(),
            model_reference: metadata.model_reference.to_owned(),
            cwd: metadata.cwd.to_owned(),
            purpose,
            session_file: self.id().map(|id| self.dir.join(format!("{id}.jsonl"))),
            unlinked_requests: self.unlinked_stats(),
            first_request: self.first_request.clone(),
            skills: if self.first_request.load(Ordering::Relaxed) {
                skills.to_vec()
            } else {
                Vec::new()
            },
        }
    }
    pub(in crate::agent) fn stats_database(&self) -> &Path {
        &self.stats_db
    }
    pub(in crate::agent) fn unlinked_stats(&self) -> Option<Arc<Mutex<Vec<String>>>> {
        self.id().is_none().then(|| self.unlinked_stats.clone())
    }

    pub(in crate::agent) fn link_unassigned_stats(&self) {
        if let Some(id) = self.id() {
            let keys = std::mem::take(
                &mut *self
                    .unlinked_stats
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            );
            if !keys.is_empty() {
                let file = self.dir.join(format!("{id}.jsonl"));
                for key in keys {
                    pp_stats::link(&self.stats_db, &key, &file);
                }
            }
        }
    }

    pub(in crate::agent) fn track_stats(&mut self, key: Option<String>, success: bool) {
        self.link_unassigned_stats();
        // 工具执行记录只跟随最近一次实际保存的请求；失败或缺键时不能沿用旧请求。
        self.last_stats = key.clone();
        if !success {
            self.pending_stats = None;
        }
        if success && key.is_some() {
            self.pending_stats = key;
        }
    }

    /// 在 `ToolFinished` 边界冻结状态、错误与耗时；结果字符数要等最终
    /// 回填模型的文本确定后由 [`Self::record_tool_output`] 写入——
    /// post hook 的 `updatedToolOutput` 会改写模型看到的正文。
    pub(super) fn tool_finished(
        &mut self,
        tool: &str,
        call_id: &str,
        result: &serde_json::Value,
        duration: std::time::Duration,
    ) {
        let Some(key) = self.last_stats.clone() else {
            return;
        };
        if self.id().is_none() {
            return;
        }
        let Ok(timestamp) =
            now_ms().and_then(|ms| i64::try_from(ms).context("session clock overflow"))
        else {
            return;
        };
        let cancelled = result.get("cancelled").and_then(serde_json::Value::as_bool) == Some(true);
        let status = if cancelled {
            pp_stats::ToolStatus::Cancelled
        } else if result.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
            pp_stats::ToolStatus::Error
        } else {
            pp_stats::ToolStatus::Ok
        };
        self.pending_tool_stats.insert(
            call_id.to_owned(),
            PendingToolStat {
                tool: tool.to_owned(),
                request_key: key,
                timestamp,
                duration_ms: duration.as_secs_f64() * 1000.0,
                status,
                error: result
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            },
        );
    }

    /// 用最终回填模型的 JSON 文本记录结果字符数；未调用则保持不写库。
    pub(in crate::agent) fn record_tool_output(&mut self, call_id: &str, output: &str) {
        let Some(pending) = self.pending_tool_stats.remove(call_id) else {
            return;
        };
        let Some(id) = self.id() else {
            return;
        };
        let file = self.dir.join(format!("{id}.jsonl"));
        pp_stats::tool(
            &self.stats_db,
            &pp_stats::ToolCall {
                request_key: Some(&pending.request_key),
                session_file: &file,
                call_id,
                tool: &pending.tool,
                status: pending.status,
                error_message: pending.error.as_deref(),
                timestamp: pending.timestamp,
                duration_ms: pending.duration_ms,
                result_chars: Some(output.chars().count() as u64),
            },
        );
    }
}
