//! Error grouping and session request timelines; uses only records in the stats database and never
//! re-reads session files.
use super::{
    COLUMNS, Range, RequestRow, SessionScope, Store, analytics::Aggregate, row,
    session_requests_sql,
};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Session titles are injected by the caller from the derived index in history.db; the key is the
/// session ID (session file name without extension) and an empty summary does not override the ID.
fn title<'a>(titles: &HashMap<&str, &'a str>, stem: Option<&str>) -> Option<&'a str> {
    let id = stem.and_then(|stem| stem.rsplit(['/', '\\']).next())?;
    titles
        .get(id)
        .copied()
        .filter(|title| !title.trim().is_empty())
}

/// Stem of the session file without the `.jsonl` suffix; a subagent file
/// `<root>.subagents/sessions/<id>.jsonl` folds into the root session, matching the directory
/// convention of `session_cost`.
fn session_stem(file: &str) -> &str {
    match file.find(".subagents/sessions/") {
        Some(index) => &file[..index],
        None => file.strip_suffix(".jsonl").unwrap_or(file),
    }
}

fn trace_session_key(file: &str) -> (bool, &str) {
    if file.ends_with(".jsonl") || file.contains(".subagents/sessions/") {
        (true, session_stem(file))
    } else {
        (false, file)
    }
}

fn summary(rows: &[&RequestRow]) -> Value {
    let mut aggregate = Aggregate::default();
    let mut models = BTreeMap::new();
    let mut end = 0.0_f64;
    for r in rows {
        aggregate.push(r, r.cost().as_ref());
        models.insert(
            &r.model_reference,
            json!({"key":r.model_reference,"model":r.model,"provider":r.provider}),
        );
        end = end.max(r.timestamp as f64 + r.duration_ms);
    }
    let mut value = aggregate.value();
    let start = value["first_seen"].as_i64().unwrap_or(0);
    value["wall_ms"] = json!((end - start as f64).max(0.0));
    value["model_ms"] = json!(rows.iter().map(|r| r.duration_ms).sum::<f64>());
    value["models"] = json!(models.values().collect::<Vec<_>>());
    value
}

/// Failed request list; `status='failed'`, the time range and the descending order are all served by
/// the partial index `requests_failed_time`. `?3` is `limit+1`: the extra row only tells whether an
/// older failed request exists.
pub(super) fn failed_requests_sql() -> String {
    format!(
        "SELECT {COLUMNS} FROM requests WHERE status='failed' AND timestamp>=?1 AND timestamp<=?2 ORDER BY timestamp DESC,id DESC LIMIT ?3"
    )
}

/// Tool executions for a Traces detail: the derived table unions two session-scoped `UNION ALL`
/// branches, then a `LEFT JOIN` fetches the request info. The outer column order must match the row
/// mapping: `t.id`, `t.request_key`, `t.session_file`, `t.call_id`, `t.tool`, `t.timestamp`,
/// `t.duration_ms`, `t.status`, `t.error_message`, `r.id`; the derived table lists the non-`r.*`
/// columns explicitly in the same order instead of a `SELECT *` that depends on table column order.
pub(super) fn trace_tools_sql() -> String {
    "SELECT t.id,t.request_key,t.session_file,t.call_id,t.tool,t.timestamp,t.duration_ms,t.status,t.error_message,r.id
                 FROM (SELECT id,request_key,session_file,call_id,tool,timestamp,duration_ms,status,error_message
                       FROM tool_calls WHERE session_file=?1
                       UNION ALL
                       SELECT id,request_key,session_file,call_id,tool,timestamp,duration_ms,status,error_message
                       FROM tool_calls WHERE session_file>=?2 AND session_file<?3) t
                 LEFT JOIN requests r ON r.request_key=t.request_key
                 ORDER BY t.timestamp,t.id"
        .into()
}

impl Store {
    /// Failed request list: the rows themselves are the page data; signature grouping, model
    /// distribution and statistics are all computed by the frontend from the loaded rows, independent
    /// of server-side aggregation; `limit+1` only detects whether an older failed request exists.
    pub(super) fn errors(&self, range: Range, now: i64, limit: usize) -> Result<Value> {
        let since = range.since(now);
        let rows = self
            .conn
            .prepare(&failed_requests_sql())?
            .query_map(params![since, now, limit as i64 + 1], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = rows.len() > limit;
        let requests = rows
            .iter()
            .take(limit)
            .map(RequestRow::value)
            .collect::<Result<Vec<_>>>()?;
        Ok(
            json!({"requests":requests,"has_more":has_more,"limit":limit,"range":range.label(),"updated_at":now}),
        )
    }

    pub(super) fn traces(
        &self,
        range: Range,
        now: i64,
        limit: usize,
        titles: &HashMap<&str, &str>,
    ) -> Result<Value> {
        let rows = self.conn.prepare(&format!("SELECT {COLUMNS} FROM requests WHERE timestamp>=?1 AND timestamp<=?2 ORDER BY timestamp DESC,id DESC"))?
            .query_map(params![range.since(now), now], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        // Subagent requests fold into the root session; requests without a recorded session form their
        // own group and must not be merged into one session.
        let mut tools = BTreeMap::<(bool, String), i64>::new();
        {
            let mut statement = self.conn.prepare("SELECT session_file,COUNT(*) FROM tool_calls WHERE timestamp>=?1 AND timestamp<=?2 GROUP BY session_file")?;
            let mut query = statement.query(params![range.since(now), now])?;
            while let Some(row) = query.next()? {
                let file: String = row.get(0)?;
                let (jsonl, stem) = trace_session_key(&file);
                *tools.entry((jsonl, stem.to_owned())).or_default() += row.get::<_, i64>(1)?;
            }
        }
        let mut sessions = BTreeMap::<(Option<(bool, &str)>, i64), Vec<&RequestRow>>::new();
        for r in &rows {
            sessions
                .entry((
                    r.session_file.as_deref().map(trace_session_key),
                    if r.session_file.is_none() { r.id } else { 0 },
                ))
                .or_default()
                .push(r);
        }
        let mut groups = sessions
            .iter()
            .map(|((key, _), group)| {
                let key = *key;
                let stem = key.map(|(_, s)| s);
                let mut value = summary(group);
                let r = group[0];
                let root = key.map(|(jsonl, s)| {
                    if jsonl {
                        format!("{s}.jsonl")
                    } else {
                        s.to_owned()
                    }
                });
                let agents = group
                    .iter()
                    .filter_map(|r| r.session_file.as_deref())
                    .filter(|file| root.as_deref() != Some(*file))
                    .collect::<BTreeSet<_>>()
                    .len();
                value["id"] = json!(r.id);
                if let Some(title) = title(titles, stem) {
                    value["title"] = json!(title);
                }
                value["session_file"] = json!(root);
                value["project"] = json!(r.project);
                value["cwd"] = json!(r.cwd);
                value["tool_calls"] = json!(
                    key.and_then(|(jsonl, s)| tools.get(&(jsonl, s.to_owned())))
                        .copied()
                        .unwrap_or(0)
                );
                value["agents"] = json!(agents);
                value
            })
            .collect::<Vec<_>>();
        groups.sort_by_key(|g| {
            std::cmp::Reverse((
                g["last_seen"].as_i64().unwrap_or(0),
                g["id"].as_i64().unwrap_or(0),
            ))
        });
        let count = groups.len();
        groups.truncate(limit);
        Ok(
            json!({"sessions":groups,"total":count,"has_more":count>limit,"limit":limit,"range":range.label(),"updated_at":now}),
        )
    }

    pub(super) fn trace(
        &self,
        id: i64,
        now: i64,
        titles: &HashMap<&str, &str>,
    ) -> Result<Option<Value>> {
        // A read transaction makes both queries see the same snapshot, so a concurrent gc that deletes
        // rows cannot leave them empty and panic.
        let tx = self.conn.unchecked_transaction()?;
        let session: Option<Option<String>> = tx
            .query_row("SELECT session_file FROM requests WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        let Some(session) = session else {
            return Ok(None);
        };
        let (stem, root, scope) = match session.as_deref() {
            Some(raw) if raw.ends_with(".jsonl") || raw.contains(".subagents/sessions/") => {
                let stem = session_stem(raw);
                let root = format!("{stem}.jsonl");
                let scope = SessionScope::new(root.clone(), format!("{stem}.subagents/sessions/"))?;
                (Some(stem), Some(root), Some(scope))
            }
            Some(raw) => (Some(raw), Some(raw.to_owned()), None),
            None => (None, None, None),
        };
        // With a session, the index range covers the root session and its dedicated subagent directory;
        // without a session or with a non-standard path, match the exact column instead.
        let (rows, tools) = match (&scope, &root) {
            (Some(scope), _) => {
                let rows = tx
                    .prepare(&format!(
                        "{} ORDER BY timestamp,id",
                        session_requests_sql(COLUMNS)
                    ))?
                    .query_map(params![scope.root, scope.prefix, scope.upper], row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let tools = tx
                    .prepare(&trace_tools_sql())?
                    .query_map(params![scope.root, scope.prefix, scope.upper], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, i64>(5)?,
                            r.get::<_, f64>(6)?,
                            r.get::<_, String>(7)?,
                            r.get::<_, Option<String>>(8)?,
                            r.get::<_, Option<i64>>(9)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                (rows, tools)
            }
            (None, Some(exact)) => {
                let rows = tx
                    .prepare(&format!(
                        "SELECT {COLUMNS} FROM requests WHERE session_file=?1 ORDER BY timestamp,id"
                    ))?
                    .query_map(params![exact], row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let tools = tx
                    .prepare(
                        "SELECT t.id,t.request_key,t.session_file,t.call_id,t.tool,t.timestamp,t.duration_ms,t.status,t.error_message,r.id
                         FROM tool_calls t
                         LEFT JOIN requests r ON r.request_key=t.request_key
                         WHERE t.session_file=?1
                         ORDER BY t.timestamp,t.id",
                    )?
                    .query_map(params![exact], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, i64>(5)?,
                            r.get::<_, f64>(6)?,
                            r.get::<_, String>(7)?,
                            r.get::<_, Option<String>>(8)?,
                            r.get::<_, Option<i64>>(9)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                (rows, tools)
            }
            (None, None) => (
                tx.prepare(&format!(
                    "SELECT {COLUMNS} FROM requests WHERE id=?1 ORDER BY timestamp,id"
                ))?
                .query_map(params![id], row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
                Vec::new(),
            ),
        };
        let Some(first) = rows.first() else {
            return Ok(None);
        };
        let requests = rows.iter().map(|r| r.value()).collect::<Result<Vec<_>>>()?;
        let mut tool_stats = BTreeMap::<&str, (usize, usize, f64, f64)>::new();
        for (_, _, _, _, tool, _, duration_ms, status, _, _) in &tools {
            let entry = tool_stats.entry(tool).or_default();
            entry.0 += 1;
            entry.1 += usize::from(status.as_str() == "error");
            entry.2 += *duration_ms;
            entry.3 = entry.3.max(*duration_ms);
        }
        let tool_stats = {
            let mut values = tool_stats
                .iter()
                .map(|(tool, (calls, errors, total_ms, max_ms))| {
                    json!({"tool":tool,"calls":calls,"errors":errors,"total_ms":total_ms,"max_ms":max_ms})
                })
                .collect::<Vec<_>>();
            values.sort_by(|a, b| {
                b["total_ms"]
                    .as_f64()
                    .unwrap_or(0.0)
                    .total_cmp(&a["total_ms"].as_f64().unwrap_or(0.0))
            });
            values
        };
        let tool_calls = tools
            .iter()
            .map(|(id, request_key, session_file, call_id, tool, timestamp, duration_ms, status, error_message, request_id)| {
                json!({"id":id,"request_key":request_key,"session_file":session_file,"call_id":call_id,
                    "tool":tool,"timestamp":timestamp,"duration_ms":duration_ms,"status":status,
                    "error_message":error_message,"request_id":request_id})
            })
            .collect::<Vec<_>>();
        let mut summary = summary(&rows.iter().collect::<Vec<_>>());
        let wall_ms = summary["wall_ms"].as_f64().unwrap_or(0.0);
        let model_ms = summary["model_ms"].as_f64().unwrap_or(0.0);
        let tool_ms = tools.iter().map(|tool| tool.6).sum::<f64>();
        let agents = rows
            .iter()
            .filter_map(|r| r.session_file.as_deref())
            .filter(|file| root.as_deref() != Some(*file))
            .collect::<BTreeSet<_>>()
            .len();
        summary["tool_ms"] = json!(tool_ms);
        summary["tool_calls"] = json!(tools.len());
        summary["idle_ms"] = json!((wall_ms - model_ms - tool_ms).max(0.0));
        summary["agents"] = json!(agents);
        let mut value = json!({"id":id,"session_file":root,"project":first.project,"cwd":first.cwd,"summary":summary,"requests":requests,"tool_calls":tool_calls,"tool_stats":tool_stats,"updated_at":now});
        if let Some(title) = title(titles, stem) {
            value["title"] = json!(title);
        }
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use crate::{ToolCall, ToolStatus, server};

    #[test]
    fn stats_traces_fold_subagents_and_aggregate_tools() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        let now = 10_000_000;
        let root = dir.path().join("root.jsonl");
        let child = dir.path().join("root.subagents/sessions/child.jsonl");
        let root_file = root.to_string_lossy().into_owned();
        let child_file = child.to_string_lossy().into_owned();
        for (key, file, timestamp, duration_ms) in [
            ("root-1", &root_file, now - 10_000, 2_000.0),
            ("child-1", &child_file, now - 5_000, 1_000.0),
            ("root-2", &root_file, now - 2_000, 3_000.0),
        ] {
            let mut record = fixture(key, timestamp);
            record.session_file = Some(file.clone());
            record.duration_ms = duration_ms;
            store.insert(&record, &Value::Null).unwrap();
        }
        for (request_key, file, call_id, status, error, timestamp, duration_ms) in [
            (
                "root-1",
                &root,
                "call-1",
                ToolStatus::Ok,
                None,
                now - 9_500,
                500.0,
            ),
            (
                "child-1",
                &child,
                "call-2",
                ToolStatus::Error,
                Some("boom"),
                now - 4_500,
                250.0,
            ),
        ] {
            store
                .tool(&ToolCall {
                    request_key: Some(request_key),
                    session_file: file,
                    call_id,
                    tool: "bash",
                    status,
                    error_message: error,
                    timestamp,
                    duration_ms,
                    result_chars: None,
                })
                .unwrap();
        }
        let titles = HashMap::from([("root", "Fix traces session titles")]);
        let traces = store.traces(Range::Hour, now, 500, &titles).unwrap();
        assert_eq!(traces["total"], 1);
        let session = &traces["sessions"][0];
        assert_eq!(session["session_file"], root_file);
        assert_eq!(session["title"], "Fix traces session titles");
        assert_eq!(session["requests"], 3);
        assert_eq!(session["agents"], 1);
        assert_eq!(session["tool_calls"], 2);
        let untitled = store
            .traces(Range::Hour, now, 500, &HashMap::new())
            .unwrap();
        assert!(untitled["sessions"][0].get("title").is_none());
        let id = session["id"].as_i64().unwrap();
        let trace = store.trace(id, now, &titles).unwrap().unwrap();
        assert_eq!(trace["title"], "Fix traces session titles");
        assert_eq!(trace["session_file"], root_file);
        assert_eq!(trace["summary"]["requests"], 3);
        assert_eq!(trace["summary"]["agents"], 1);
        assert_eq!(trace["summary"]["tool_calls"], 2);
        assert_eq!(trace["summary"]["model_ms"], 6_000.0);
        assert_eq!(trace["summary"]["tool_ms"], 750.0);
        assert_eq!(trace["summary"]["wall_ms"], 11_000.0);
        assert_eq!(trace["summary"]["idle_ms"], 4_250.0);
        assert_eq!(trace["requests"].as_array().unwrap().len(), 3);
        assert_eq!(trace["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(trace["tool_stats"][0]["tool"], "bash");
        assert_eq!(trace["tool_stats"][0]["calls"], 2);
        assert_eq!(trace["tool_stats"][0]["errors"], 1);
        assert_eq!(trace["tool_stats"][0]["total_ms"], 750.0);
        assert_eq!(trace["tool_stats"][0]["max_ms"], 500.0);
        assert_eq!(
            trace["tool_calls"][1]["request_id"],
            trace["requests"][1]["id"]
        );
    }
    #[test]
    fn stats_activity_uses_complete_ranges_and_full_session_traces() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let mut failed = fixture("old-failure", now - 7_200_000);
        failed.status = "failed".into();
        failed.error_message = Some("HTTP 429 request 0123456789abcdef after 12ms".into());
        failed.session_file = Some("/missing/session.jsonl".into());
        store
            .insert(&failed, &json!({"text":"private-output"}))
            .unwrap();
        for n in 0..501 {
            let mut r = fixture(&format!("failure-{n}"), now - 1000);
            r.status = "failed".into();
            r.error_message = Some("HTTP 429 request fedcba9876543210 after 99ms".into());
            store.insert(&r, &Value::Null).unwrap();
        }
        let mut latest = fixture("latest", now);
        latest.session_file = failed.session_file.clone();
        store.insert(&latest, &Value::Null).unwrap();
        store
            .insert(&fixture("future", now + 1), &Value::Null)
            .unwrap();
        // Failed rows are fetched with `limit+1`; the extra row only tells whether an older failed
        // request exists.
        let errors = store.errors(Range::Day, now, 200).unwrap();
        assert_eq!(errors["requests"].as_array().unwrap().len(), 200);
        assert_eq!(errors["limit"], 200);
        assert_eq!(errors["has_more"], true);
        assert!(!errors.to_string().contains("private-output"));
        let all = store.errors(Range::Day, now, 1000).unwrap();
        assert_eq!(all["requests"].as_array().unwrap().len(), 502);
        assert_eq!(all["has_more"], false);
        let traces = store
            .traces(Range::Hour, now, 500, &HashMap::new())
            .unwrap();
        assert_eq!(traces["total"], 502);
        assert_eq!(traces["sessions"][0]["requests"], 1);
        let id = traces["sessions"][0]["id"].as_i64().unwrap();
        let trace = store.trace(id, now, &HashMap::new()).unwrap().unwrap();
        assert_eq!(trace["summary"]["requests"], 2);
        assert_eq!(trace["requests"][0]["id"], 1);
        assert_eq!(trace["summary"]["wall_ms"], 7_202_000.0);
        assert_eq!(trace["summary"]["model_ms"], 4000.0);
        assert!(!trace.to_string().contains("private-output"));
        let detail = store.detail(1).unwrap().unwrap();
        assert!(detail.get("output_message").is_none());
        assert!(detail.get("session_entry").is_none());
        assert!(detail["request_context"].is_null());
        assert!(!detail.to_string().contains("private-output"));
        assert!(store.trace(9999, now, &HashMap::new()).unwrap().is_none());
        assert_eq!(
            store.trace(2, now, &HashMap::new()).unwrap().unwrap()["summary"]["requests"],
            1
        );
        let since = Range::Day.since(now);
        for (key, timestamp, status) in [
            ("before-range", since - 1, "failed"),
            ("at-lower", since, "failed"),
            ("at-upper-one", now, "failed"),
            ("at-upper-two", now, "failed"),
            ("after-range", now + 1, "failed"),
            ("in-range-ok", now - 500, "ok"),
            ("in-range-aborted", now - 500, "aborted"),
        ] {
            let mut record = fixture(key, timestamp);
            record.status = status.into();
            record.error_message = failed.error_message.clone();
            store.insert(&record, &Value::Null).unwrap();
        }
        let complete = store.errors(Range::Day, now, 1000).unwrap();
        let keys = complete["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["request_key"].as_str().unwrap())
            .collect::<Vec<_>>();
        let expected = ["at-upper-two".to_owned(), "at-upper-one".to_owned()]
            .into_iter()
            .chain((0..501).rev().map(|n| format!("failure-{n}")))
            .chain(["old-failure".to_owned(), "at-lower".to_owned()])
            .collect::<Vec<_>>();
        assert_eq!(keys, expected);
        assert_eq!(complete["has_more"], false);
        let limited = store.errors(Range::Day, now, 50).unwrap();
        assert_eq!(
            limited["requests"],
            json!(&complete["requests"].as_array().unwrap()[..50])
        );
        assert_eq!(limited["has_more"], true);
        for endpoint in ["errors", "traces"] {
            for query in [
                "range=bad",
                "range=all&range=24h",
                "limit=1",
                "unexpected=1",
            ] {
                assert_eq!(
                    server::dispatch(&store, &format!("/api/stats/{endpoint}?{query}"), &[]).0,
                    400
                );
            }
            // The Errors page loads in 50/200/1000 steps; the request log and Traces use 500/2000.
            let limit = if endpoint == "errors" { 1000 } else { 2000 };
            assert_eq!(
                server::dispatch(
                    &store,
                    &format!("/api/stats/{endpoint}?range=all&limit={limit}"),
                    &[]
                )
                .0,
                200
            );
        }
        assert_eq!(
            server::dispatch(&store, "/api/stats/traces/99999", &[]).0,
            404
        );
        assert_eq!(server::dispatch(&store, "/api/stats/traces/0", &[]).0, 400);
        assert_eq!(
            server::dispatch(&store, "/api/stats/traces/1?file=/tmp/private", &[]).0,
            400
        );

        // A session_file without the `.jsonl` suffix must not panic on out-of-bounds slicing.
        let mut custom = fixture("custom-session", now - 10);
        custom.session_file = Some("custom-session".into());
        store.insert(&custom, &Value::Null).unwrap();
        let custom_id: i64 = store
            .conn
            .query_row(
                "SELECT id FROM requests WHERE request_key='custom-session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let custom_trace = store
            .trace(custom_id, now, &HashMap::new())
            .unwrap()
            .unwrap();
        assert_eq!(custom_trace["session_file"], "custom-session");
        assert_eq!(custom_trace["requests"].as_array().unwrap().len(), 1);
        let custom_listed = store
            .traces(Range::Hour, now, 500, &HashMap::new())
            .unwrap()["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["session_file"] == "custom-session")
            .cloned()
            .expect("traces list must preserve non-.jsonl session_file verbatim");
        assert_eq!(custom_listed["agents"], 0);
    }
}
