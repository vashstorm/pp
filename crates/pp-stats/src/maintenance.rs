use super::{Store, call_tools, database_path, ensure_column, saved_tool_calls, table_columns};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::fs;
use std::time::Duration;

/// 一次性退役旧 `request_payloads`：复制首请求快照、回填工具调用、删表并回收空间。
///
/// 幂等：旧表不存在时只提交一个空事务后返回，不建表、不 `VACUUM`。迁移只写
/// `request_contexts`／`request_tools`，不改动 `requests` 的任一字段。仅在
/// `server::run` 启动与 [`gc`] 中调用，运行期写路径与 `Store::open` 不执行。
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
        Ok(true) => {
            let page_count: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
            let page_size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
            if page_count.saturating_mul(page_size) > 64 * 1024 * 1024 {
                conn.execute_batch("VACUUM;")?;
            }
            Ok(())
        }
        Ok(false) => Ok(()),
        Err(error) => Err(error),
    };
    // 无论迁移或回收成败，都恢复 `Store::open` 的默认超时。
    let restored = conn.busy_timeout(Duration::from_millis(250));
    outcome?;
    restored?;
    Ok(())
}

/// Returns (total requests before cleanup, removed), counted in the same transaction.
/// Delete request snapshots before their parents, then reclaim the freed pages.
pub fn gc(cutoff: i64) -> Result<(u64, u64)> {
    let path = database_path()?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(metadata.is_file(), "statistics database must be a file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(error) => return Err(error.into()),
    }
    let mut store = Store::open(&path)?;
    // 旧库迁移失败时保留旧表并让本次 `gc` 失败（非零退出），下次重试。
    maintain(&mut store.conn)?;
    let tx = store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let total = tx.query_row("SELECT COUNT(*) FROM requests", [], |row| {
        row.get::<_, i64>(0)
    })? as u64;
    tx.execute(
        "DELETE FROM request_tools WHERE request_id IN (SELECT id FROM requests WHERE timestamp < ?1)",
        [cutoff],
    )?;
    tx.execute(
        "DELETE FROM request_contexts WHERE request_id IN (SELECT id FROM requests WHERE timestamp < ?1)",
        [cutoff],
    )?;
    let removed = tx.execute("DELETE FROM requests WHERE timestamp < ?1", [cutoff])? as u64;
    tx.commit()?;
    if removed > 0 {
        store.conn.execute_batch("VACUUM;")?;
    }
    Ok((total, removed))
}
