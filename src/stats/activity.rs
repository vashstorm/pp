//! 错误分组和会话请求时间线，只使用统计库中的记录，不重新读取会话文件。
use super::{
    COLUMNS, Range, RequestRow, Store,
    analytics::{Aggregate, Pricing},
    row,
};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;

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
        Ok(
            json!({"summary":summary(&rows.iter().collect::<Vec<_>>(),pricing),"signatures":groups,"models":by_model,"requests":requests,"has_more":rows.len()>limit,"limit":limit,"range":range.label(),"updated_at":now,"pricing_error":pricing.error}),
        )
    }

    pub(super) fn traces(
        &self,
        range: Range,
        now: i64,
        limit: usize,
        pricing: &Pricing,
    ) -> Result<Value> {
        let rows = self.conn.prepare(&format!("SELECT {COLUMNS} FROM requests WHERE timestamp>=?1 AND timestamp<=?2 ORDER BY timestamp DESC,id DESC"))?
            .query_map(params![range.since(now), now], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        // 未记录会话的请求各自成组，不能将所有摘要请求合成一个会话。
        let mut sessions = BTreeMap::<(Option<&str>, i64), Vec<&RequestRow>>::new();
        for r in &rows {
            sessions
                .entry((
                    r.session_file.as_deref(),
                    if r.session_file.is_none() { r.id } else { 0 },
                ))
                .or_default()
                .push(r);
        }
        let mut groups = sessions
            .values()
            .map(|group| {
                let mut value = summary(group, pricing);
                let r = group[0];
                value["id"] = json!(r.id);
                value["session_file"] = json!(r.session_file);
                value["project"] = json!(r.project);
                value["cwd"] = json!(r.cwd);
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

    pub(super) fn trace(&self, id: i64, now: i64, pricing: &Pricing) -> Result<Option<Value>> {
        let session: Option<Option<String>> = self
            .conn
            .query_row("SELECT session_file FROM requests WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        let Some(session) = session else {
            return Ok(None);
        };
        let rows = self.conn.prepare(&format!("SELECT {COLUMNS} FROM requests WHERE (?1 IS NOT NULL AND session_file=?1) OR (?1 IS NULL AND id=?2) ORDER BY timestamp,id"))?
            .query_map(params![session,id],row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        let requests = rows
            .iter()
            .map(|r| pricing.request(r))
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(
            json!({"id":id,"session_file":session,"project":rows[0].project,"cwd":rows[0].cwd,"summary":summary(&rows.iter().collect::<Vec<_>>(),pricing),"requests":requests,"updated_at":now,"pricing_error":pricing.error}),
        ))
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
