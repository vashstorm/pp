//! 错误分组和会话请求时间线，只使用统计库中的记录，不重新读取会话文件。
use super::{
    COLUMNS, Range, RequestRow, SessionScope, Store, analytics::Aggregate, row,
    session_requests_sql,
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

/// 会话文件去掉 `.jsonl` 后的 stem；子 Agent 文件 `<root>.subagents/sessions/<id>.jsonl`
/// 归到根会话，与 `session_cost` 的目录约定一致。
fn session_stem(file: &str) -> &str {
    match file.find(".subagents/sessions/") {
        Some(index) => &file[..index],
        None => file.strip_suffix(".jsonl").unwrap_or(file),
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

/// 失败请求列表；`status='failed'` 与时间范围、倒序都由部分索引 `requests_failed_time` 满足。
/// `?3` 是 `limit+1`：多取一行只为判断是否还有更早的失败请求。
pub(super) fn failed_requests_sql() -> String {
    format!(
        "SELECT {COLUMNS} FROM requests WHERE status='failed' AND timestamp>=?1 AND timestamp<=?2 ORDER BY timestamp DESC,id DESC LIMIT ?3"
    )
}

/// Traces 详情的工具执行：派生表按会话作用域两段 `UNION ALL`，再用 `LEFT JOIN` 取请求信息。
/// 外层的列顺序必须与行映射一致：`t.id`、`t.request_key`、`t.session_file`、`t.call_id`、
/// `t.tool`、`t.timestamp`、`t.duration_ms`、`t.status`、`t.error_message`、`r.id`；
/// 派生表内显式列出同一顺序的非 `r.*` 列，不用依赖表列顺序的 `SELECT *`。
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
    /// 失败请求列表：行本身就是页面数据，签名分组、模型分布与统计量都由前端按已加载行计算，
    /// 与服务端聚合无关；`limit+1` 只用来判断是否存在更早的失败请求。
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
                let mut value = summary(group);
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
            json!({"sessions":groups,"total":count,"has_more":count>limit,"limit":limit,"range":range.label(),"updated_at":now}),
        )
    }

    pub(super) fn trace(
        &self,
        id: i64,
        now: i64,
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
        let scope = stem
            .map(|stem| {
                SessionScope::new(
                    format!("{stem}.jsonl"),
                    format!("{stem}.subagents/sessions/"),
                )
            })
            .transpose()?;
        // 有会话时按索引范围取根会话与专属子 Agent 目录；没有会话时只有当前这一行。
        let (rows, tools) = match &scope {
            Some(scope) => {
                let rows = self
                    .conn
                    .prepare(&format!(
                        "{} ORDER BY timestamp,id",
                        session_requests_sql(COLUMNS)
                    ))?
                    .query_map(params![scope.root, scope.prefix, scope.upper], row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let tools = self
                    .conn
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
            None => (
                self.conn
                    .prepare(&format!(
                        "SELECT {COLUMNS} FROM requests WHERE id=?1 ORDER BY timestamp,id"
                    ))?
                    .query_map(params![id], row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                Vec::new(),
            ),
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
        let mut value = json!({"id":id,"session_file":root,"project":rows[0].project,"cwd":rows[0].cwd,"summary":summary,"requests":requests,"tool_calls":tool_calls,"tool_stats":tool_stats,"updated_at":now});
        if let Some(title) = title(titles, stem) {
            value["title"] = json!(title);
        }
        Ok(Some(value))
    }
}
