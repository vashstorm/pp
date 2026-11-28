use super::schema::Store;
use super::schema::unavailable;
use anyhow::Result;
use pp_core::cancellation::CancellationToken;
use rusqlite::TransactionBehavior;
use std::collections::HashSet;
use std::path::Path;

impl Store {
    pub(crate) fn remove_session(&mut self, id: &str) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(unavailable)?;
        let sessions = tx
            .execute("DELETE FROM sessions WHERE session_id=?1", [id])
            .map_err(unavailable)?;
        let pins = tx
            .execute("DELETE FROM session_pins WHERE session_id=?1", [id])
            .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        Ok(sessions > 0 || pins > 0)
    }

    pub(crate) fn orphan_session_ids(&self, present: &HashSet<String>) -> Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT session_id FROM sessions UNION SELECT session_id FROM session_pins")
            .map_err(unavailable)?;
        let mut rows = statement.query([]).map_err(unavailable)?;
        let mut orphans = Vec::new();
        while let Some(row) = rows.next().map_err(unavailable)? {
            let id: String = row.get(0).map_err(unavailable)?;
            if !present.contains(&id) {
                orphans.push(id);
            }
        }
        Ok(orphans)
    }

    /// Best-effort reclaim of the WAL and free pages; if another connection holds the database, warn and skip without blocking GC.
    pub(crate) fn compact(&mut self, vacuum: bool) -> Result<()> {
        if let Err(error) = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
            if is_busy(&error) {
                tracing::warn!("history database busy during wal checkpoint");
                return Ok(());
            }
            return Err(unavailable(error));
        }
        if vacuum && let Err(error) = self.conn.execute_batch("VACUUM;") {
            if is_busy(&error) {
                tracing::warn!("history database busy during vacuum");
                return Ok(());
            }
            return Err(unavailable(error));
        }
        Ok(())
    }

    /// Syncs the session catalog projection: delegates to the persistence index module, which runs per-file transactions on the same connection,
    /// returning per-file redacted diagnostics. On cancellation the index module rolls back the current file's transaction and returns Cancelled.
    pub fn sync_sessions(
        &mut self,
        dir: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Vec<String>> {
        crate::agent::persistence::index::sync(&mut self.conn, dir, cancellation)
    }

    /// Acceptance helper (test only): drop derived table data then resync to verify the projection rebuilds completely
    /// without touching input_history. DELETE sessions empties prompts through the foreign-key cascade,
    /// and triggers remove the FTS documents in step.
    #[cfg(test)]
    pub(crate) fn rebuild_sessions(&mut self, dir: &Path) -> Result<Vec<String>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(unavailable)?;
        tx.execute("DELETE FROM sessions", [])
            .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        self.sync_sessions(dir, &CancellationToken::new())
    }
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{count, cwd, match_rows, temp_store};

    #[test]
    fn sqlite_fts_triggers_cascade_and_derived_rebuild_preserve_inputs() {
        let (root, mut store) = temp_store("history.db");
        let a = cwd(root.path(), "project-a");
        store
            .append_input(&a, "修复模型切换失败 needle_42", 1000)
            .unwrap();

        store
            .conn
            .execute(
                "INSERT INTO sessions(session_id,cwd,created_at_ms,updated_at_secs,
                 updated_at_nanos,summary,preview,bytes,done,device,inode,
                 summary_folded,preview_folded,cwd_folded)
                 VALUES ('session-1',?1,10,20,30,'示例标题','首条预览内容',42,1,7,8,
                 '示例标题','首条预览内容',?2)",
                rusqlite::params![a.to_str().unwrap(), a.to_str().unwrap()],
            )
            .unwrap();
        let session_rowid = store.conn.last_insert_rowid();
        store
            .conn
            .execute(
                "INSERT INTO session_prompts(session_rowid,entry_offset,text,text_folded)
                 VALUES (?1,0,'首条 无关键词的记录','首条 无关键词的记录')",
                [session_rowid],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO session_prompts(session_rowid,entry_offset,text,text_folded)
                 VALUES (?1,1,'修复模型切换失败 needle_42','修复模型切换失败 needle_42')",
                [session_rowid],
            )
            .unwrap();

        // Trigram phrase matching: long terms hit (including a term present only in the second prompt).
        assert_eq!(match_rows(&store.conn, "prompt_fts", "\"模型切\""), [2]);
        assert_eq!(match_rows(&store.conn, "session_fts", "\"示例标题\""), [1]);
        assert_eq!(match_rows(&store.conn, "session_fts", "\"预览内容\""), [1]);
        assert_eq!(
            match_rows(&store.conn, "session_fts", "\"project-a\"").len(),
            1
        );

        // Update trigger: delete OLD then insert NEW, so old and new content stay searchable.
        store
            .conn
            .execute(
                "UPDATE sessions SET summary='新标题了', summary_folded='新标题了' WHERE rowid=?1",
                [session_rowid],
            )
            .unwrap();
        assert!(match_rows(&store.conn, "session_fts", "\"示例标题\"").is_empty());
        assert_eq!(match_rows(&store.conn, "session_fts", "\"新标题了\""), [1]);
        store
            .conn
            .execute(
                "UPDATE session_prompts SET text='改写后的新内容', text_folded='改写后的新内容'
                 WHERE session_rowid=?1 AND entry_offset=0",
                [session_rowid],
            )
            .unwrap();
        assert!(match_rows(&store.conn, "prompt_fts", "\"无关键词\"").is_empty());
        assert_eq!(match_rows(&store.conn, "prompt_fts", "\"改写后\""), [1]);

        // Deleting a single prompt directly: the FTS document is removed in step.
        store
            .conn
            .execute(
                "DELETE FROM session_prompts WHERE session_rowid=?1 AND entry_offset=1",
                [session_rowid],
            )
            .unwrap();
        assert!(match_rows(&store.conn, "prompt_fts", "\"needle_42\"").is_empty());

        // Deleting the session: the FK cascade empties prompts, both FTS indexes are left empty, and input_history is unaffected.
        store
            .conn
            .execute("DELETE FROM sessions WHERE rowid=?1", [session_rowid])
            .unwrap();
        assert_eq!(
            count(&store.conn, "SELECT COUNT(*) FROM session_prompts"),
            0
        );
        assert_eq!(count(&store.conn, "SELECT COUNT(*) FROM session_fts"), 0);
        assert_eq!(count(&store.conn, "SELECT COUNT(*) FROM prompt_fts"), 0);
        assert_eq!(
            store.load_inputs(&a, None, 100).unwrap()[0].text,
            "修复模型切换失败 needle_42"
        );

        // Index consistency acceptance after rebuilding the derived projection: the external-content FTS integrity-check must pass.
        store
            .conn
            .execute(
                "INSERT INTO session_fts(session_fts) VALUES ('integrity-check')",
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO prompt_fts(prompt_fts) VALUES ('integrity-check')",
                [],
            )
            .unwrap();
    }
}
