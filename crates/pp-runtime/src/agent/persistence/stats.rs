use super::*;
use std::sync::atomic::Ordering;

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

    /// 工具执行明细写入统计；统计未启用（没有最近请求键）或会话未建立时不写库。
    pub(super) fn record_tool(
        &self,
        tool: &str,
        call_id: &str,
        result: &serde_json::Value,
        duration_ms: f64,
    ) {
        let (Some(id), Some(key)) = (self.id(), self.last_stats.as_deref()) else {
            return;
        };
        let Ok(timestamp) =
            now_ms().and_then(|ms| i64::try_from(ms).context("session clock overflow"))
        else {
            return;
        };
        // 与 UI 一致：只有显式 ok=false 才算失败，缺失视为成功；取消单独归类。
        let cancelled = result.get("cancelled").and_then(serde_json::Value::as_bool) == Some(true);
        let status = if cancelled {
            pp_stats::ToolStatus::Cancelled
        } else if result.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
            pp_stats::ToolStatus::Error
        } else {
            pp_stats::ToolStatus::Ok
        };
        let file = self.dir.join(format!("{id}.jsonl"));
        pp_stats::tool(
            &self.stats_db,
            &pp_stats::ToolCall {
                request_key: Some(key),
                session_file: &file,
                call_id,
                tool,
                status,
                error_message: result.get("error").and_then(serde_json::Value::as_str),
                timestamp,
                duration_ms,
            },
        );
    }
}
