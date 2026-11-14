use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

/// 建表、补列、建索引并在结构发生变化时刷新 planner 统计。
///
/// 不读写 `PRAGMA user_version`：旧库按缺列／缺索引幂等补齐，历史行保持不变；
/// 结构齐全时只做几次廉价查询，不执行 `ANALYZE`。
pub(super) fn ensure_schema(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS requests (
            id INTEGER PRIMARY KEY, request_key TEXT NOT NULL UNIQUE, timestamp INTEGER NOT NULL,
            model TEXT NOT NULL, provider TEXT NOT NULL, model_reference TEXT NOT NULL, api TEXT NOT NULL,
            project TEXT NOT NULL, cwd TEXT NOT NULL, purpose TEXT NOT NULL,
            session_file TEXT, entry_id TEXT, duration_ms REAL NOT NULL, ttft_ms REAL,
            input_tokens INTEGER, output_tokens INTEGER, cached_tokens INTEGER,
            status TEXT NOT NULL CHECK(status IN ('ok','failed','aborted')), stop_reason TEXT, error_message TEXT,
            cache_write_tokens INTEGER, cost_json TEXT, rate_json TEXT
        );
        CREATE TABLE IF NOT EXISTS request_contexts (
            request_id INTEGER PRIMARY KEY REFERENCES requests(id),
            request_json TEXT NOT NULL, skills_json TEXT
        );
        CREATE TABLE IF NOT EXISTS request_tools (
            request_id INTEGER NOT NULL REFERENCES requests(id),
            tool TEXT NOT NULL, args_chars INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS tool_calls (
            id INTEGER PRIMARY KEY, request_key TEXT, session_file TEXT NOT NULL, call_id TEXT NOT NULL,
            tool TEXT NOT NULL, timestamp INTEGER NOT NULL,
            status TEXT NOT NULL CHECK(status IN ('ok','error','cancelled')),
            error_message TEXT, duration_ms REAL NOT NULL, result_chars INTEGER, UNIQUE(session_file, call_id)
        );",
    )?;
    let mut changed = false;
    for (name, ddl) in [
        (
            "requests_time",
            "CREATE INDEX IF NOT EXISTS requests_time ON requests(timestamp DESC,id DESC)",
        ),
        (
            "requests_failed_time",
            "CREATE INDEX IF NOT EXISTS requests_failed_time ON requests(timestamp DESC,id DESC) WHERE status='failed'",
        ),
        (
            "requests_session",
            "CREATE INDEX IF NOT EXISTS requests_session ON requests(session_file)",
        ),
        (
            "tool_calls_time",
            "CREATE INDEX IF NOT EXISTS tool_calls_time ON tool_calls(timestamp DESC,id DESC)",
        ),
        (
            "request_tools_request",
            "CREATE INDEX IF NOT EXISTS request_tools_request ON request_tools(request_id)",
        ),
    ] {
        changed |= !index_exists(&tx, name)?;
        tx.execute_batch(ddl)?;
    }
    // `tool_calls_error_time` 曾服务于 Errors 页的工具失败列表；该列表与聚合删除后无消费者，
    // 旧库在这里幂等删除，不留下只会拖慢写入的废弃索引。
    if index_exists(&tx, "tool_calls_error_time")? {
        tx.execute_batch("DROP INDEX IF EXISTS tool_calls_error_time;")?;
        changed = true;
    }
    let mut requests = table_columns(&tx, "requests")?;
    changed |= ensure_column(
        &tx,
        "requests",
        &mut requests,
        "cache_write_tokens",
        "INTEGER",
    )?;
    changed |= ensure_column(&tx, "requests", &mut requests, "cost_json", "TEXT")?;
    changed |= ensure_column(&tx, "requests", &mut requests, "rate_json", "TEXT")?;
    let mut tool_calls = table_columns(&tx, "tool_calls")?;
    changed |= ensure_column(
        &tx,
        "tool_calls",
        &mut tool_calls,
        "result_chars",
        "INTEGER",
    )?;
    if changed {
        // 只在新结构第一次出现时给 planner 补统计，之后每次打开不写库。
        tx.execute_batch("ANALYZE requests; ANALYZE tool_calls;")?;
    }
    tx.commit()?;
    Ok(())
}

fn index_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1",
        [name],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
}

pub(super) fn table_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns)
}

pub(super) fn ensure_column(
    conn: &Connection,
    table: &str,
    columns: &mut Vec<String>,
    column: &str,
    kind: &str,
) -> rusqlite::Result<bool> {
    if columns.iter().any(|name| name == column) {
        return Ok(false);
    }
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
    columns.push(column.to_owned());
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use crate::{
        COLUMNS, CostSnapshot, Range, RateSnapshot, SessionScope, Store, ToolCall, ToolStatus,
        activity, latest_sql, session_requests_sql,
    };
    use rusqlite::params;
    use serde_json::{Value, json};
    use std::path::Path;

    #[test]
    fn stats_failed_index_restore_preserves_error_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        let mut store = open_store(&path).unwrap();
        for (key, status) in [
            ("failed-one", "failed"),
            ("ok", "ok"),
            ("aborted", "aborted"),
            ("failed-two", "failed"),
        ] {
            let mut record = fixture(key, 1000);
            record.status = status.into();
            record.error_message = (status == "failed").then(|| format!("{key} error"));
            store
                .insert(&record, &json!({"text":format!("{key} full output")}))
                .unwrap();
        }
        store
            .link("failed-one", Path::new("/missing/session.jsonl"))
            .unwrap();
        let errors = store.errors(Range::All, 2000, 50).unwrap();
        assert_eq!(errors["requests"].as_array().unwrap().len(), 2);
        let latest = serde_json::to_value(store.latest(0, 2000, 500).unwrap()).unwrap();
        let details = (1..=4)
            .map(|id| store.detail(id).unwrap().unwrap())
            .collect::<Vec<_>>();
        store
            .conn
            .execute_batch("DROP INDEX requests_failed_time;")
            .unwrap();
        drop(store);
        for _ in 0..2 {
            let store = open_store(&path).unwrap();
            assert_eq!(store.errors(Range::All, 2000, 50).unwrap(), errors);
            assert_eq!(
                serde_json::to_value(store.latest(0, 2000, 500).unwrap()).unwrap(),
                latest
            );
            for (id, detail) in (1..=4).zip(&details) {
                assert_eq!(&store.detail(id).unwrap().unwrap(), detail);
            }
        }
    }

    #[test]
    fn stats_schema_is_ensured_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        let mut store = open_store(&path).unwrap();
        let mut priced = fixture("priced", 1000);
        priced.cost = Some(CostSnapshot {
            input: 0.25,
            cache_read: 0.25,
            cache_write: 0.25,
            output: 0.25,
        });
        priced.rates = Some(RateSnapshot {
            input: 2.0,
            cached_input: 0.2,
            cache_write: None,
            output: 8.0,
        });
        store.insert(&priced, &Value::Null).unwrap();
        store
            .insert(&fixture("legacy", 1001), &Value::Null)
            .unwrap();
        // 采集时写入的快照可被列表与详情读取。
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM requests WHERE cost_json IS NOT NULL AND rate_json IS NOT NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        let detail = store.detail(1).unwrap().unwrap();
        assert_eq!(detail["request"]["cost"], 1.0);
        assert_eq!(detail["request"]["cost_components"]["cache_write"], 0.25);
        assert_eq!(
            store.latest(0, 1000, 1).unwrap()[0].rates().unwrap().input,
            2.0
        );
        let session = dir.path().join("session.jsonl");
        store
            .tool(&ToolCall {
                request_key: Some("priced"),
                session_file: &session,
                call_id: "call-1",
                tool: "bash",
                status: ToolStatus::Error,
                error_message: Some("boom"),
                timestamp: 1000,
                duration_ms: 5.0,
                result_chars: Some(12),
            })
            .unwrap();
        // 模拟旧库：新索引、废弃索引与费用快照列都不在预期状态。
        store
            .conn
            .execute_batch(
                "DROP INDEX requests_session;
                 CREATE INDEX tool_calls_error_time ON tool_calls(timestamp DESC,id DESC) WHERE status='error';
                 ALTER TABLE requests DROP COLUMN cost_json; ALTER TABLE requests DROP COLUMN rate_json;
                 ALTER TABLE tool_calls DROP COLUMN result_chars;",
            )
            .unwrap();
        drop(store);

        let store = open_store(&path).unwrap();
        assert!(
            table_columns(&store.conn, "tool_calls")
                .unwrap()
                .contains(&"result_chars".to_owned())
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT result_chars FROM tool_calls WHERE call_id='call-1'",
                    [],
                    |row| row.get::<_, Option<i64>>(0)
                )
                .unwrap(),
            None
        );
        let indexes = |table: &str| -> Vec<String> {
            let mut statement = store
                .conn
                .prepare(&format!("PRAGMA index_list({table})"))
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert!(indexes("requests").contains(&"requests_session".to_owned()));
        assert!(indexes("requests").contains(&"requests_time".to_owned()));
        // 废弃的部分索引在重开时被删除，不重建。
        assert!(
            !indexes("tool_calls").contains(&"tool_calls_error_time".to_owned()),
            "旧库里的废弃索引必须被删除"
        );
        assert!(indexes("tool_calls").contains(&"tool_calls_time".to_owned()));
        let columns = table_columns(&store.conn, "requests").unwrap();
        assert!(columns.iter().any(|name| name == "cost_json"));
        assert!(columns.iter().any(|name| name == "rate_json"));
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM requests", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
        // 历史行保留；补列后旧行的费用快照全为 NULL，按未定价统计。
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM requests", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM requests WHERE cost_json IS NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        let summary = store.analytics(Range::All, 2000).unwrap();
        assert_eq!(summary["summary"]["requests"], 2);
        assert_eq!(summary["summary"]["priced"], 0);
        assert_eq!(summary["summary"]["unpriced"], 2);
        assert!(summary["summary"]["cost"].is_null());
        // 补齐结构的那次 open 会给 planner 补统计。
        for table in ["requests", "tool_calls"] {
            assert!(
                store
                    .conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_stat1 WHERE tbl=?1",
                        [table],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap()
                    > 0,
                "{table} 缺少 ANALYZE 统计"
            );
        }
        // 版本号不再被读写。
        assert_eq!(
            store
                .conn
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        let structure = |store: &Store| -> Vec<String> {
            let mut statement = store
                .conn
                .prepare("SELECT type,name,sql FROM sqlite_master ORDER BY type,name")
                .unwrap();
            statement
                .query_map([], |row| {
                    Ok(format!(
                        "{}:{}:{}",
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?.unwrap_or_default()
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let before = structure(&store);
        drop(store);
        let store = open_store(&path).unwrap();
        // 结构齐全时再次 open 不改变 schema。
        assert_eq!(structure(&store), before);
    }

    /// 取 `EXPLAIN QUERY PLAN` 的计划文本（`detail` 列）。
    fn query_plan(conn: &Connection, sql: &str, parameters: impl rusqlite::Params) -> String {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        statement
            .query_map(parameters, |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n")
    }

    #[test]
    fn stats_query_plans_use_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        let now = 1_800_000_000_000;
        let root = "root.jsonl";
        let child = "root.subagents/sessions/child.jsonl";
        for n in 0..200 {
            let mut record = fixture(&format!("request-{n}"), now - n * 60_000);
            // 真实库中一个会话只占少数行；根会话与子 Agent 各保留几条，其余归入其它会话。
            record.session_file = Some(match n {
                0..=3 => root.to_owned(),
                4..=7 => child.to_owned(),
                _ => format!("other-{n}.jsonl"),
            });
            if n % 20 == 0 {
                record.status = "failed".into();
                record.error_message = Some("HTTP 500".into());
            }
            store.insert(&record, &Value::Null).unwrap();
        }
        for n in 0..100 {
            let call_id = format!("call-{n}");
            let session = match n {
                0..=3 => root.to_owned(),
                4..=7 => child.to_owned(),
                _ => format!("other-{n}.jsonl"),
            };
            store
                .tool(&ToolCall {
                    request_key: Some("request-0"),
                    session_file: Path::new(&session),
                    call_id: &call_id,
                    tool: "bash",
                    status: if n % 10 == 0 {
                        ToolStatus::Error
                    } else {
                        ToolStatus::Ok
                    },
                    error_message: None,
                    timestamp: now - n * 60_000,
                    duration_ms: 12.0,
                    result_chars: None,
                })
                .unwrap();
        }
        store
            .conn
            .execute_batch("ANALYZE requests; ANALYZE tool_calls;")
            .unwrap();
        let scope =
            SessionScope::new(root.to_owned(), "root.subagents/sessions/".to_owned()).unwrap();
        let since = Range::Day.since(now);

        let latest = query_plan(&store.conn, &latest_sql(), params![since, now, 12_i64]);
        assert!(latest.contains("requests_time"), "{latest}");
        assert!(!latest.contains("SCAN requests"), "{latest}");

        let session = query_plan(
            &store.conn,
            &session_requests_sql(COLUMNS),
            params![scope.root, scope.prefix, scope.upper],
        );
        assert!(session.contains("requests_session"), "{session}");
        assert!(!session.contains("SCAN requests"), "{session}");

        let failed = query_plan(
            &store.conn,
            &activity::failed_requests_sql(),
            params![since, now, 50_i64],
        );
        assert!(failed.contains("requests_failed_time"), "{failed}");
        assert!(!failed.contains("SCAN requests"), "{failed}");

        let trace_tools = query_plan(
            &store.conn,
            &activity::trace_tools_sql(),
            params![scope.root, scope.prefix, scope.upper],
        );
        assert!(
            trace_tools.contains("sqlite_autoindex_tool_calls_1"),
            "{trace_tools}"
        );
        assert!(!trace_tools.contains("SCAN tool_calls"), "{trace_tools}");

        // `range=all` 从时间下界 0 开始；记录实际计划，由报告说明是否退化为全表读取。
        for (name, plan) in [
            (
                "latest",
                query_plan(&store.conn, &latest_sql(), params![0_i64, now, 12_i64]),
            ),
            (
                "failed",
                query_plan(
                    &store.conn,
                    &activity::failed_requests_sql(),
                    params![0_i64, now, 50_i64],
                ),
            ),
        ] {
            eprintln!("range=all {name} plan:\n{plan}");
        }
    }
}
