use anyhow::{Result, ensure};
use rusqlite::{Connection, TransactionBehavior};
use std::path::Path;

pub(super) const UNAVAILABLE: &str = "History database unavailable.";
const UNSUPPORTED_VERSION: &str = "History database schema version is not supported.";
const CORRUPT: &str = "History database is corrupt.";

/// `sessions`/`session_prompts` 之外，两个 FTS 虚拟表也属于必要 schema；
/// user_version=1 的既有文件缺失其中任意对象一律按损坏拒绝，不当空库重建。
const REQUIRED_TABLES: &[&str] = &[
    "input_history",
    "sessions",
    "session_prompts",
    "session_fts",
    "prompt_fts",
];

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS input_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    cwd TEXT NOT NULL,
    text TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    rowid INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL UNIQUE,
    cwd TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_secs INTEGER NOT NULL,
    updated_at_nanos INTEGER NOT NULL,
    summary TEXT NOT NULL,
    preview TEXT NOT NULL,
    bytes INTEGER NOT NULL CHECK(bytes>=0),
    done INTEGER NOT NULL CHECK(done IN (0,1)),
    device INTEGER NOT NULL,
    inode INTEGER NOT NULL,
    summary_folded TEXT NOT NULL,
    preview_folded TEXT NOT NULL,
    cwd_folded TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS session_prompts (
    rowid INTEGER PRIMARY KEY,
    session_rowid INTEGER NOT NULL REFERENCES sessions(rowid) ON DELETE CASCADE,
    entry_offset INTEGER NOT NULL CHECK(entry_offset>=0),
    text TEXT NOT NULL,
    text_folded TEXT NOT NULL,
    UNIQUE(session_rowid,entry_offset)
);
CREATE INDEX IF NOT EXISTS input_history_cwd_id ON input_history(cwd,id DESC);
CREATE INDEX IF NOT EXISTS sessions_cwd_updated
    ON sessions(cwd,updated_at_secs DESC,updated_at_nanos DESC,session_id DESC);
CREATE INDEX IF NOT EXISTS sessions_updated
    ON sessions(updated_at_secs DESC,updated_at_nanos DESC,session_id DESC);
CREATE VIRTUAL TABLE IF NOT EXISTS session_fts USING fts5(
    summary_folded,preview_folded,cwd_folded,session_id,
    content='sessions',content_rowid='rowid',tokenize='trigram'
);
CREATE VIRTUAL TABLE IF NOT EXISTS prompt_fts USING fts5(
    text_folded,
    content='session_prompts',content_rowid='rowid',tokenize='trigram'
);
CREATE TRIGGER IF NOT EXISTS sessions_ai AFTER INSERT ON sessions BEGIN
    INSERT INTO session_fts(rowid,summary_folded,preview_folded,cwd_folded,session_id)
    VALUES (new.rowid,new.summary_folded,new.preview_folded,new.cwd_folded,new.session_id);
END;
CREATE TRIGGER IF NOT EXISTS sessions_au AFTER UPDATE OF
    summary_folded,preview_folded,cwd_folded,session_id ON sessions BEGIN
    INSERT INTO session_fts(session_fts,rowid,summary_folded,preview_folded,cwd_folded,session_id)
    VALUES ('delete',old.rowid,old.summary_folded,old.preview_folded,old.cwd_folded,old.session_id);
    INSERT INTO session_fts(rowid,summary_folded,preview_folded,cwd_folded,session_id)
    VALUES (new.rowid,new.summary_folded,new.preview_folded,new.cwd_folded,new.session_id);
END;
CREATE TRIGGER IF NOT EXISTS sessions_ad AFTER DELETE ON sessions BEGIN
    INSERT INTO session_fts(session_fts,rowid,summary_folded,preview_folded,cwd_folded,session_id)
    VALUES ('delete',old.rowid,old.summary_folded,old.preview_folded,old.cwd_folded,old.session_id);
END;
CREATE TRIGGER IF NOT EXISTS session_prompts_ai AFTER INSERT ON session_prompts BEGIN
    INSERT INTO prompt_fts(rowid,text_folded) VALUES (new.rowid,new.text_folded);
END;
CREATE TRIGGER IF NOT EXISTS session_prompts_au AFTER UPDATE OF text_folded ON session_prompts BEGIN
    INSERT INTO prompt_fts(prompt_fts,rowid,text_folded) VALUES ('delete',old.rowid,old.text_folded);
    INSERT INTO prompt_fts(rowid,text_folded) VALUES (new.rowid,new.text_folded);
END;
CREATE TRIGGER IF NOT EXISTS session_prompts_ad AFTER DELETE ON session_prompts BEGIN
    INSERT INTO prompt_fts(prompt_fts,rowid,text_folded) VALUES ('delete',old.rowid,old.text_folded);
END;";

// 不关联派生行的外键，重建或重新同步索引不能丢失用户的置顶偏好。
const PIN_SCHEMA: &str = "CREATE TABLE session_pins (session_id TEXT PRIMARY KEY NOT NULL);";

pub struct Store {
    /// 后续 `src/agent/persistence/index.rs` 需在同一连接上执行逐文件事务。
    pub(crate) conn: Connection,
}

pub(super) fn unavailable<E>(_: E) -> anyhow::Error {
    anyhow::anyhow!(UNAVAILABLE)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let mut conn = pp_core::sqlite::open(path).map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(unavailable)?;
        ensure!(version <= 3, UNSUPPORTED_VERSION);
        if version < 1 {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(unavailable)?;
            // 在写锁内重读版本，避免与并发初始化竞争。
            let version: i64 = tx
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(unavailable)?;
            ensure!(version <= 3, UNSUPPORTED_VERSION);
            if version == 0 {
                // 非 SQLite 识别的既有对象说明文件不是本模块创建的空库，拒绝覆盖。
                let existing: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('input_history',
                         'sessions','session_prompts','session_fts','prompt_fts')",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(unavailable)?;
                ensure!(existing == 0, CORRUPT);
                tx.execute_batch(SCHEMA).map_err(unavailable)?;
                tx.pragma_update(None, "user_version", 1)
                    .map_err(unavailable)?;
            }
            tx.commit().map_err(unavailable)?;
        } else {
            let missing = REQUIRED_TABLES
                .iter()
                .filter(|table| {
                    !matches!(
                        conn.query_row(
                            "SELECT 1 FROM sqlite_master WHERE name=?1 AND type='table'",
                            [table],
                            |_| Ok(()),
                        ),
                        Ok(())
                    )
                })
                .count();
            ensure!(missing == 0, CORRUPT);
        }
        // 在写事务内检查版本，允许多个 pp 进程同时升级旧库。
        if conn
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .map_err(unavailable)?
            < 2
        {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(unavailable)?;
            let version: i64 = tx
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(unavailable)?;
            if version == 1 {
                tx.execute_batch(
                    "ALTER TABLE input_history ADD COLUMN images TEXT NOT NULL DEFAULT '[]';",
                )
                .map_err(unavailable)?;
                tx.pragma_update(None, "user_version", 2)
                    .map_err(unavailable)?;
            }
            tx.commit().map_err(unavailable)?;
        }
        if conn
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .map_err(unavailable)?
            < 3
        {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(unavailable)?;
            let version: i64 = tx
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(unavailable)?;
            if version == 2 {
                tx.execute_batch(PIN_SCHEMA).map_err(unavailable)?;
                tx.pragma_update(None, "user_version", 3)
                    .map_err(unavailable)?;
            }
            tx.commit().map_err(unavailable)?;
        }
        ensure!(
            conn.query_row(
                "SELECT 1 FROM sqlite_master WHERE name='session_pins' AND type='table'",
                [],
                |_| Ok(())
            )
            .is_ok(),
            CORRUPT
        );
        Ok(Self { conn })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn sqlite_corrupt_databases_are_preserved_without_sensitive_errors() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("history.db");
        let bytes = b"not-sqlite fixture-private-credential";
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        let error = Store::open(&path).err().unwrap();
        assert!(!format!("{error:#}").contains("fixture-private-credential"));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn sqlite_rejects_unsupported_or_foreign_schema() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("history.db");
        {
            let store = Store::open(&path).unwrap();
            store.conn.pragma_update(None, "user_version", 4).unwrap();
        }
        let error = Store::open(&path).err().unwrap();
        assert_eq!(error.to_string(), UNSUPPORTED_VERSION);

        let foreign = root.path().join("foreign.db");
        fs::write(&foreign, "not a database at all").unwrap();
        fs::set_permissions(&foreign, fs::Permissions::from_mode(0o600)).unwrap();
        let error = Store::open(&foreign).err().unwrap();
        assert_eq!(error.to_string(), UNAVAILABLE);
    }
}
