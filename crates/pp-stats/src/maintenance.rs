use super::{
    SessionScope, Store, call_tools, database_path, ensure_column, saved_tool_calls, table_columns,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, Params, Transaction, TransactionBehavior, params};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::time::Duration;

/// 一次性退役旧 `request_payloads`：复制首请求快照、回填工具调用、删表并回收空间。
///
/// 幂等：旧表不存在时只提交一个空事务后返回，不建表、不 `VACUUM`。迁移只写
/// `request_contexts`／`request_tools`，不改动 `requests` 的任一字段。仅在
/// `server::run` 启动与 `pp gc` 中调用，运行期写路径与 `Store::open` 不执行。
pub(crate) fn maintain(conn: &mut Connection) -> Result<()> {
    // 迁移与其它连接的写事务竞争，临时放宽锁等待；无论成败都恢复 `Store::open`
    // 的 250ms，避免把一次性迁移的等待时间留在常驻连接上。
    conn.busy_timeout(Duration::from_secs(5))?;
    let result = (|| -> Result<bool> {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let legacy = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='request_payloads')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !legacy {
            tx.commit()?;
            return Ok(false);
        }
        // 更早的旧库可能缺这两列（旧版 `ensure_schema` 会补）；迁移前就地补齐，
        // 随后整表删除，不改变永久 schema。
        let mut columns = table_columns(&tx, "request_payloads")?;
        ensure_column(
            &tx,
            "request_payloads",
            &mut columns,
            "request_json",
            "TEXT",
        )?;
        ensure_column(&tx, "request_payloads", &mut columns, "skills_json", "TEXT")?;
        tx.execute(
            "INSERT OR IGNORE INTO request_contexts(request_id,request_json,skills_json) \
             SELECT request_id,request_json,skills_json FROM request_payloads \
             WHERE request_json IS NOT NULL",
            [],
        )?;
        // 先取一次已回填的请求集合，重试时整行跳过，避免逐行查询。
        let existing = {
            let mut statement = tx.prepare("SELECT DISTINCT request_id FROM request_tools")?;
            statement
                .query_map([], |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?
        };
        let mut insert =
            tx.prepare("INSERT INTO request_tools(request_id,tool,args_chars) VALUES (?1,?2,?3)")?;
        // 逐行流式读取旧正文，避免把整库文本同时读进内存。
        let mut payloads =
            tx.prepare("SELECT request_id,output_json,session_entry_json FROM request_payloads")?;
        let mut rows = payloads.query([])?;
        while let Some(row) = rows.next()? {
            let request_id: i64 = row.get(0)?;
            if existing.contains(&request_id) {
                continue;
            }
            // 与迁移前的聚合口径一致：已提交会话条目里的 toolCall 优先，缺失时回退到
            // 旧包装 `{"native":…}` 的原生输出；损坏的快照按空处理，不中断整次迁移。
            let entry = row
                .get::<_, Option<String>>(2)?
                .as_deref()
                .and_then(|json| serde_json::from_str(json).ok())
                .unwrap_or(Value::Null);
            let mut calls = saved_tool_calls(&entry);
            if calls.is_empty() {
                let output = row
                    .get::<_, Option<String>>(1)?
                    .as_deref()
                    .and_then(|json| serde_json::from_str::<Value>(json).ok())
                    .unwrap_or(Value::Null);
                calls = call_tools(&output["native"]);
            }
            for (tool, chars) in calls {
                insert.execute(params![
                    request_id,
                    tool,
                    i64::try_from(chars).context("tool argument length exceeds i64")?
                ])?;
            }
        }
        drop(rows);
        drop(payloads);
        drop(insert);
        tx.execute("DROP TABLE IF EXISTS request_payloads", [])?;
        tx.commit()?;
        Ok(true)
    })();
    // 大库迁移提交后立即回收空间；`VACUUM` 仍在放宽后的锁等待内执行。
    let outcome = match result {
        Ok(true) => (|| -> Result<()> {
            let page_count: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
            let page_size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
            if page_count.saturating_mul(page_size) > 64 * 1024 * 1024 {
                conn.execute_batch("VACUUM;")?;
            }
            Ok(())
        })(),
        Ok(false) => Ok(()),
        Err(error) => Err(error),
    };
    // 无论迁移或回收成败，都恢复 `Store::open` 的默认超时。
    let restored = conn.busy_timeout(Duration::from_millis(250));
    outcome?;
    restored?;
    Ok(())
}

pub struct GarbageCollection {
    store: Option<Store>,
    requests_removed: u64,
    changed: bool,
}

impl GarbageCollection {
    pub fn open() -> Result<Self> {
        Self::open_at(&database_path()?)
    }

    fn open_at(path: &Path) -> Result<Self> {
        let store = match fs::symlink_metadata(path) {
            Ok(metadata) => {
                ensure!(metadata.is_file(), "statistics database must be a file");
                let mut store = Store::open(path)?;
                maintain(&mut store.conn)?;
                Some(store)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            store,
            requests_removed: 0,
            changed: false,
        })
    }

    /// 在会话锁保护下清理根会话及其子 Agent 的统计。
    pub fn remove_session(&mut self, root: &Path) -> Result<()> {
        let Some(store) = &mut self.store else {
            return Ok(());
        };
        let scope = SessionScope::for_root(root)?;
        let tx = store
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (removed, changed) = delete_records(
            &tx,
            "session_file = ?1 OR (session_file >= ?2 AND session_file < ?3)",
            params![scope.root, scope.prefix, scope.upper],
        )?;
        tx.commit()?;
        self.requests_removed += removed;
        self.changed |= changed;
        Ok(())
    }

    /// 清理按时间过期的统计，并在本次 GC 结束时统一回收空间。
    pub fn finish(self, cutoff: i64) -> Result<(u64, u64)> {
        let Some(mut store) = self.store else {
            return Ok((0, 0));
        };
        let tx = store
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let total = tx.query_row("SELECT COUNT(*) FROM requests", [], |row| {
            row.get::<_, i64>(0)
        })? as u64;
        let (removed, changed) = delete_records(&tx, "timestamp < ?1", [cutoff])?;
        tx.commit()?;
        if self.changed || changed {
            store.conn.execute_batch("VACUUM;")?;
        }
        Ok((
            total + self.requests_removed,
            removed + self.requests_removed,
        ))
    }
}

fn delete_records(
    tx: &Transaction<'_>,
    filter: &str,
    parameters: impl Params + Copy,
) -> Result<(u64, bool)> {
    for table in ["request_tools", "request_contexts"] {
        tx.execute(
            &format!(
                "DELETE FROM {table} WHERE request_id IN (SELECT id FROM requests WHERE {filter})"
            ),
            parameters,
        )?;
    }
    let removed = tx.execute(&format!("DELETE FROM requests WHERE {filter}"), parameters)?;
    let tools_removed = tx.execute(
        &format!("DELETE FROM tool_calls WHERE {filter}"),
        parameters,
    )?;
    let hooks_removed = tx.execute(&format!("DELETE FROM hook_runs WHERE {filter}"), parameters)?;
    Ok((
        removed as u64,
        removed > 0 || tools_removed > 0 || hooks_removed > 0,
    ))
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use crate::{ToolCall, ToolStatus};

    #[test]
    fn gc_removes_expired_tool_calls_alongside_requests() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("stats.db");
        let mut store = open_store(&db_path).unwrap();
        let session = dir.path().join("s.jsonl");
        store
            .insert(&fixture("old", 100), &serde_json::Value::Null)
            .unwrap();
        store
            .insert(&fixture("fresh", 2000), &serde_json::Value::Null)
            .unwrap();
        store
            .tool(&ToolCall {
                request_key: Some("old"),
                session_file: &session,
                call_id: "c1",
                tool: "bash",
                status: ToolStatus::Ok,
                error_message: None,
                timestamp: 100,
                duration_ms: 10.0,
                result_chars: None,
            })
            .unwrap();
        store
            .tool(&ToolCall {
                request_key: Some("fresh"),
                session_file: &session,
                call_id: "c2",
                tool: "read",
                status: ToolStatus::Ok,
                error_message: None,
                timestamp: 2000,
                duration_ms: 5.0,
                result_chars: None,
            })
            .unwrap();
        drop(store);

        let (total, removed) = GarbageCollection::open_at(&db_path)
            .unwrap()
            .finish(1000)
            .unwrap();
        assert_eq!((total, removed), (2, 1));
        let store = open_store(&db_path).unwrap();
        let remaining_tools: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM tool_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining_tools, 1);
    }
}
