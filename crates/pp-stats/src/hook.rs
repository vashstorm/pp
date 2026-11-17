use super::{Database, Range, Store, save::project_name, warn};
use anyhow::Result;
use pp_config::config::HookEvent;
use rusqlite::params;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// 生命周期 Hook 单次 handler 执行的采集状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookStatus {
    Ok,
    Blocked,
    Error,
}

impl HookStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Blocked => "blocked",
            Self::Error => "error",
        }
    }
}

/// 单次 Hook handler 执行记录；不持久化命令原文、参数或完整 stdin/stdout。
pub struct HookRun<'a> {
    pub session_file: &'a Path,
    pub cwd: &'a Path,
    pub event: HookEvent,
    pub tool: Option<&'a str>,
    pub status: HookStatus,
    pub modified: bool,
    pub timestamp: i64,
    pub duration_ms: f64,
}

/// 记录一次 Hook handler 执行；复用句柄内的连接，存储失败只提示，不改变 Hook 决策。
pub fn hook(database: &Database, run: &HookRun<'_>) {
    if database.with_store(|store| store.hook(run)).is_err() {
        warn();
    }
}

#[derive(Default)]
struct HookCounters {
    runs: u64,
    ok: u64,
    blocked: u64,
    errors: u64,
    modified: u64,
    total_duration_ms: f64,
    max_duration_ms: Option<f64>,
    first_seen: Option<i64>,
    last_seen: Option<i64>,
}

impl HookCounters {
    fn push(&mut self, status: &str, modified: bool, duration_ms: f64, timestamp: i64) {
        self.runs += 1;
        match status {
            "ok" => self.ok += 1,
            "blocked" => self.blocked += 1,
            "error" => self.errors += 1,
            _ => {}
        }
        self.modified += u64::from(modified);
        self.total_duration_ms += duration_ms;
        self.max_duration_ms = Some(
            self.max_duration_ms
                .map_or(duration_ms, |m| m.max(duration_ms)),
        );
        self.first_seen = Some(self.first_seen.map_or(timestamp, |t| t.min(timestamp)));
        self.last_seen = Some(self.last_seen.map_or(timestamp, |t| t.max(timestamp)));
    }

    fn value(&self) -> Value {
        let avg = (self.runs > 0).then(|| self.total_duration_ms / self.runs as f64);
        json!({
            "runs": self.runs,
            "ok": self.ok,
            "blocked": self.blocked,
            "errors": self.errors,
            "modified": self.modified,
            "total_duration_ms": self.total_duration_ms,
            "avg_duration_ms": avg,
            "max_duration_ms": self.max_duration_ms,
            "first_seen": self.first_seen,
            "last_seen": self.last_seen,
        })
    }
}

#[derive(Default)]
struct EventGroup {
    counters: HookCounters,
    sessions: BTreeSet<String>,
    tools: BTreeMap<String, HookCounters>,
}

#[derive(Default)]
struct ProjectGroup {
    name: String,
    counters: HookCounters,
    sessions: BTreeSet<String>,
    events: BTreeSet<String>,
}

impl Store {
    pub(crate) fn hook(&mut self, run: &HookRun<'_>) -> Result<()> {
        let project = project_name(run.cwd);
        self.conn.execute(
            "INSERT INTO hook_runs (timestamp,session_file,project,cwd,event,tool,status,modified,duration_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                run.timestamp,
                run.session_file.to_string_lossy(),
                project,
                run.cwd.to_string_lossy(),
                run.event.as_str(),
                run.tool,
                run.status.as_str(),
                i64::from(run.modified),
                run.duration_ms,
            ],
        )?;
        Ok(())
    }

    pub(super) fn hooks(&self, range: Range, now: i64) -> Result<Value> {
        let mut statement = self.conn.prepare(
            "SELECT timestamp,session_file,project,cwd,event,tool,status,modified,duration_ms
             FROM hook_runs
             WHERE timestamp>=?1 AND timestamp<=?2
             ORDER BY timestamp DESC,id DESC",
        )?;
        let mut rows = statement.query(params![range.since(now), now])?;
        let mut summary = HookCounters::default();
        let mut summary_sessions = BTreeSet::<String>::new();
        let mut buckets = BTreeMap::<i64, HookCounters>::new();
        let mut events = BTreeMap::<String, EventGroup>::new();
        let mut projects = BTreeMap::<String, ProjectGroup>::new();

        while let Some(row) = rows.next()? {
            let timestamp: i64 = row.get(0)?;
            let session_file: String = row.get(1)?;
            let project: String = row.get(2)?;
            let cwd: String = row.get(3)?;
            let event: String = row.get(4)?;
            let tool: Option<String> = row.get(5)?;
            let status: String = row.get(6)?;
            let modified = row.get::<_, i64>(7)? != 0;
            let duration_ms: f64 = row.get(8)?;

            let slot = range.bucket(timestamp)?;
            summary.push(&status, modified, duration_ms, timestamp);
            if !session_file.is_empty() {
                summary_sessions.insert(session_file.clone());
            }
            buckets
                .entry(slot)
                .or_default()
                .push(&status, modified, duration_ms, timestamp);

            let event_group = events.entry(event.clone()).or_default();
            event_group
                .counters
                .push(&status, modified, duration_ms, timestamp);
            if !session_file.is_empty() {
                event_group.sessions.insert(session_file.clone());
            }
            if let Some(tool_name) = tool {
                event_group.tools.entry(tool_name).or_default().push(
                    &status,
                    modified,
                    duration_ms,
                    timestamp,
                );
            }

            let proj_group = projects.entry(cwd).or_default();
            if proj_group.name.is_empty() {
                proj_group.name = project;
            }
            proj_group
                .counters
                .push(&status, modified, duration_ms, timestamp);
            if !session_file.is_empty() {
                proj_group.sessions.insert(session_file);
            }
            proj_group.events.insert(event);
        }

        let mut summary_value = summary.value();
        summary_value["sessions"] = json!(summary_sessions.len());
        summary_value["projects"] = json!(projects.len());

        let buckets_value: Vec<Value> = buckets
            .into_iter()
            .map(|(timestamp, b)| {
                let mut v = b.value();
                v["timestamp"] = json!(timestamp);
                v
            })
            .collect();

        let mut event_list: Vec<_> = events.into_iter().collect();
        event_list.sort_by(|a, b| {
            b.1.counters
                .runs
                .cmp(&a.1.counters.runs)
                .then(a.0.cmp(&b.0))
        });
        let events_value: Vec<Value> = event_list
            .into_iter()
            .map(|(name, group)| {
                let mut tool_list: Vec<_> = group.tools.into_iter().collect();
                tool_list.sort_by(|a, b| b.1.runs.cmp(&a.1.runs).then(a.0.cmp(&b.0)));
                let tools_value: Vec<Value> = tool_list
                    .into_iter()
                    .map(|(tool_name, counters)| {
                        let mut v = counters.value();
                        v["tool"] = json!(tool_name);
                        v
                    })
                    .collect();
                let mut v = group.counters.value();
                v["event"] = json!(name);
                v["sessions"] = json!(group.sessions.len());
                v["tools"] = json!(tools_value);
                v
            })
            .collect();

        let mut project_list: Vec<_> = projects.into_iter().collect();
        project_list.sort_by(|a, b| {
            b.1.counters
                .runs
                .cmp(&a.1.counters.runs)
                .then(a.0.cmp(&b.0))
        });
        let projects_value: Vec<Value> = project_list
            .into_iter()
            .map(|(key, group)| {
                let mut v = group.counters.value();
                v["key"] = json!(key);
                v["project"] = json!(group.name);
                v["sessions"] = json!(group.sessions.len());
                v["events"] = json!(group.events.len());
                v
            })
            .collect();

        Ok(json!({
            "summary": summary_value,
            "buckets": buckets_value,
            "events": events_value,
            "projects": projects_value,
            "updated_at": now,
            "range": range.label(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use crate::server;

    #[test]
    fn stats_hooks_aggregate_events_tools_buckets_and_projects_within_range() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("stats.db");
        let mut store = open_store(&db).unwrap();
        let now = 1_800_000_000_000;
        let alpha = Path::new("/work/alpha");
        let beta = Path::new("/work/beta");
        let s1 = Path::new("/sessions/s1.jsonl");
        let s2 = Path::new("/sessions/s2.jsonl");

        for (session_file, cwd, event, tool, status, modified, timestamp, duration_ms) in [
            (
                s1,
                alpha,
                HookEvent::UserPromptSubmit,
                None,
                HookStatus::Ok,
                true,
                now - 1000,
                12.0,
            ),
            (
                s1,
                alpha,
                HookEvent::PreToolUse,
                Some("bash"),
                HookStatus::Blocked,
                false,
                now - 500,
                28.0,
            ),
            (
                s1,
                alpha,
                HookEvent::PreToolUse,
                Some("read"),
                HookStatus::Ok,
                true,
                now - 300,
                10.0,
            ),
            (
                s2,
                beta,
                HookEvent::PostToolUse,
                Some("bash"),
                HookStatus::Error,
                false,
                now - 200,
                40.0,
            ),
            // 边界测试：恰好落在 1 小时下界（包含）与 1 小时外（排除）与未来（排除）
            (
                s2,
                beta,
                HookEvent::Stop,
                None,
                HookStatus::Ok,
                false,
                now - 3_600_000,
                10.0,
            ),
            (
                s2,
                beta,
                HookEvent::Stop,
                None,
                HookStatus::Blocked,
                false,
                now - 3_600_001,
                99.0,
            ),
            (
                s2,
                beta,
                HookEvent::Stop,
                None,
                HookStatus::Error,
                false,
                now + 1,
                99.0,
            ),
        ] {
            store
                .hook(&HookRun {
                    session_file,
                    cwd,
                    event,
                    tool,
                    status,
                    modified,
                    timestamp,
                    duration_ms,
                })
                .unwrap();
        }

        let data = store.hooks(Range::Hour, now).unwrap();
        assert_eq!(data["summary"]["runs"], 5);
        assert_eq!(data["summary"]["ok"], 3);
        assert_eq!(data["summary"]["blocked"], 1);
        assert_eq!(data["summary"]["errors"], 1);
        assert_eq!(data["summary"]["modified"], 2);
        assert_eq!(data["summary"]["sessions"], 2);
        assert_eq!(data["summary"]["projects"], 2);
        assert_eq!(data["summary"]["total_duration_ms"], 100.0);
        assert_eq!(data["summary"]["avg_duration_ms"], 20.0);
        assert_eq!(data["summary"]["max_duration_ms"], 40.0);

        let events = data["events"].as_array().unwrap();
        assert_eq!(events[0]["event"], "PreToolUse");
        assert_eq!(events[0]["runs"], 2);
        assert_eq!(events[0]["blocked"], 1);
        assert_eq!(events[0]["modified"], 1);
        assert_eq!(events[0]["tools"].as_array().unwrap().len(), 2);

        let projects = data["projects"].as_array().unwrap();
        assert_eq!(projects[0]["key"], "/work/alpha");
        assert_eq!(projects[0]["project"], "alpha");
        assert_eq!(projects[0]["runs"], 3);
        assert_eq!(projects[0]["events"], 2);

        let buckets = data["buckets"].as_array().unwrap();
        assert_eq!(
            buckets
                .iter()
                .map(|b| b["runs"].as_u64().unwrap())
                .sum::<u64>(),
            5
        );

        let (status, _, body) = server::dispatch(&store, "/api/stats/hooks?range=24h", &[]);
        assert_eq!(status, 200);
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["range"], "24h");
        assert!(parsed["events"].is_array());
        assert_eq!(
            server::dispatch(&store, "/api/stats/hooks?range=24h&limit=500", &[]).0,
            400
        );

        let empty = store.hooks(Range::Hour, now - 7_200_000).unwrap();
        assert_eq!(empty["summary"]["runs"], 0);
        assert!(empty["summary"]["avg_duration_ms"].is_null());
        assert!(empty["events"].as_array().unwrap().is_empty());
    }
}
