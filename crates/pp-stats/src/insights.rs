use super::{COLUMNS, CostSnapshot, Range, RequestRow, Store, analytics::Aggregate, row};
use anyhow::{Context, Result};
use chrono::{Local, TimeZone};
use rusqlite::params;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn bucket(timestamp: i64, range: Range) -> Result<i64> {
    Ok(match range {
        Range::Hour => timestamp.div_euclid(300_000) * 300_000,
        Range::Day => timestamp.div_euclid(3_600_000) * 3_600_000,
        _ => Local
            .timestamp_millis_opt(timestamp)
            .single()
            .and_then(|d| d.date_naive().and_hms_opt(0, 0, 0))
            .and_then(|d| d.and_local_timezone(Local).earliest())
            .context("invalid local date")?
            .timestamp_millis(),
    })
}

#[derive(Default)]
struct Project {
    totals: Aggregate,
    buckets: BTreeMap<i64, Aggregate>,
    models: BTreeMap<String, (String, String, Aggregate)>,
    sessions: BTreeSet<String>,
    no_session: u64,
    recent: Vec<Value>,
    name: String,
}
impl Project {
    fn push(&mut self, r: &RequestRow, cost: Option<&CostSnapshot>, timestamp: i64) -> Result<()> {
        self.name.clone_from(&r.project);
        self.totals.push(r, cost);
        self.buckets.entry(timestamp).or_default().push(r, cost);
        self.models
            .entry(r.model_reference.clone())
            .or_insert_with(|| (r.model.clone(), r.provider.clone(), Aggregate::default()))
            .2
            .push(r, cost);
        if let Some(session) = r.session_file.as_ref().filter(|s| !s.is_empty()) {
            self.sessions.insert(session.clone());
        } else {
            self.no_session += 1;
        }
        if self.recent.len() < 10 {
            self.recent.push(r.value()?);
        }
        Ok(())
    }
    fn value(&self, key: &str) -> Value {
        let mut v = self.totals.value();
        v["key"] = json!(key);
        v["project"] = json!(self.name);
        v["sessions"] = json!(self.sessions.len());
        v["requests_without_session"] = json!(self.no_session);
        v["recent"] = json!(self.recent);
        v["buckets"] = json!(
            self.buckets
                .iter()
                .map(|(t, a)| {
                    let mut b = a.value();
                    b["timestamp"] = json!(t);
                    b
                })
                .collect::<Vec<_>>()
        );
        v["models"] = json!(
            self.models
                .iter()
                .map(|(key, (name, provider, a))| {
                    let mut m = a.value();
                    m["key"] = json!(key);
                    m["model"] = json!(name);
                    m["provider"] = json!(provider);
                    m
                })
                .collect::<Vec<_>>()
        );
        v
    }
}

#[derive(Default)]
struct Tool {
    calls: u64,
    args: u64,
    tokens: Option<f64>,
    output: Option<f64>,
    usage_known: u64,
    cost: f64,
    priced: u64,
    last: i64,
    buckets: BTreeMap<i64, u64>,
    recent: Vec<Value>,
}
impl Tool {
    fn push(
        &mut self,
        r: &RequestRow,
        args_chars: u64,
        count: usize,
        timestamp: i64,
        cost: Option<f64>,
    ) {
        self.calls += 1;
        self.args += args_chars;
        self.last = self.last.max(r.timestamp);
        *self.buckets.entry(timestamp).or_default() += 1;
        if let Some((i, o)) = r.input_tokens.zip(r.output_tokens) {
            self.tokens = Some(self.tokens.unwrap_or(0.0) + (i as f64 + o as f64) / count as f64);
            self.output = Some(self.output.unwrap_or(0.0) + o as f64 / count as f64);
            self.usage_known += 1;
        }
        if let Some(cost) = cost {
            self.cost += cost / count as f64;
            self.priced += 1;
        }
        if self.recent.len() < 10 && self.recent.last().is_none_or(|v| v["id"] != r.id) {
            self.recent.push(json!({"id":r.id,"model":r.model,"provider":r.provider,"timestamp":r.timestamp,"status":r.status,"stop_reason":r.stop_reason}));
        }
    }
    fn value(&self) -> Value {
        json!({"calls":self.calls,"args_chars":self.args,"tokens":if self.calls == 0 {Some(0.0)} else {self.tokens},"output_tokens":if self.calls == 0 {Some(0.0)} else {self.output},
            "usage_known":self.usage_known,"cost":if self.priced > 0 || self.calls == 0 {Some(self.cost)} else {None},
            "priced":self.priced,"unpriced":self.calls-self.priced,"last_seen":self.last,
            "errors":null,"cancelled":null,"executions":null,"result_chars":null,"avg_duration_ms":null,
            "buckets":self.buckets.iter().map(|(t, c)| json!({"timestamp":t,"requests":c})).collect::<Vec<_>>(),
            "recent":self.recent})
    }
}

/// 已采集的工具执行统计；`recorded == 0` 表示该范围内没有执行记录，不能当作零错误。
#[derive(Default)]
struct Execution {
    recorded: u64,
    errors: u64,
    cancelled: u64,
    duration: f64,
    error_buckets: BTreeMap<i64, u64>,
}
impl Execution {
    fn push(&mut self, status: &str, duration: f64, bucket: i64) {
        self.recorded += 1;
        self.duration += duration;
        match status {
            "error" => {
                self.errors += 1;
                *self.error_buckets.entry(bucket).or_default() += 1;
            }
            "cancelled" => self.cancelled += 1,
            _ => {}
        }
    }
    fn apply(&self, value: &mut Value) {
        if self.recorded == 0 {
            return;
        }
        value["executions"] = json!(self.recorded);
        value["errors"] = json!(self.errors);
        value["cancelled"] = json!(self.cancelled);
        value["avg_duration_ms"] = json!(self.duration / self.recorded as f64);
        value["error_buckets"] = json!(
            self.error_buckets
                .iter()
                .map(|(t, c)| json!({"timestamp":t,"errors":c}))
                .collect::<Vec<_>>()
        );
    }
}
impl Store {
    pub(super) fn projects(&self, range: Range, now: i64) -> Result<Value> {
        let mut statement = self.conn.prepare(&format!("SELECT {COLUMNS} FROM requests WHERE timestamp>=?1 AND timestamp<=?2 ORDER BY timestamp DESC,id DESC"))?;
        let rows = statement.query_map(params![range.since(now), now], row)?;
        let mut summary = Aggregate::default();
        let mut projects = BTreeMap::<String, Project>::new();
        for r in rows {
            let r = r?;
            let cost = r.cost();
            summary.push(&r, cost.as_ref());
            projects.entry(r.cwd.clone()).or_default().push(
                &r,
                cost.as_ref(),
                bucket(r.timestamp, range)?,
            )?;
        }
        Ok(
            json!({"summary":summary.value(),"projects":projects.iter().map(|(k, p)| p.value(k)).collect::<Vec<_>>(),
            "updated_at":now,"range":range.label()}),
        )
    }

    pub(super) fn tools(&self, range: Range, now: i64) -> Result<Value> {
        let columns = COLUMNS
            .split(',')
            .map(|c| format!("r.{c}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut statement = self.conn.prepare(&format!("SELECT {columns},t.tool,t.args_chars FROM requests r JOIN request_tools t ON t.request_id=r.id WHERE r.timestamp>=?1 AND r.timestamp<=?2 ORDER BY r.timestamp DESC,r.id DESC"))?;
        let rows = statement.query_map(params![range.since(now), now], |r| {
            Ok((row(r)?, r.get::<_, String>(23)?, r.get::<_, i64>(24)?))
        })?;
        let mut all = Tool::default();
        let mut tools = BTreeMap::<String, Tool>::new();
        let mut models = BTreeMap::<(String, String), (String, String, Tool)>::new();
        let mut requests = 0u64;
        let mut partial = 0u64;
        let mut requests_by_key = BTreeMap::<String, String>::new();
        // 同一请求的每个工具调用各占一行，且按 id 相邻排列；攒成一批后再按调用数均分 Token 与费用。
        let mut push_request = |r: RequestRow, calls: &[(String, u64)]| -> Result<()> {
            let count = calls.len();
            requests += 1;
            requests_by_key.insert(r.request_key.clone(), r.model_reference.clone());
            partial += u64::from(r.status != "ok");
            let timestamp = bucket(r.timestamp, range)?;
            let cost = r.cost().map(|c| c.total());
            for (name, args_chars) in calls {
                all.push(&r, *args_chars, count, timestamp, cost);
                tools.entry(name.clone()).or_default().push(
                    &r,
                    *args_chars,
                    count,
                    timestamp,
                    cost,
                );
                models
                    .entry((name.clone(), r.model_reference.clone()))
                    .or_insert_with(|| (r.model.clone(), r.provider.clone(), Tool::default()))
                    .2
                    .push(&r, *args_chars, count, timestamp, cost);
            }
            Ok(())
        };
        let mut pending: Option<(RequestRow, Vec<(String, u64)>)> = None;
        for result in rows {
            let (r, name, args_chars) = result?;
            let args_chars =
                u64::try_from(args_chars).context("tool argument length is negative")?;
            match pending.as_mut() {
                Some((current, calls)) if current.id == r.id => calls.push((name, args_chars)),
                _ => {
                    if let Some((current, calls)) = pending.take() {
                        push_request(current, &calls)?;
                    }
                    pending = Some((r, vec![(name, args_chars)]));
                }
            }
        }
        if let Some((current, calls)) = pending.take() {
            push_request(current, &calls)?;
        }
        let mut executions = BTreeMap::<String, Execution>::new();
        let mut model_executions = BTreeMap::<(String, String), Execution>::new();
        let mut execution_summary = Execution::default();
        let mut tool_statement = self.conn.prepare(
            "SELECT tool,request_key,status,duration_ms,timestamp FROM tool_calls WHERE timestamp>=?1 AND timestamp<=?2",
        )?;
        let mut tool_rows = tool_statement.query(params![range.since(now), now])?;
        while let Some(tool_row) = tool_rows.next()? {
            let tool: String = tool_row.get(0)?;
            let key: Option<String> = tool_row.get(1)?;
            let status: String = tool_row.get(2)?;
            let duration: f64 = tool_row.get(3)?;
            let slot = bucket(tool_row.get(4)?, range)?;
            execution_summary.push(&status, duration, slot);
            executions
                .entry(tool.clone())
                .or_default()
                .push(&status, duration, slot);
            if let Some(reference) = key.as_ref().and_then(|key| requests_by_key.get(key)) {
                model_executions
                    .entry((tool, reference.clone()))
                    .or_default()
                    .push(&status, duration, slot);
            }
        }
        let tools: Vec<_> = tools
            .iter()
            .map(|(name, t)| {
                let mut v = t.value();
                if let Some(execution) = executions.get(name.as_str()) {
                    execution.apply(&mut v);
                }
                v["key"] = json!(name);
                v["tool"] = json!(name);
                v["models"] = json!(
                    models
                        .range((name.clone(), String::new())..)
                        .take_while(|((tool, _), _)| tool == name)
                        .map(|((_, key), (model, provider, t))| {
                            let mut m = t.value();
                            if let Some(execution) =
                                model_executions.get(&(name.clone(), key.clone()))
                            {
                                execution.apply(&mut m);
                            }
                            m["key"] = json!(key);
                            m["model"] = json!(model);
                            m["provider"] = json!(provider);
                            m
                        })
                        .collect::<Vec<_>>()
                );
                v
            })
            .collect();
        let mut summary = all.value();
        execution_summary.apply(&mut summary);
        Ok(
            json!({"summary":summary,"tools":tools,"requests_with_tools":requests,"partial_requests":partial,
            "updated_at":now,"range":range.label()}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use crate::Rates;
    use std::fs;
    use std::path::Path;

    #[test]
    fn stats_insights_count_both_protocols_without_double_counting_or_leaking_payloads() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("models.yml"),
            models_with_cost(json!({"input":2.0,"cacheRead":0.2,"output":8.0})),
        )
        .unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let rates = Rates::load(&store.models_path);
        let completion = json!({"tool_calls":[
            {"function":{"name":"read","arguments":"private-arguments"}},
            {"function":{"name":"bash","arguments":"{}"}}
        ]});
        let mut first = fixture("one", now - 1000);
        first.session_file = Some("/missing/session.jsonl".into());
        snapshot_for(&rates, &mut first);
        store.insert(&first, &completion).unwrap();
        store
            .link("one", Path::new("/missing/session.jsonl"))
            .unwrap();
        let responses = json!([{"type":"function_call","name":"read","arguments":"{}"}]);
        for n in 0..501 {
            let mut r = fixture(&format!("response-{n}"), now);
            r.api = "responses".into();
            r.cwd = "/other/project".into();
            r.model_reference = "other/primary".into();
            r.input_tokens = None;
            r.output_tokens = None;
            r.cached_tokens = None;
            snapshot_for(&rates, &mut r);
            store.insert(&r, &responses).unwrap();
        }
        let mut failed = fixture("failed", now);
        failed.status = "aborted".into();
        snapshot_for(&rates, &mut failed);
        store
            .insert(
                &failed,
                &json!({"output":[{"type":"function_call","name":"rg","arguments":"中😀"}]}),
            )
            .unwrap();
        store
            .insert(&fixture("future", now + 1), &responses)
            .unwrap();
        store
            .insert(&fixture("old", now - 3_600_001), &responses)
            .unwrap();
        let tools = store.tools(Range::Hour, now).unwrap();
        assert_eq!(tools["summary"]["calls"], 504);
        assert_eq!(tools["requests_with_tools"], 503);
        assert_eq!(tools["partial_requests"], 1);
        assert_eq!(tools["summary"]["priced"], 3);
        assert_eq!(tools["summary"]["unpriced"], 501);
        assert_eq!(tools["summary"]["tokens"], 240.0);
        assert!((tools["summary"]["cost"].as_f64().unwrap() - 0.000432).abs() < 1e-12);
        let read = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["tool"] == "read")
            .unwrap();
        assert_eq!(read["calls"], 502);
        assert_eq!(read["tokens"], 60.0);
        assert_eq!(read["models"].as_array().unwrap().len(), 2);
        assert!(tools["summary"]["errors"].is_null());
        assert!(!tools.to_string().contains("private-arguments"));
        assert!(!tools.to_string().contains("private-output"));
        let projects = store.projects(Range::Hour, now).unwrap();
        assert_eq!(projects["summary"]["requests"], 503);
        assert_eq!(projects["projects"].as_array().unwrap().len(), 2);
        let p = projects["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["key"] == "/tmp/project")
            .unwrap();
        assert_eq!(p["sessions"], 1);
        assert_eq!(p["requests_without_session"], 1);
        assert_eq!(p["requests"], 2);
        assert!(!projects.to_string().contains("private-output"));
        let empty = store.tools(Range::Hour, now - 7_200_000).unwrap();
        assert_eq!(empty["summary"]["cost"], 0.0);
        assert_eq!(empty["summary"]["tokens"], 0.0);
        assert!(empty["tools"].as_array().unwrap().is_empty());
    }
}
