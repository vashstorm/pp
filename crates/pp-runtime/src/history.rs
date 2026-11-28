//! Persistent input and session derived-index storage for history.db (`~/.pp/agent/history.db`).
//!
//! Safe opening and the WAL/foreign-key/FULL/250ms settings are all handled by `crate::sqlite::open`; this module owns
//! only history.db's fixed schema and domain operations. Every storage error is converted into a fixed domain hint that
//! carries no SQL parameters or body text. `input_history` is independent persistent data (the accepted raw input text),
//! `session_pins` stores the separate user pin preference, while `sessions`/`session_prompts` and their FTS tables are a
//! derived projection of session logs that `sync_sessions` (the persistence index module) can rebuild at any time;
//! deleting derived rows never touches `input_history`.

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
