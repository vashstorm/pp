use super::{
    SessionScope, Store, call_tools, database_path, ensure_column, saved_tool_calls, table_columns,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, Params, Transaction, TransactionBehavior, params};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::time::Duration;

/// One-time retirement of the legacy `request_payloads`: copies the first-request snapshot,
/// backfills tool calls, drops the table and reclaims space.
///
/// Idempotent: when the legacy table is absent it commits an empty transaction and returns without
/// creating tables or running `VACUUM`. The migration only writes `request_contexts`/`request_tools`
/// and touches no field of `requests`. It is called only from `server::run` startup and `pp gc`;
/// the runtime write path and `Store::open` never run it.
pub(crate) fn maintain(conn: &mut Connection) -> Result<()> {
    // The migration competes with write transactions on other connections, so the lock wait is
    // widened temporarily; success or failure, the 250ms wait of `Store::open` is restored so the
    // one-off migration wait does not stay on a long-lived connection.
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
        // Older databases may lack these two columns (the previous `ensure_schema` added them); add
        // them in place before migrating, then drop the whole table without changing the permanent
        // schema.
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
        // Fetch the already-backfilled request set once so retries skip whole rows instead of
        // querying per row.
        let existing = {
            let mut statement = tx.prepare("SELECT DISTINCT request_id FROM request_tools")?;
            statement
                .query_map([], |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?
        };
        let mut insert =
            tx.prepare("INSERT INTO request_tools(request_id,tool,args_chars) VALUES (?1,?2,?3)")?;
        // Stream the legacy bodies row by row instead of loading the whole database text into memory.
        let mut payloads =
            tx.prepare("SELECT request_id,output_json,session_entry_json FROM request_payloads")?;
        let mut rows = payloads.query([])?;
        while let Some(row) = rows.next()? {
            let request_id: i64 = row.get(0)?;
            if existing.contains(&request_id) {
                continue;
            }
            // Matches the pre-migration aggregation: a toolCall in the committed session entry wins,
            // falling back to the native output of the legacy `{"native":…}` wrapper when missing; a
            // corrupt snapshot counts as empty and does not abort the whole migration.
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
    // Reclaim space right after a large migration commits; `VACUUM` still runs within the widened
    // lock wait.
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
    // Whether the migration or reclamation succeeds or fails, restore the default wait of
    // `Store::open`.
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

    /// Removes the stats of a root session and its subagents under the session lock.
    pub fn remove_session(&mut self, root: &Path) -> Result<()> {
        let Some(store) = &mut self.store else {
            return Ok(());
        };
        let scope = SessionScope::for_root(root)?;
        let (removed, changed) = remove_session_in(&mut store.conn, &scope)?;
        self.requests_removed += removed;
        self.changed |= changed;
        Ok(())
    }

    /// Lightweight cleanup of a single session (and its subagents); it does not run `VACUUM` and does not
    /// create an empty database when the stats database is missing.
    pub fn delete_session(database: &super::Database, root: &Path) -> Result<()> {
        let scope = SessionScope::for_root(root)?;
        database.with_existing_store(|store| {
            remove_session_in(&mut store.conn, &scope)?;
            Ok(())
        })?;
        Ok(())
    }

    /// Enumerates the root session file name IDs whose paths fall inside `sessions_dir` (subagent paths
    /// fold into the root).
    pub fn orphan_session_ids(&self, sessions_dir: &Path) -> Result<Vec<String>> {
        let Some(store) = &self.store else {
            return Ok(Vec::new());
        };
        let scope = SessionScope::new(
            String::new(),
            format!("{}/", sessions_dir.to_string_lossy().trim_end_matches('/')),
        )?;
        let mut ids = std::collections::BTreeSet::new();
        for table in ["requests", "tool_calls", "hook_runs"] {
            let mut statement = store.conn.prepare(&format!(
                "SELECT DISTINCT session_file FROM {table} \
                 WHERE session_file >= ?1 AND session_file < ?2"
            ))?;
            let mut rows = statement.query(params![scope.prefix, scope.upper])?;
            while let Some(row) = rows.next()? {
                let path: String = row.get(0)?;
                if let Some(id) = extract_root_session_id(&scope.prefix, &path) {
                    ids.insert(id.to_owned());
                }
            }
        }
        Ok(ids.into_iter().collect())
    }

    /// Removes time-expired stats and reclaims space once at the end of this GC.
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

fn remove_session_in(conn: &mut Connection, scope: &SessionScope) -> Result<(u64, bool)> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let result = delete_records(
        &tx,
        "session_file = ?1 OR (session_file >= ?2 AND session_file < ?3)",
        params![scope.root, scope.prefix, scope.upper],
    )?;
    tx.commit()?;
    Ok(result)
}

fn extract_root_session_id<'a>(prefix: &str, session_file: &'a str) -> Option<&'a str> {
    let relative = session_file.strip_prefix(prefix)?;
    let id = if let Some((root, _)) = relative.split_once(".subagents/sessions/") {
        root
    } else {
        relative.strip_suffix(".jsonl")?
    };
    (!id.is_empty() && !id.contains('/')).then_some(id)
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
    fn stats_gc_orphan_rollup_and_delete_session() {
        {
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
        {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("stats.db");
            let sessions = dir.path().join("sessions");
            let mut store = open_store(&db_path).unwrap();
            let root_a =
                sessions.join("2026-10-17T01-02-03-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.jsonl");
            let child_a = sessions.join(
            "2026-10-17T01-02-03-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.subagents/sessions/child.jsonl",
        );
            let child_b = sessions.join(
            "2026-10-17T01-02-03-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.subagents/sessions/child.jsonl",
        );
            let root_c =
                sessions.join("2026-10-17T01-02-03-cccccccccccccccccccccccccccccccc.jsonl");
            let outside = dir
                .path()
                .join("other/2026-10-17T01-02-03-dddddddddddddddddddddddddddddddd.jsonl");
            let adjacent = dir
                .path()
                .join("sessions-other/2026-10-17T01-02-03-eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.jsonl");

            for (key, file) in [
                ("k-none", None),
                ("k-a1", Some(&root_a)),
                ("k-a2", Some(&child_a)),
                ("k-out", Some(&outside)),
                ("k-adj", Some(&adjacent)),
            ] {
                let mut rec = fixture(key, 1000);
                rec.session_file = file.map(|p| p.to_string_lossy().into_owned());
                store.insert(&rec, &serde_json::Value::Null).unwrap();
            }
            store
                .tool(&ToolCall {
                    request_key: Some("k-a2"),
                    session_file: &child_b,
                    call_id: "c1",
                    tool: "read",
                    status: ToolStatus::Ok,
                    error_message: None,
                    timestamp: 1000,
                    duration_ms: 1.0,
                    result_chars: None,
                })
                .unwrap();
            store
                .hook(&crate::HookRun {
                    session_file: &root_c,
                    cwd: dir.path(),
                    event: pp_config::config::HookEvent::Stop,
                    tool: None,
                    status: crate::HookStatus::Ok,
                    modified: false,
                    timestamp: 1000,
                    duration_ms: 1.0,
                })
                .unwrap();
            drop(store);

            let gc = GarbageCollection::open_at(&db_path).unwrap();
            assert_eq!(
                gc.orphan_session_ids(&sessions).unwrap(),
                vec![
                    "2026-10-17T01-02-03-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                    "2026-10-17T01-02-03-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
                    "2026-10-17T01-02-03-cccccccccccccccccccccccccccccccc".to_owned(),
                ]
            );
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("stats.db");
            let database = super::super::Database::new(db_path.clone());
            let root = dir.path().join("sessions/root.jsonl");
            let child = dir
                .path()
                .join("sessions/root.subagents/sessions/child.jsonl");
            let keep = dir.path().join("sessions/keep.jsonl");

            // Never create an empty database when the stats database is missing.
            GarbageCollection::delete_session(&database, &root).unwrap();
            assert!(!db_path.exists());

            let mut store = open_store(&db_path).unwrap();
            for (key, file, payload) in [
                ("r1", &root, Some(format!("\"{}\"", "x".repeat(128 * 1024)))),
                ("c1", &child, None),
                ("k1", &keep, None),
            ] {
                let mut rec = fixture(key, 1000);
                rec.session_file = Some(file.to_string_lossy().into_owned());
                let snapshot = payload.map(|text| crate::RequestSnapshot {
                    request: serde_json::json!({ "text": text }),
                    skills: Vec::new(),
                });
                store
                .insert_with_context(
                    &rec,
                    &serde_json::json!([{"type":"function_call","name":"read","arguments":"{}"}]),
                    snapshot.as_ref(),
                )
                .unwrap();
            }
            for (idx, file) in [&root, &child, &keep].into_iter().enumerate() {
                store
                    .tool(&ToolCall {
                        request_key: Some(if idx == 2 { "k1" } else { "r1" }),
                        session_file: file,
                        call_id: "call",
                        tool: "read",
                        status: ToolStatus::Ok,
                        error_message: None,
                        timestamp: 1000,
                        duration_ms: 1.0,
                        result_chars: None,
                    })
                    .unwrap();
                store
                    .hook(&crate::HookRun {
                        session_file: file,
                        cwd: dir.path(),
                        event: pp_config::config::HookEvent::Stop,
                        tool: None,
                        status: crate::HookStatus::Ok,
                        modified: false,
                        timestamp: 1000,
                        duration_ms: 1.0,
                    })
                    .unwrap();
            }
            drop(store);

            GarbageCollection::delete_session(&database, &root).unwrap();
            let store = open_store(&db_path).unwrap();
            for table in ["requests", "tool_calls", "hook_runs"] {
                let remaining: Vec<String> = store
                    .conn
                    .prepare(&format!("SELECT session_file FROM {table}"))
                    .unwrap()
                    .query_map([], |row| row.get(0))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                assert_eq!(
                    remaining,
                    vec![keep.to_string_lossy().into_owned()],
                    "{table}"
                );
            }
            let ctx_count: i64 = store
                .conn
                .query_row("SELECT COUNT(*) FROM request_contexts", [], |r| r.get(0))
                .unwrap();
            assert_eq!(ctx_count, 0);
        }
    }
}
