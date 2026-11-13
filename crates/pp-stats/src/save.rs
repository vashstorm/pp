use super::{Observation, Rates, RequestContext, RequestRow, Store, warn};
use anyhow::Result;
use std::path::Path;
use std::time::Instant;

pub fn save(
    context: &RequestContext,
    model: &str,
    api: &str,
    observation: Observation,
    started: Instant,
    outcome: &Result<impl Sized>,
) -> Option<String> {
    let result = (|| -> Result<String> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)
            .map_err(|_| anyhow::anyhow!("statistics random source unavailable"))?;
        let key = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let status = if outcome.is_ok() {
            "ok"
        } else if outcome
            .as_ref()
            .err()
            .is_some_and(|e| e.is::<pp_core::cancellation::Cancelled>())
        {
            "aborted"
        } else {
            "failed"
        };
        let mut record = RequestRow {
            id: 0,
            request_key: key.clone(),
            timestamp: observation.timestamp,
            model: model.into(),
            provider: context
                .model_reference
                .split('/')
                .next()
                .unwrap_or_default()
                .into(),
            model_reference: context.model_reference.clone(),
            api: api.into(),
            project: context
                .cwd
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into(),
            cwd: context.cwd.to_string_lossy().into(),
            purpose: context.purpose.into(),
            session_file: context
                .session_file
                .as_ref()
                .map(|p| p.to_string_lossy().into()),
            entry_id: None,
            duration_ms: started.elapsed().as_secs_f64() * 1000.0,
            ttft_ms: observation.ttft_ms.map(|n| n as f64),
            input_tokens: observation
                .usage
                .map(|u| i64::try_from(u.input_tokens))
                .transpose()?,
            output_tokens: observation
                .usage
                .map(|u| i64::try_from(u.output_tokens))
                .transpose()?,
            cached_tokens: observation
                .usage
                .and_then(|u| u.cached_tokens)
                .map(i64::try_from)
                .transpose()?,
            cache_write_tokens: observation
                .usage
                .and_then(|u| u.cache_write_tokens)
                .map(i64::try_from)
                .transpose()?,
            status: status.into(),
            stop_reason: observation.stop_reason,
            error_message: outcome.as_ref().err().map(|e| format!("{e:#}")),
            cost: None,
            rates: None,
        };
        let mut store = Store::open(&context.database)?;
        // 费用与费率在采集时按当次模型配置固定下来，读取路径不再回算。
        let (cost, rates) = Rates::load(&store.models_path).snapshot(
            &record.model_reference,
            &record.model,
            record.input_tokens,
            record.output_tokens,
            record.cached_tokens,
            record.cache_write_tokens,
        );
        record.cost = cost;
        record.rates = rates;
        store.insert_with_context(
            &record,
            &observation.output,
            observation.request_context.as_ref(),
        )?;
        if let Some(unlinked) = &context.unlinked_requests
            && context.session_file.is_none()
        {
            unlinked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(key.clone());
        }
        Ok(key)
    })();
    match result {
        Ok(key) => Some(key),
        Err(_) => {
            warn();
            None
        }
    }
}
pub fn link(database: &Path, key: &str, file: &Path) {
    if (|| -> Result<()> { Store::open(database)?.link(key, file) })().is_err() {
        warn();
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use crate::{Range, RequestSnapshot, ToolCall, ToolStatus, maintain, server};
    use rusqlite::params;
    use serde_json::{Value, json};

    #[test]
    fn stats_context_list_marks_saved_first_requests_without_loading_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        let snapshot = RequestSnapshot {
            request: json!({"instructions":"private-system-marker","unknown":{"preserved":true}}),
            skills: Vec::new(),
        };
        let mut first = fixture("first", 1000);
        first.status = "failed".into();
        store
            .insert_with_context(&first, &Value::Null, Some(&snapshot))
            .unwrap();
        store.insert(&fixture("later", 1001), &Value::Null).unwrap();
        store.insert(&fixture("legacy", 900), &Value::Null).unwrap();
        let (status, _, body) = server::dispatch(&store, "/api/stats/requests?range=all", &[]);
        assert_eq!(status, 200);
        let list: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            list["requests"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| (
                    r["request_key"].as_str().unwrap(),
                    r["has_context"].as_bool().unwrap()
                ))
                .collect::<Vec<_>>(),
            vec![("later", false), ("first", true), ("legacy", false)]
        );
        assert!(!body.contains("private-system-marker"));
        assert!(!body.contains("request_context"));
        assert!(!store.latest_with_context(1001, 2000, 1).unwrap()[0].1);
        assert!(!store.latest_with_context(0, 2000, 1).unwrap()[0].1);
        store
            .conn
            .execute_batch(
                "DELETE FROM request_contexts WHERE request_id=(SELECT id FROM requests WHERE request_key='first');
                 DELETE FROM requests WHERE request_key='first';",
            )
            .unwrap();
        assert!(
            store
                .latest_with_context(0, 2000, 500)
                .unwrap()
                .iter()
                .all(|(_, has_context)| !has_context)
        );
    }

    #[test]
    fn stats_context_tables_restore_and_snapshots_deduplicate_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        let mut store = open_store(&path).unwrap();
        let record = fixture("legacy", 1000);
        store
            .insert(
                &record,
                &json!({"tool_calls":[{"function":{"name":"read","arguments":"abc"}}]}),
            )
            .unwrap();
        store
            .link("legacy", Path::new("/missing/session.jsonl"))
            .unwrap();
        // 模拟没有快照表的旧库：重开时幂等建表，请求历史保持原样。
        store
            .conn
            .execute_batch("DROP TABLE request_contexts; DROP TABLE request_tools;")
            .unwrap();
        drop(store);
        let mut store = open_store(&path).unwrap();
        let old = store.detail(1).unwrap().unwrap();
        assert!(old.get("output_message").is_none());
        assert!(old.get("session_entry").is_none());
        assert!(old["request_context"].is_null());
        assert_eq!(old["request"]["request_key"], "legacy");
        assert_eq!(old["request"]["session_file"], "/missing/session.jsonl");
        assert_eq!(
            ["request_contexts", "request_tools"].map(|table| store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()),
            [1, 1]
        );
        let snapshot = RequestSnapshot {
            request: json!({"instructions":"full\n\t<script>&</script>","input":[{"type":"input_image","image_url":"data:image/png;base64,AAAA","future_field":{"nested":[1,null]}}],"tools":[{"type":"function","name":"read","parameters":{"additionalProperties":false}}],"unknown":{"preserved":true}}),
            skills: vec![pp_config::skills::SkillHeader {
                name: "available".into(),
                description: "header".into(),
                uri: "skill://available".into(),
            }],
        };
        let record = fixture("snapshot", 1001);
        store
            .insert_with_context(&record, &Value::Null, Some(&snapshot))
            .unwrap();
        store
            .insert_with_context(
                &record,
                &json!({"tool_calls":[{"function":{"name":"bash","arguments":"x"}}]}),
                None,
            )
            .unwrap();
        assert_eq!(store.latest(0, 2000, 500).unwrap().len(), 2);
        drop(store);
        let store = open_store(&path).unwrap();
        let detail = store.detail(2).unwrap().unwrap();
        assert_eq!(
            detail["request_context"],
            serde_json::to_value(&snapshot).unwrap()
        );
        assert!(detail.get("output_message").is_none());
        assert!(detail.get("session_entry").is_none());
        // 重复键的写入不会补写工具行或覆盖快照。
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM request_tools", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let (status, _, body) = server::dispatch(&store, "/api/stats/requests?range=all", &[]);
        assert_eq!(status, 200);
        assert!(!body.contains("request_context"));
        assert!(!body.contains("future_field"));
    }

    #[test]
    fn stats_request_tools_capture_chat_and_responses_calls() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        store
            .insert(
                &fixture("chat", 1000),
                &json!({"tool_calls":[
                    {"id":"call-1","function":{"name":"read","arguments":"abc"}},
                    {"id":"call-1","function":{"name":"bash","arguments":null}},
                    {"function":{"name":"edit","arguments":{"path":"a.rs"}}}
                ]}),
            )
            .unwrap();
        store
            .insert(
                &fixture("responses", 1001),
                &json!([
                    {"type":"function_call","name":"read","arguments":"中😀"},
                    {"type":"message","content":[]},
                    {"type":"function_call","name":"bash","arguments":"{}"}
                ]),
            )
            .unwrap();
        let mut statement = store
            .conn
            .prepare(
                "SELECT request_id,tool,args_chars FROM request_tools ORDER BY request_id,rowid",
            )
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                // 同一请求内不按调用 id 去重；null 参数为 0，对象按 JSON 文本计字符。
                (1, "read".to_owned(), 3),
                (1, "bash".to_owned(), 0),
                (1, "edit".to_owned(), 15),
                (2, "read".to_owned(), 2),
                (2, "bash".to_owned(), 2),
            ]
        );
    }

    #[test]
    fn stats_legacy_payloads_migrate_contexts_and_tools_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        store.insert(&fixture("saved", 1000), &Value::Null).unwrap();
        store
            .insert(&fixture("native", 1001), &Value::Null)
            .unwrap();
        // 手工还原旧库形态：request_payloads 保存原生输出包装与会话条目快照。
        store
            .conn
            .execute_batch(
                "CREATE TABLE request_payloads (
                    request_id INTEGER PRIMARY KEY REFERENCES requests(id), output_json TEXT NOT NULL,
                    session_entry_json TEXT, request_json TEXT, skills_json TEXT
                );",
            )
            .unwrap();
        let request_json = json!({"instructions":"first request","tools":[{"name":"read"}]});
        let skills_json = json!([{"name":"demo","description":"d","uri":"skill://demo"}]);
        let entry = json!([{
            "type":"message",
            "message":{"content":[{"type":"toolCall","name":"read","arguments":"abc"}]}
        }]);
        // 已提交会话条目里有 toolCall 时优先，原生输出不参与。
        let saved_output = json!({
            "native":{"tool_calls":[{"function":{"name":"ignored","arguments":"zzz"}}]},
            "text":"answer body","thinking":"thinking body","partial":null
        });
        store
            .conn
            .execute(
                "INSERT INTO request_payloads(request_id,output_json,session_entry_json,request_json,skills_json) VALUES (1,?1,?2,?3,?4)",
                params![
                    serde_json::to_string(&saved_output).unwrap(),
                    serde_json::to_string(&entry).unwrap(),
                    serde_json::to_string(&request_json).unwrap(),
                    serde_json::to_string(&skills_json).unwrap()
                ],
            )
            .unwrap();
        // 没有会话条目时从旧包装的 native 回退，参数按字符数计。
        let native_output = json!({
            "native":[{"type":"function_call","name":"grep","arguments":"中😀"}],
            "text":"answer body"
        });
        store
            .conn
            .execute(
                "INSERT INTO request_payloads(request_id,output_json,session_entry_json,request_json,skills_json) VALUES (2,?1,NULL,NULL,NULL)",
                params![serde_json::to_string(&native_output).unwrap()],
            )
            .unwrap();
        let contexts_before: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM request_payloads WHERE request_json IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(contexts_before, 1);
        let requests = |store: &Store| {
            let mut statement = store
                .conn
                .prepare(
                    "SELECT id,request_key,session_file,entry_id,status,timestamp FROM requests ORDER BY id",
                )
                .unwrap();
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let before = requests(&store);
        assert_eq!(before.len(), 2);

        maintain(&mut store.conn).unwrap();

        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='request_payloads'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        let contexts = {
            let mut statement = store
                .conn
                .prepare(
                    "SELECT request_id,request_json,skills_json FROM request_contexts ORDER BY request_id",
                )
                .unwrap();
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(contexts.len() as i64, contexts_before);
        assert_eq!(contexts[0].0, 1);
        assert_eq!(
            serde_json::from_str::<Value>(&contexts[0].1).unwrap(),
            request_json
        );
        assert_eq!(
            serde_json::from_str::<Value>(contexts[0].2.as_deref().unwrap()).unwrap(),
            skills_json
        );
        let tools = {
            let mut statement = store
                .conn
                .prepare(
                    "SELECT request_id,tool,args_chars FROM request_tools ORDER BY request_id,rowid",
                )
                .unwrap();
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(
            tools,
            vec![(1, "read".to_owned(), 3), (2, "grep".to_owned(), 2)]
        );
        assert_eq!(requests(&store), before);
        // 迁移结束恢复打开时的默认超时。
        assert_eq!(
            store
                .conn
                .query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            250
        );

        // 旧表已不存在：重复调用是空操作，不写任何行。
        maintain(&mut store.conn).unwrap();
        for (table, expected) in [("request_contexts", 1), ("request_tools", 2)] {
            assert_eq!(
                store
                    .conn
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                expected
            );
        }
        assert_eq!(requests(&store), before);
    }

    #[test]
    fn stats_tool_executions_record_errors_survive_table_rebuild_and_stay_in_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        let mut store = open_store(&path).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let session = dir.path().join("session.jsonl");
        let mut first = fixture("one", now - 1000);
        first.session_file = Some(session.to_string_lossy().into_owned());
        store
            .insert(
                &first,
                &json!({"tool_calls":[
                    {"function":{"name":"bash","arguments":"{}"}},
                    {"function":{"name":"read","arguments":"{}"}}
                ]}),
            )
            .unwrap();
        store
            .tool(&tool_call(
                &session,
                "c1",
                "bash",
                ToolStatus::Ok,
                None,
                now - 900,
                120.0,
            ))
            .unwrap();
        store
            .tool(&tool_call(
                &session,
                "c2",
                "bash",
                ToolStatus::Error,
                Some("exit status 1"),
                now - 800,
                240.0,
            ))
            .unwrap();
        store
            .tool(&tool_call(
                &session,
                "c3",
                "read",
                ToolStatus::Error,
                Some("No such file: /tmp/x"),
                now - 700,
                10.0,
            ))
            .unwrap();
        store
            .tool(&tool_call(
                &session,
                "c4",
                "read",
                ToolStatus::Cancelled,
                Some("Tool cancelled"),
                now - 600,
                5.0,
            ))
            .unwrap();
        // 同一会话的同一个 call ID 只保留最后一次结果。
        store
            .tool(&tool_call(
                &session,
                "c2",
                "bash",
                ToolStatus::Error,
                Some("exit status 2"),
                now - 800,
                300.0,
            ))
            .unwrap();
        // 范围外与无法关联请求的执行不能污染范围内的计数。
        store
            .tool(&ToolCall {
                request_key: None,
                session_file: &session,
                call_id: "old",
                tool: "bash",
                status: ToolStatus::Error,
                error_message: Some("old failure"),
                timestamp: Range::Day.since(now) - 1,
                duration_ms: 1.0,
            })
            .unwrap();

        // Errors 端点只返回失败请求行；工具失败不再进入该负载，历史行为由 Tools 页统计覆盖。
        let errors = store.errors(Range::Day, now, 50).unwrap();
        let mut keys = errors
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        assert_eq!(
            keys,
            ["has_more", "limit", "range", "requests", "updated_at"]
        );
        assert_eq!(errors["limit"], 50);
        assert!(errors["requests"].as_array().unwrap().is_empty());

        let tools = store.tools(Range::Day, now).unwrap();
        assert_eq!(tools["summary"]["executions"], 4);
        assert_eq!(tools["summary"]["errors"], 2);
        assert_eq!(tools["summary"]["cancelled"], 1);
        assert_eq!(tools["summary"]["avg_duration_ms"], 108.75);
        let bash = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["tool"] == "bash")
            .unwrap();
        assert_eq!(bash["calls"], 1);
        assert_eq!(bash["executions"], 2);
        assert_eq!(bash["errors"], 1);
        assert_eq!(bash["avg_duration_ms"], 210.0);
        assert_eq!(bash["models"][0]["errors"], 1);
        assert_eq!(bash["models"][0]["avg_duration_ms"], 210.0);
        let read = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["tool"] == "read")
            .unwrap();
        assert_eq!(read["errors"], 1);
        assert_eq!(read["cancelled"], 1);
        assert_eq!(read["avg_duration_ms"], 7.5);
        assert!(!tools.to_string().contains("/tmp/x"));
        // 取消不是错误：取消信息不出现在任何工具错误汇总里。
        assert!(!tools.to_string().contains("Tool cancelled"));

        store.conn.execute_batch("DROP TABLE tool_calls;").unwrap();
        drop(store);
        let store = open_store(&path).unwrap();
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM tool_calls", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        // 重开后表与索引按幂等 schema 重建。
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name IN ('tool_calls_time','tool_calls_error_time')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(store.latest(0, now + 1, 10).unwrap().len(), 1);
        let empty = store.errors(Range::All, now, 50).unwrap();
        assert!(empty["requests"].as_array().unwrap().is_empty());
        assert_eq!(empty["has_more"], false);
    }
    #[test]
    fn stats_api_rejects_invalid_queries_and_keeps_payloads_out_of_lists() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        store
            .insert(
                &fixture("test", chrono::Utc::now().timestamp_millis()),
                &json!({"text":"private output"}),
            )
            .unwrap();
        for path in [
            "/api/stats/overview?range=bad",
            "/api/stats/overview?range=1h&range=7d",
            "/api/stats/requests?limit=9999999",
            "/api/stats/requests/-1",
            "/api/stats/requests/not-a-number",
            "/api/stats/requests?foo=bar",
        ] {
            assert_eq!(server::dispatch(&store, path, &[]).0, 400, "{path}");
        }
        for path in [
            "/api/stats/overview?range=all",
            "/api/stats/requests?range=all&limit=500",
        ] {
            let (status, _, body) = server::dispatch(&store, path, &[]);
            assert_eq!(status, 200);
            assert!(!body.contains("private output"));
        }
        assert_eq!(
            server::dispatch(&store, "/api/stats/requests/999", &[]).0,
            404
        );
        let (status, _, body) = server::dispatch(&store, "/api/stats/requests/1", &[]);
        assert_eq!(status, 200);
        let detail: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(detail["request"]["request_key"], "test");
        assert!(detail["request_context"].is_null());
        assert!(detail.get("output_message").is_none());
        assert!(detail.get("session_entry").is_none());
        assert!(!body.contains("private output"));
    }
}
