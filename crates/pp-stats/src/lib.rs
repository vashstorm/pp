//! Request statistics kept separate from session logs; database failures do not change model call
//! results.
mod activity;
mod analytics;
mod cost;
mod database;
mod hook;
mod insights;
mod maintenance;
mod range;
mod row;
mod save;
mod schema;
pub mod server;
mod session;
mod store;
mod tool;

pub(crate) use cost::{CostSnapshot, RateSnapshot, Rates};
pub use database::Database;
pub use hook::{HookRun, HookStatus, hook};
pub use maintenance::GarbageCollection;
pub(crate) use maintenance::maintain;
pub(crate) use range::Range;
pub(crate) use row::{COLUMN_COUNT, COLUMNS, RequestRow, latest_sql, row};
pub use save::{link, save};
pub(crate) use schema::{ensure_column, ensure_schema, table_columns};
pub(crate) use session::{SessionScope, session_requests_sql};
pub use session::{SessionUsage, session_cost, session_usage};
pub(crate) use store::Store;
pub use tool::{ToolCall, ToolStatus, tool};
pub(crate) use tool::{call_tools, saved_tool_calls};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

#[derive(Clone)]
pub struct RequestContext {
    pub model_reference: String,
    /// Stats database handle: one connection is reused within the process, and a failed `Store::open`
    /// is not cached so the next attempt retries; production builds it from [`database_path`], while
    /// tests can point it at a temporary database to avoid writing the real HOME.
    pub database: Database,
    pub cwd: PathBuf,
    pub purpose: &'static str,
    pub session_file: Option<PathBuf>,
    /// Keeps the stats keys already written while no session file is assigned yet, so they can be
    /// linked afterwards.
    pub unlinked_requests: Option<Arc<Mutex<Vec<String>>>>,
    pub first_request: Arc<AtomicBool>,
    pub skills: Vec<pp_config::skills::SkillHeader>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct RequestSnapshot {
    pub request: Value,
    pub skills: Vec<pp_config::skills::SkillHeader>,
}

impl RequestContext {
    pub fn capture(&self, request: &impl Serialize) -> Option<RequestSnapshot> {
        if !self.first_request.swap(false, Ordering::Relaxed) {
            return None;
        }
        match serde_json::to_value(request) {
            Ok(request) => Some(RequestSnapshot {
                request,
                skills: self.skills.clone(),
            }),
            Err(_) => {
                warn();
                None
            }
        }
    }
}

pub struct Observation {
    pub timestamp: i64,
    pub ttft_ms: Option<u64>,
    /// Native model output; used only at the request boundary to extract tool calls and never stored
    /// wholesale.
    pub output: Value,
    pub stop_reason: Option<String>,
    pub usage: Option<pp_core::context::TokenUsage>,
    pub request_context: Option<RequestSnapshot>,
}
impl Observation {
    pub fn new() -> Self {
        Self {
            timestamp: chrono::Utc::now().timestamp_millis(),
            ttft_ms: None,
            output: Value::Null,
            stop_reason: None,
            usage: None,
            request_context: None,
        }
    }
    pub fn event(&mut self, event: &pp_core::events::AgentEvent, started: Instant) {
        if let pp_core::events::AgentEvent::ModelOutputProgress { output_bytes } = event
            && *output_bytes > 0
        {
            self.ttft_ms
                .get_or_insert(started.elapsed().as_millis() as u64);
        }
    }
}

impl Default for Observation {
    fn default() -> Self {
        Self::new()
    }
}

/// Production stats database path: `$HOME/.pp/stats/stats.db`.
///
/// Collection entry points never call it directly — `main::run` uses it to build the [`Database`]
/// handle and passes that down through [`RequestContext`] and the `link`/`tool`/`hook` arguments,
/// which lets tests confine writes to a temporary directory.
pub fn database_path() -> Result<PathBuf> {
    let agent = pp_config::config::global_dir()?;
    Ok(agent
        .parent()
        .context("statistics directory unavailable")?
        .join("stats/stats.db"))
}

static WARNED: AtomicBool = AtomicBool::new(false);
pub fn warn() {
    tracing::warn!(event = "statistics_unavailable");
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("warning: statistics unavailable; model tasks are unaffected");
    }
}

#[cfg(test)]
mod fixtures {
    use super::{Rates, RequestRow, Store, ToolCall, ToolStatus};
    use anyhow::Result;
    use serde_json::{Value, json};
    use std::path::Path;

    pub(super) fn open_store(path: &Path) -> Result<Store> {
        Store::open_with_models(path, path.with_file_name("models.yml"))
    }
    pub(super) fn models_with_cost(cost: Value) -> String {
        json!({"providers":{"fixture": {"api_key":"secret-marker-fixture-key",
            "models":{"primary": {"model":"fixture-model","cost":cost}}}}})
        .to_string()
    }
    pub(super) fn fixture(key: &str, timestamp: i64) -> RequestRow {
        RequestRow {
            id: 0,
            request_key: key.into(),
            timestamp,
            model: "fixture-model".into(),
            provider: "fixture".into(),
            model_reference: "fixture/primary".into(),
            api: "responses".into(),
            project: "project".into(),
            cwd: "/tmp/project".into(),
            purpose: "task".into(),
            session_file: None,
            entry_id: None,
            duration_ms: 2000.0,
            ttft_ms: Some(500.0),
            input_tokens: Some(100),
            output_tokens: Some(20),
            cached_tokens: Some(80),
            cache_write_tokens: Some(0),
            reasoning_tokens: None,
            status: "ok".into(),
            stop_reason: Some("stop".into()),
            error_message: None,
            cost: None,
            rates: None,
        }
    }
    /// Writes the cost/rate snapshots that collection would write for the given rates.
    pub(super) fn snapshot_for(rates: &Rates, record: &mut RequestRow) {
        let (cost, rate) = rates.snapshot(
            &record.model_reference,
            &record.model,
            record.input_tokens,
            record.output_tokens,
            record.cached_tokens,
            record.cache_write_tokens,
        );
        record.cost = cost;
        record.rates = rate;
    }
    pub(super) fn tool_call<'a>(
        session: &'a Path,
        call_id: &'a str,
        tool: &'a str,
        status: ToolStatus,
        error: Option<&'a str>,
        timestamp: i64,
        duration_ms: f64,
    ) -> ToolCall<'a> {
        ToolCall {
            request_key: Some("one"),
            session_file: session,
            call_id,
            tool,
            status,
            error_message: error,
            timestamp,
            duration_ms,
            result_chars: None,
        }
    }
}
