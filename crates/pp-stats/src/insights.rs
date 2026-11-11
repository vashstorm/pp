use super::{
    COLUMNS, Range, RequestRow, Store,
    analytics::{Aggregate, Cost, Pricing},
    row,
};
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
    fn push(
        &mut self,
        r: &RequestRow,
        cost: Option<&Cost>,
        timestamp: i64,
        pricing: &Pricing,
    ) -> Result<()> {
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
            self.recent.push(pricing.request(r)?);
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
        args: &Value,
        count: usize,
        timestamp: i64,
        cost: Option<f64>,
    ) {
        self.calls += 1;
        self.args += match args {
            Value::String(s) => s.chars().count(),
            Value::Null => 0,
            v => v.to_string().chars().count(),
        } as u64;
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
// 优先使用已提交快照，避免同一调用在快照与原生输出中重复计数。
fn tool_calls<'a>(output: &'a Value, entry: &'a Value) -> Vec<(&'a str, &'a Value)> {
    let saved: Vec<_> = entry
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|e| e["message"]["content"].as_array().into_iter().flatten())
        .filter(|c| c["type"] == "toolCall")
        .filter_map(|c| Some((c["name"].as_str()?, &c["arguments"])))
        .collect();
    if !saved.is_empty() {
        return saved;
    }
    let native = &output["native"];
    if let Some(calls) = native["tool_calls"].as_array() {
        return calls
            .iter()
            .filter_map(|c| Some((c["function"]["name"].as_str()?, &c["function"]["arguments"])))
            .collect();
    }
    native
        .as_array()
        .or_else(|| native["output"].as_array())
        .into_iter()
        .flatten()
        .filter(|c| c["type"] == "function_call")
        .filter_map(|c| Some((c["name"].as_str()?, &c["arguments"])))
        .collect()
}

impl Store {
    pub(super) fn projects(&self, range: Range, now: i64, pricing: &Pricing) -> Result<Value> {
        let mut statement = self.conn.prepare(&format!("SELECT {COLUMNS} FROM requests WHERE timestamp>=?1 AND timestamp<=?2 ORDER BY timestamp DESC,id DESC"))?;
        let rows = statement.query_map(params![range.since(now), now], row)?;
        let mut summary = Aggregate::default();
        let mut projects = BTreeMap::<String, Project>::new();
        for r in rows {
            let r = r?;
            let cost = pricing.cost(&r);
            summary.push(&r, cost.as_ref());
            projects.entry(r.cwd.clone()).or_default().push(
                &r,
                cost.as_ref(),
                bucket(r.timestamp, range)?,
                pricing,
            )?;
        }
        Ok(
            json!({"summary":summary.value(),"projects":projects.iter().map(|(k, p)| p.value(k)).collect::<Vec<_>>(),
            "updated_at":now,"range":range.label(),"pricing_error":pricing.error}),
        )
    }

    pub(super) fn tools(&self, range: Range, now: i64, pricing: &Pricing) -> Result<Value> {
        let columns = COLUMNS
            .split(',')
            .map(|c| format!("r.{c}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut statement = self.conn.prepare(&format!("SELECT {columns},p.output_json,p.session_entry_json FROM requests r JOIN request_payloads p ON p.request_id=r.id WHERE r.timestamp>=?1 AND r.timestamp<=?2 ORDER BY r.timestamp DESC,r.id DESC"))?;
        let rows = statement.query_map(params![range.since(now), now], |r| {
            Ok((
                row(r)?,
                r.get::<_, String>("output_json")?,
                r.get::<_, Option<String>>("session_entry_json")?,
            ))
        })?;
        let mut all = Tool::default();
        let mut tools = BTreeMap::<String, Tool>::new();
        let mut models = BTreeMap::<(String, String), (String, String, Tool)>::new();
        let mut requests = 0u64;
        let mut partial = 0u64;
        let mut requests_by_key = BTreeMap::<String, String>::new();
        for result in rows {
            let (r, output, entry) = result?;
            let output: Value = serde_json::from_str(&output)?;
            let entry: Value = entry
                .map(|s| serde_json::from_str(&s))
                .transpose()?
                .unwrap_or(Value::Null);
            let calls = tool_calls(&output, &entry);
            let count = calls.len();
            if count == 0 {
                continue;
            }
            requests += 1;
            requests_by_key.insert(r.request_key.clone(), r.model_reference.clone());
            partial += u64::from(r.status != "ok");
            let timestamp = bucket(r.timestamp, range)?;
            let cost = pricing.cost(&r).map(|c| c.total());
            for (name, args) in calls {
                all.push(&r, args, count, timestamp, cost);
                tools
                    .entry(name.into())
                    .or_default()
                    .push(&r, args, count, timestamp, cost);
                models
                    .entry((name.into(), r.model_reference.clone()))
                    .or_insert_with(|| (r.model.clone(), r.provider.clone(), Tool::default()))
                    .2
                    .push(&r, args, count, timestamp, cost);
            }
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
            "updated_at":now,"range":range.label(),"pricing_error":pricing.error}),
        )
    }
}
