use super::{Metadata, Session, SessionRef};
use crate::logging::DebugLog;
use async_openai::types::responses::InputParam;
use chrono::{DateTime, Local};
use serde::Serialize;
use std::io;

pub(super) struct DebugState {
    log: DebugLog,
    prompt_recorded: bool,
}

#[derive(Serialize)]
struct HarnessState<'a> {
    #[serde(serialize_with = "serialize_timestamp")]
    timestamp: DateTime<Local>,
    event: &'static str,
    pid: u32,
    status: &'static str,
    session_id: Option<&'a str>,
    model_reference: &'a str,
    protocol: &'static str,
    cwd: &'a std::path::Path,
    think_level: crate::config::ThinkLevel,
    edit_mode: crate::config::EditMode,
    tool_limits: crate::config::ToolLimits,
    history_items: usize,
    tool_count: usize,
}

#[derive(Serialize)]
struct FirstPrompt<'a> {
    #[serde(serialize_with = "serialize_timestamp")]
    timestamp: DateTime<Local>,
    event: &'static str,
    pid: u32,
    session_id: &'a str,
    model_reference: &'a str,
    #[serde(flatten)]
    prompt: SessionRef<'a>,
}

fn serialize_timestamp<S: serde::Serializer>(
    timestamp: &DateTime<Local>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&timestamp.format("%Y-%m-%dT%H:%M:%S%.3f%:z"))
}

impl DebugState {
    pub(super) fn new(enabled: bool, prompt_recorded: bool) -> Option<Self> {
        if !enabled {
            return None;
        }
        match DebugLog::new() {
            Ok(log) => Some(Self {
                log,
                prompt_recorded,
            }),
            Err(_) => {
                Self::warn_unavailable();
                None
            }
        }
    }

    pub(super) fn warn_unavailable() {
        tracing::warn!(
            event = "debug_logging_unavailable",
            pid = std::process::id()
        );
    }

    pub(super) fn request(
        &mut self,
        session: &Session,
        metadata: &Metadata<'_>,
        session_id: Option<&str>,
    ) -> io::Result<()> {
        let (protocol, history_items, tool_count) = match session {
            Session::Completion(request) => (
                "completion",
                request.messages.len(),
                request.tools.as_ref().map_or(0, Vec::len),
            ),
            Session::Response(request) => (
                "response",
                match &request.input {
                    InputParam::Items(items) => items.len(),
                    InputParam::Text(_) => 1,
                },
                request.tools.as_ref().map_or(0, Vec::len),
            ),
        };
        self.log.record(&HarnessState {
            timestamp: Local::now(),
            event: "harness_state",
            pid: std::process::id(),
            status: "request_ready",
            session_id,
            model_reference: metadata.model_reference,
            protocol,
            cwd: metadata.cwd,
            think_level: metadata.think_level,
            edit_mode: metadata.edit_mode,
            tool_limits: metadata.tool_limits,
            history_items,
            tool_count,
        })?;
        Ok(())
    }

    pub(super) fn set_prompt_recorded(&mut self, recorded: bool) {
        self.prompt_recorded = recorded;
    }

    pub(super) fn prompt(
        &mut self,
        session: SessionRef<'_>,
        metadata: &Metadata<'_>,
        session_id: &str,
    ) -> io::Result<()> {
        if !self.prompt_recorded {
            self.log.record_prompt(
                session_id,
                &FirstPrompt {
                    timestamp: Local::now(),
                    event: "llm_first_prompt",
                    pid: std::process::id(),
                    session_id,
                    model_reference: metadata.model_reference,
                    prompt: session,
                },
            )?;
            self.prompt_recorded = true;
        }
        Ok(())
    }
}
