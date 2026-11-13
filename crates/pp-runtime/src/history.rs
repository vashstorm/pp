//! history.db（`~/.pp/agent/history.db`）的持久输入与会话派生索引存储。
//!
//! 安全打开、WAL/外键/FULL/250ms 由 `crate::sqlite::open` 统一负责；本模块只拥有
//! history.db 的固定 schema 与领域操作。所有存储错误都转换为不含 SQL 参数与正文
//! 的固定领域提示。`input_history` 是独立持久数据（已接受的原始输入原文），
//! `session_pins` 保存独立的用户置顶偏好；`sessions`/`session_prompts` 及其 FTS 是会话日志的派生投影，可随时由
//! `sync_sessions`（persistence 索引模块）重建；派生行的删除绝不触碰 `input_history`。

mod index;
mod input;
mod schema;
mod search;

pub use input::InputEntry;
pub use schema::Store;

#[cfg(test)]
mod fixtures {
    use super::*;
    use rusqlite::Connection;
    use std::fs;
    use std::path::Path;

    pub(in crate::history) fn temp_store(name: &str) -> (tempfile::TempDir, Store) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(name);
        let store = Store::open(&path).unwrap();
        (root, store)
    }

    pub(in crate::history) fn cwd(root: &Path, name: &str) -> std::path::PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub(in crate::history) fn match_rows(conn: &Connection, fts: &str, phrase: &str) -> Vec<i64> {
        let sql = format!("SELECT rowid FROM {fts} WHERE {fts} MATCH ?1 ORDER BY rowid");
        let mut statement = conn.prepare(&sql).unwrap();
        let rows = statement
            .query_map([phrase], |row| row.get::<_, i64>(0))
            .unwrap();
        rows.map(|row| row.unwrap()).collect()
    }

    pub(in crate::history) fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }
}
