//! 错误分组和会话请求时间线，只使用统计库中的记录，不重新读取会话文件。
use super::{
    COLUMNS, Range, RequestRow, Store,
    analytics::{Aggregate, Pricing},
    row,
};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// 会话标题由调用方从 history.db 的派生索引注入；键为会话 ID（会话文件名去扩展名），
/// 空摘要不覆盖会话 ID。
fn title<'a>(titles: &HashMap<&str, &'a str>, stem: Option<&str>) -> Option<&'a str> {
    let id = stem.and_then(|stem| stem.rsplit(['/', '\\']).next())?;
    titles
        .get(id)
        .copied()
        .filter(|title| !title.trim().is_empty())
}

fn signature(message: &str) -> String {
    let mut result = String::new();
    let mut token = String::new();
    let flush = |token: &mut String, result: &mut String| {
        if token.len() >= 16 && token.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            result.push_str("<id>");
        } else {
            let mut number = false;
            for c in token.chars() {
                if c.is_ascii_digit() {
                    if !number {
                        result.push('N');
                    }
                    number = true;
                } else {
                    result.push(c);
                    number = false;
                }
            }
        }
        token.clear();
    };
    for c in message.chars() {
        if c.is_alphanumeric() || c == '-' {
            token.push(c);
        } else {
            flush(&mut token, &mut result);
            if c.is_whitespace() {
                if !result.ends_with(' ') {
                    result.push(' ');
                }
            } else {
                result.push(c);
            }
        }
    }
    flush(&mut token, &mut result);
    result.trim().to_owned()
}

/// 会话文件去掉 `.jsonl` 后的 stem；子 Agent 文件 `<root>.subagents/sessions/<id>.jsonl`
/// 归到根会话，与 `session_cost` 的目录约定一致。
fn session_stem(file: &str) -> &str {
    match file.find(".subagents/sessions/") {
        Some(index) => &file[..index],
        None => file.strip_suffix(".jsonl").unwrap_or(file),
    }
}

fn summary(rows: &[&RequestRow], pricing: &Pricing) -> Value {
    let mut aggregate = Aggregate::default();
    let mut models = BTreeMap::new();
    let mut end = 0.0_f64;
    for r in rows {
        aggregate.push(r, pricing.cost(r).as_ref());
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

impl Store {
    pub(super) fn errors(
        &self,
        range: Range,
        now: i64,
        limit: usize,
        pricing: &Pricing,
    ) -> Result<Value> {
        let rows = self.conn.prepare(&format!("SELECT {COLUMNS} FROM requests WHERE status='failed' AND timestamp>=?1 AND timestamp<=?2 ORDER BY timestamp DESC,id DESC"))?
            .query_map(params![range.since(now), now], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut signatures = BTreeMap::<String, Vec<&RequestRow>>::new();
        let mut models = BTreeMap::<&str, Vec<&RequestRow>>::new();
        for r in &rows {
            signatures
                .entry(signature(
                    r.error_message.as_deref().unwrap_or("Unknown error"),
                ))
                .or_default()
                .push(r);
            models.entry(&r.model_reference).or_default().push(r);
        }
        let mut groups = signatures
            .iter()
            .map(|(key, group)| {
                let mut value = summary(group, pricing);
                value["signature"] = json!(key);
                value["example"] = json!(group[0].error_message);
                value
            })
            .collect::<Vec<_>>();
        groups.sort_by_key(|g| {
            std::cmp::Reverse((
                g["requests"].as_u64().unwrap_or(0),
                g["last_seen"].as_i64().unwrap_or(0),
            ))
        });
        let mut by_model = models.iter().map(|(key, group)| json!({"key":key,"model":group[0].model,"provider":group[0].provider,"failures":group.len()})).collect::<Vec<_>>();
        by_model.sort_by_key(|g| std::cmp::Reverse(g["failures"].as_u64().unwrap_or(0)));
        let requests = rows
            .iter()
            .take(limit)
            .map(|r| {
                let mut value = pricing.request(r)?;
                value["signature"] = json!(signature(
                    r.error_message.as_deref().unwrap_or("Unknown error")
                ));
                Ok(value)
            })
            .collect::<Result<Vec<_>>>()?;
        let tools = self.tool_failures(range, now, limit)?;
        Ok(
            json!({"summary":summary(&rows.iter().collect::<Vec<_>>(),pricing),"signatures":groups,"models":by_model,"requests":requests,"has_more":rows.len()>limit,"limit":limit,"range":range.label(),"updated_at":now,"pricing_error":pricing.error,
                "tool_summary":tools["summary"],"tool_signatures":tools["signatures"],"tool_failures":tools["failures"],"tool_has_more":tools["has_more"],"tool_limit":limit}),
        )
    }

    /// 已采集的工具执行错误；请求关联只用于展示模型与目录，不改变工具结果。
    fn tool_failures(&self, range: Range, now: i64, limit: usize) -> Result<Value> {
        struct Failure {
            id: i64,
            tool: String,
            call_id: String,
            timestamp: i64,
            error_message: Option<String>,
            duration_ms: f64,
            request_id: Option<i64>,
            model: Option<String>,
            provider: Option<String>,
            model_reference: Option<String>,
            project: Option<String>,
            cwd: Option<String>,
        }
        let rows = self
            .conn
            .prepare(
                "SELECT t.id,t.tool,t.call_id,t.timestamp,t.error_message,t.duration_ms,
                 r.id,r.model,r.provider,r.model_reference,r.project,r.cwd
                 FROM tool_calls t LEFT JOIN requests r ON r.request_key=t.request_key
                 WHERE t.status='error' AND t.timestamp>=?1 AND t.timestamp<=?2
                 ORDER BY t.timestamp DESC,t.id DESC",
            )?
            .query_map(params![range.since(now), now], |r| {
                Ok(Failure {
                    id: r.get(0)?,
                    tool: r.get(1)?,
                    call_id: r.get(2)?,
                    timestamp: r.get(3)?,
                    error_message: r.get(4)?,
                    duration_ms: r.get(5)?,
                    request_id: r.get(6)?,
                    model: r.get(7)?,
                    provider: r.get(8)?,
                    model_reference: r.get(9)?,
                    project: r.get(10)?,
                    cwd: r.get(11)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut signatures = BTreeMap::<(String, String), Vec<&Failure>>::new();
        for failure in &rows {
            signatures
                .entry((
                    failure.tool.clone(),
                    signature(&format!(
                        "{}: {}",
                        failure.tool,
                        failure
                            .error_message
                            .as_deref()
                            .unwrap_or("Unknown tool error")
                    )),
                ))
                .or_default()
                .push(failure);
        }
        let mut groups = signatures
            .iter()
            .map(|((tool, signature), group)| {
                json!({"tool":tool,"signature":signature,"failures":group.len(),
                    "first_seen":group.iter().map(|f| f.timestamp).min(),
                    "last_seen":group.iter().map(|f| f.timestamp).max(),
                    "example":group[0].error_message})
            })
            .collect::<Vec<_>>();
        groups.sort_by_key(|g| {
            std::cmp::Reverse((
                g["failures"].as_u64().unwrap_or(0),
                g["last_seen"].as_i64().unwrap_or(0),
            ))
        });
        let tools = groups
            .iter()
            .filter_map(|g| g["tool"].as_str())
            .collect::<BTreeSet<_>>()
            .len();
        let failures = rows
            .iter()
            .take(limit)
            .map(|f| {
                json!({"id":f.id,"tool":f.tool,"call_id":f.call_id,"timestamp":f.timestamp,
                    "error_message":f.error_message,"duration_ms":f.duration_ms,
                    "request_id":f.request_id,"model":f.model,"provider":f.provider,
                    "model_reference":f.model_reference,"project":f.project,"cwd":f.cwd})
            })
            .collect::<Vec<_>>();
        Ok(
            json!({"summary":{"failures":rows.len(),"tools":tools,"signatures":groups.len(),
                "last_seen":rows.iter().map(|f| f.timestamp).max()},
                "signatures":groups,"failures":failures,"has_more":rows.len()>limit}),
        )
    }

    pub(super) fn traces(
        &self,
        range: Range,
        now: i64,
        limit: usize,
        pricing: &Pricing,
        titles: &HashMap<&str, &str>,
    ) -> Result<Value> {
        let rows = self.conn.prepare(&format!("SELECT {COLUMNS} FROM requests WHERE timestamp>=?1 AND timestamp<=?2 ORDER BY timestamp DESC,id DESC"))?
            .query_map(params![range.since(now), now], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        // 子 Agent 请求折叠进根会话；未记录会话的请求各自成组，不能合并成一个会话。
        let mut tools = BTreeMap::<String, i64>::new();
        {
            let mut statement = self.conn.prepare("SELECT session_file,COUNT(*) FROM tool_calls WHERE timestamp>=?1 AND timestamp<=?2 GROUP BY session_file")?;
            let mut query = statement.query(params![range.since(now), now])?;
            while let Some(row) = query.next()? {
                let file: String = row.get(0)?;
                *tools.entry(session_stem(&file).to_owned()).or_default() +=
                    row.get::<_, i64>(1)?;
            }
        }
        let mut sessions = BTreeMap::<(Option<&str>, i64), Vec<&RequestRow>>::new();
        for r in &rows {
            sessions
                .entry((
                    r.session_file.as_deref().map(session_stem),
                    if r.session_file.is_none() { r.id } else { 0 },
                ))
                .or_default()
                .push(r);
        }
        let mut groups = sessions
            .iter()
            .map(|((stem, _), group)| {
                let stem = *stem;
                let mut value = summary(group, pricing);
                let r = group[0];
                let root = stem.map(|stem| format!("{stem}.jsonl"));
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
                value["tool_calls"] =
                    json!(stem.and_then(|stem| tools.get(stem)).copied().unwrap_or(0));
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
            json!({"sessions":groups,"total":count,"has_more":count>limit,"limit":limit,"range":range.label(),"updated_at":now,"pricing_error":pricing.error}),
        )
    }

    pub(super) fn trace(
        &self,
        id: i64,
        now: i64,
        pricing: &Pricing,
        titles: &HashMap<&str, &str>,
    ) -> Result<Option<Value>> {
        let session: Option<Option<String>> = self
            .conn
            .query_row("SELECT session_file FROM requests WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        let Some(session) = session else {
            return Ok(None);
        };
        let stem = session.as_deref().map(session_stem);
        let root = stem.map(|stem| format!("{stem}.jsonl"));
        let children = stem.map(|stem| format!("{stem}.subagents/sessions/"));
        let (rows, tools) = match (&root, &children) {
            (Some(root), Some(children)) => {
                let rows = self.conn.prepare(&format!("SELECT {COLUMNS} FROM requests WHERE session_file=?1 OR substr(session_file,1,length(?2))=?2 ORDER BY timestamp,id"))?
                    .query_map(params![root, children], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
                let tools = self.conn.prepare("SELECT t.id,t.request_key,t.session_file,t.call_id,t.tool,t.timestamp,t.duration_ms,t.status,t.error_message,r.id
                     FROM tool_calls t LEFT JOIN requests r ON r.request_key=t.request_key
                     WHERE t.session_file=?1 OR substr(t.session_file,1,length(?2))=?2 ORDER BY t.timestamp,t.id")?
                    .query_map(params![root, children], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, i64>(5)?,
                            r.get::<_, f64>(6)?, r.get::<_, String>(7)?, r.get::<_, Option<String>>(8)?,
                            r.get::<_, Option<i64>>(9)?))
                    })?.collect::<rusqlite::Result<Vec<_>>>()?;
                (rows, tools)
            }
            _ => (
                self.conn
                    .prepare(&format!(
                        "SELECT {COLUMNS} FROM requests WHERE id=?1 ORDER BY timestamp,id"
                    ))?
                    .query_map(params![id], row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                Vec::new(),
            ),
        };
        let requests = rows
            .iter()
            .map(|r| pricing.request(r))
            .collect::<Result<Vec<_>>>()?;
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
        let mut summary = summary(&rows.iter().collect::<Vec<_>>(), pricing);
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
        let mut value = json!({"id":id,"session_file":root,"project":rows[0].project,"cwd":rows[0].cwd,"summary":summary,"requests":requests,"tool_calls":tool_calls,"tool_stats":tool_stats,"updated_at":now,"pricing_error":pricing.error});
        if let Some(title) = title(titles, stem) {
            value["title"] = json!(title);
        }
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn signatures_normalize_ids_and_counters() {
        assert_ne!(
            super::signature("HTTP 429 quota"),
            super::signature("HTTP 403 forbidden")
        );
        assert_eq!(super::signature("错误 123\n  请求"), "错误 N 请求");
    }
}
