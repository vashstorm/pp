use super::schema::{Store, UNAVAILABLE, unavailable};
use crate::agent::persistence::SessionSummary;
use anyhow::{Result, ensure};
use rusqlite::{TransactionBehavior, params_from_iter, types::Value as SqlValue};
use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

impl Store {
    /// Session catalog search. `cwd` of None means all projects, otherwise exact absolute-path equality;
    /// an empty query returns every catalog row in scope. All words must hit, and one word may hit
    /// summary/preview/id/cwd or any single prompt of that session (each word is judged independently,
    /// not required to be in the same message). Pinned first; within a group, (updated_at_secs, nanos, session_id) descending.
    pub fn search_sessions(&self, cwd: Option<&Path>, query: &str) -> Result<Vec<SessionSummary>> {
        let folded = query.to_lowercase();
        let mut values = vec![cwd.map_or(SqlValue::Null, |cwd| {
            SqlValue::Text(cwd.to_string_lossy().into_owned())
        })];
        let mut sql = String::from(
            "SELECT s.session_id,s.cwd,s.updated_at_secs,s.updated_at_nanos,
                    s.summary,s.preview,s.bytes,s.done,
                    EXISTS(SELECT 1 FROM session_pins WHERE session_id=s.session_id) AS pinned
             FROM sessions s
             WHERE (?1 IS NULL OR s.cwd=?1)",
        );
        for word in folded.split_whitespace() {
            values.push(SqlValue::Text(word.to_owned()));
            let parameter = values.len();
            if word.chars().count() >= 3 {
                values.push(SqlValue::Text(fts_phrase(word)));
                let phrase = values.len();
                write!(sql,
                    " AND (s.rowid IN (SELECT rowid FROM session_fts WHERE session_fts MATCH ?{phrase})
                       OR s.rowid IN (SELECT p.session_rowid FROM session_prompts p
                           JOIN prompt_fts ON prompt_fts.rowid=p.rowid WHERE prompt_fts MATCH ?{phrase}))"
                ).expect("writing to String");
            }
            // validate_id restricts IDs to ASCII; the only uppercase character is the fixed date separator T.
            write!(
                sql,
                " AND (instr(s.summary_folded,?{parameter})>0
                    OR instr(s.preview_folded,?{parameter})>0
                    OR instr(s.cwd_folded,?{parameter})>0
                    OR instr(replace(s.session_id,'T','t'),?{parameter})>0
                    OR EXISTS(SELECT 1 FROM session_prompts p
                        WHERE p.session_rowid=s.rowid AND instr(p.text_folded,?{parameter})>0))"
            )
            .expect("writing to String");
        }
        sql.push_str(" ORDER BY pinned DESC,s.updated_at_secs DESC,s.updated_at_nanos DESC,s.session_id DESC");
        let mut statement = self.conn.prepare(&sql).map_err(unavailable)?;
        let rows = statement
            .query_map(params_from_iter(values.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, bool>(7)?,
                    row.get::<_, bool>(8)?,
                ))
            })
            .map_err(unavailable)?;
        let mut summaries = Vec::new();
        for row in rows {
            let (id, cwd, secs, nanos, summary, preview, bytes, done, pinned) =
                row.map_err(unavailable)?;
            let entry = SessionSummary {
                id,
                cwd: PathBuf::from(cwd),
                updated_at: to_system_time(secs, nanos)?,
                summary,
                preview,
                bytes: u64::try_from(bytes).map_err(unavailable)?,
                done,
                pinned,
            };
            ensure!(entry.cwd.is_absolute(), UNAVAILABLE);
            summaries.push(entry);
        }
        Ok(summaries)
    }

    /// Changes user preference only, never rewriting the session log or its timestamp; an explicit target value keeps repeated requests idempotent.
    pub fn set_session_pinned(&mut self, id: &str, pinned: bool) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(unavailable)?;
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id=?1)",
                [id],
                |row| row.get(0),
            )
            .map_err(unavailable)?;
        ensure!(exists, "Saved session is no longer indexed.");
        if pinned {
            tx.execute(
                "INSERT INTO session_pins(session_id) VALUES (?1) ON CONFLICT DO NOTHING",
                [id],
            )
            .map_err(unavailable)?;
        } else {
            tx.execute("DELETE FROM session_pins WHERE session_id=?1", [id])
                .map_err(unavailable)?;
        }
        tx.commit().map_err(unavailable)
    }
}

/// FTS5 double-quoted literal phrase: inner double quotes are doubled so OR/quotes/operators stay literal.
fn fts_phrase(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

/// Restores a catalog row's seconds/nanoseconds into a SystemTime; nanoseconds come from the subsecond part, non-negative.
fn to_system_time(secs: i64, nanos: i64) -> Result<SystemTime> {
    ensure!((0..1_000_000_000).contains(&nanos), UNAVAILABLE);
    let base = if secs >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))
    };
    base.and_then(|time| time.checked_add(Duration::from_nanos(nanos as u64)))
        .ok_or_else(|| anyhow::anyhow!(UNAVAILABLE))
}

#[cfg(test)]
mod tests {
    use super::super::schema::SCHEMA;
    use super::*;
    use pp_core::cancellation::CancellationToken;
    use std::fs;
    use std::path::Path;

    fn write_session(
        dir: &Path,
        id: &str,
        cwd: &Path,
        mtime: SystemTime,
        messages: &[serde_json::Value],
    ) {
        let mut text = serde_json::json!({
            "type": "session",
            "id": id,
            "timestamp": "2026-10-30T12:00:00Z",
            "cwd": cwd,
        })
        .to_string();
        text.push('\n');
        for (index, message) in messages.iter().enumerate() {
            let entry = serde_json::json!({
                "id": format!("e{}", index + 1),
                "parentId": if index == 0 {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::String(format!("e{}", index))
                },
                "timestamp": "2026-10-30T12:00:00Z",
                "type": "message",
                "message": message,
            });
            text.push_str(&entry.to_string());
            text.push('\n');
        }
        let path = dir.join(format!("{id}.jsonl"));
        crate::agent::persistence::tests::write_log(&path, &text);
        let file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(mtime))
            .unwrap();
    }

    fn user_message(content: &str) -> serde_json::Value {
        serde_json::json!({ "role": "user", "content": content })
    }

    fn assistant_message(text: &str, stop: bool) -> serde_json::Value {
        let mut message = serde_json::json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": text }],
        });
        if stop {
            message["stopReason"] = serde_json::json!("stop");
        }
        message
    }

    fn summary_ids(summaries: &[SessionSummary]) -> Vec<String> {
        summaries.iter().map(|entry| entry.id.clone()).collect()
    }

    #[test]
    fn sqlite_session_pins_migrate_persist_and_survive_index_rebuild() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("history.db");
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        // Upgrade from the real v2 schema, keeping existing input and session index.
        let conn = pp_core::sqlite::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(
            "ALTER TABLE input_history ADD COLUMN images TEXT NOT NULL DEFAULT '[]';",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        conn.execute(
            "INSERT INTO input_history(cwd,text,created_at_ms) VALUES ('/project','kept input',0)",
            [],
        )
        .unwrap();
        drop(conn);
        let mut store = Store::open(&path).unwrap();
        let ids: Vec<_> = (1..=3)
            .map(|index| format!("2026-10-30T12-00-0{index}-0123456789abcdef0123456789abcdef"))
            .collect();
        let project = Path::new("/project");
        for (index, id) in ids.iter().enumerate() {
            write_session(
                &sessions,
                id,
                if index == 1 {
                    Path::new("/other")
                } else {
                    project
                },
                SystemTime::UNIX_EPOCH + Duration::from_secs(1000 + index as u64),
                &[
                    user_message(&format!("common title {index}")),
                    assistant_message("ok", true),
                ],
            );
        }
        store
            .sync_sessions(&sessions, &CancellationToken::new())
            .unwrap();
        let journal = sessions.join(format!("{}.jsonl", ids[0]));
        let original = fs::read(&journal).unwrap();
        let modified = fs::metadata(&journal).unwrap().modified().unwrap();
        store.set_session_pinned(&ids[0], true).unwrap();
        store.set_session_pinned(&ids[0], true).unwrap();
        store.set_session_pinned(&ids[1], true).unwrap();
        assert_eq!(
            summary_ids(&store.search_sessions(None, "").unwrap()),
            [ids[1].clone(), ids[0].clone(), ids[2].clone()]
        );
        assert_eq!(
            summary_ids(&store.search_sessions(Some(project), "common").unwrap()),
            [ids[0].clone(), ids[2].clone()]
        );
        assert!(
            store.search_sessions(None, "absent").unwrap().is_empty(),
            "a pin cannot bypass the search filter"
        );
        drop(store);
        let mut store = Store::open(&path).unwrap();
        store.rebuild_sessions(&sessions).unwrap();
        let rows = store.search_sessions(None, "common").unwrap();
        assert_eq!(
            summary_ids(&rows),
            [ids[1].clone(), ids[0].clone(), ids[2].clone()]
        );
        assert!(rows[0].pinned && rows[1].pinned && !rows[2].pinned);
        assert_eq!(
            store.load_inputs(project, None, 100).unwrap()[0].text,
            "kept input"
        );
        store.set_session_pinned(&ids[0], false).unwrap();
        assert_eq!(
            summary_ids(&store.search_sessions(None, "").unwrap()),
            [ids[1].clone(), ids[2].clone(), ids[0].clone()]
        );
        assert_eq!(fs::read(&journal).unwrap(), original);
        assert_eq!(
            fs::metadata(&journal).unwrap().modified().unwrap(),
            modified
        );
        // A lost database lock race must not change already committed preferences.
        let blocker = pp_core::sqlite::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let error = store.set_session_pinned(&ids[1], false).unwrap_err();
        assert_eq!(error.to_string(), UNAVAILABLE);
        blocker.execute_batch("ROLLBACK").unwrap();
        assert!(store.search_sessions(None, "").unwrap()[0].pinned);
        assert!(store.set_session_pinned("missing", true).is_err());
    }

    #[test]
    fn sqlite_prompt_search_covers_full_history() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let database = tempfile::tempdir().unwrap();
        let mut store = Store::open(&database.path().join("history.db")).unwrap();
        let project_a = root.path().join("project-a");
        let project_b = root.path().join("project-b");
        fs::create_dir_all(&project_a).unwrap();
        fs::create_dir_all(&project_b).unwrap();
        let random = "0123456789abcdef0123456789abcdef";
        let id_a = format!("2026-10-30T12-00-01-{random}");
        let id_b = format!("2026-10-30T12-00-02-{random}");
        let id_c = format!("2026-10-30T12-00-03-{random}");
        let at = |secs: u64, nanos: u32| SystemTime::UNIX_EPOCH + Duration::new(secs, nanos);

        // A: neither the first summary nor the assistant preview has the keyword; only the second user message contains needle.
        write_session(
            &sessions,
            &id_a,
            &project_a,
            at(1000, 100_000_000),
            &[
                user_message("initialize the project setup RUNNING"),
                assistant_message("helpful preview text", false),
                user_message("修复模型切换失败 needle_42"),
                assistant_message("done", true),
            ],
        );
        // B: newer file in the same project; prompts cover literal input such as %, _, quotes and OR.
        write_session(
            &sessions,
            &id_b,
            &project_a,
            at(1000, 900_000_000),
            &[
                user_message("budget notes"),
                user_message("discount 50% for snake_case items or quote \"hi\" there"),
                user_message("模型 tuning plan"),
                assistant_message("ok", true),
            ],
        );
        // C: another project; used for scope isolation and nanosecond-level ordering.
        write_session(
            &sessions,
            &id_c,
            &project_b,
            at(999, 0),
            &[
                user_message("其它项目"),
                user_message("修复模型切换失败 elsewhere"),
                assistant_message("ok", true),
            ],
        );

        let diagnostics = store
            .sync_sessions(&sessions, &CancellationToken::new())
            .unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        // An empty query returns every row in scope; catalog summary/preview/done/nanosecond timestamp restore identically.
        let listed = store.search_sessions(Some(&project_a), "").unwrap();
        assert_eq!(summary_ids(&listed), [id_b.clone(), id_a.clone()]);
        let summary = &listed[1];
        assert_eq!(summary.summary, "initialize the project setup RUNNING");
        assert_eq!(summary.preview, "helpful preview text");
        assert!(summary.done);
        assert_eq!(summary.cwd, project_a);
        assert_eq!(summary.updated_at, at(1000, 100_000_000));
        assert_eq!(
            summary_ids(&store.search_sessions(None, "").unwrap()),
            [id_b.clone(), id_a.clone(), id_c.clone()]
        );

        // The keyword is only in the second user prompt: 2-char terms go through instr, 3-char terms through the FTS candidate.
        assert_eq!(
            summary_ids(
                &store
                    .search_sessions(Some(&project_a), "needle_42")
                    .unwrap()
            ),
            std::slice::from_ref(&id_a)
        );
        assert_eq!(
            summary_ids(&store.search_sessions(None, "模型").unwrap()),
            [id_b.clone(), id_a.clone(), id_c.clone()]
        );
        assert_eq!(
            summary_ids(&store.search_sessions(Some(&project_a), "模型").unwrap()),
            [id_b.clone(), id_a.clone()]
        );
        assert_eq!(
            summary_ids(&store.search_sessions(None, "模型切").unwrap()),
            [id_a.clone(), id_c.clone()]
        );
        assert_eq!(
            summary_ids(&store.search_sessions(Some(&project_b), "模型切").unwrap()),
            std::slice::from_ref(&id_c)
        );

        // Multi-word AND with the two words in different messages (summary + second prompt).
        assert_eq!(
            summary_ids(&store.search_sessions(None, "running needle_42").unwrap()),
            std::slice::from_ref(&id_a)
        );
        // Unicode case folding: query case is independent of the original text.
        assert_eq!(
            summary_ids(&store.search_sessions(None, "RUNNING").unwrap()),
            std::slice::from_ref(&id_a)
        );
        assert_eq!(
            summary_ids(&store.search_sessions(None, "Running").unwrap()),
            std::slice::from_ref(&id_a)
        );
        // A 1-char short term goes through instr and never returns a false empty result because of length.
        assert_eq!(
            summary_ids(&store.search_sessions(None, "模").unwrap()),
            [id_b.clone(), id_a.clone(), id_c.clone()]
        );
        // %, _, OR, quotes and NOT are handled literally and never as query syntax.
        assert_eq!(
            summary_ids(&store.search_sessions(None, "50%").unwrap()),
            std::slice::from_ref(&id_b)
        );
        assert_eq!(
            summary_ids(&store.search_sessions(None, "snake_case").unwrap()),
            std::slice::from_ref(&id_b)
        );
        assert_eq!(
            summary_ids(&store.search_sessions(None, "OR").unwrap()),
            std::slice::from_ref(&id_b)
        );
        assert_eq!(
            summary_ids(&store.search_sessions(None, "\"hi").unwrap()),
            std::slice::from_ref(&id_b)
        );
        assert_eq!(
            summary_ids(&store.search_sessions(None, "quote \"hi").unwrap()),
            std::slice::from_ref(&id_b)
        );
        assert!(
            store
                .search_sessions(None, "NOT needle_42")
                .unwrap()
                .is_empty()
        );
        assert!(store.search_sessions(None, "needle_43").unwrap().is_empty());

        // The derived projection rebuilds completely: clearing sessions with its cascade then rebuilding restores search while input_history is untouched.
        store.append_input(&project_a, "kept input", 1).unwrap();
        let diagnostics = store.rebuild_sessions(&sessions).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            summary_ids(&store.search_sessions(None, "needle_42").unwrap()),
            std::slice::from_ref(&id_a)
        );
        assert_eq!(
            summary_ids(&store.search_sessions(None, "模型切").unwrap()),
            [id_a.clone(), id_c.clone()]
        );
        assert_eq!(
            store.load_inputs(&project_a, None, 100).unwrap()[0].text,
            "kept input"
        );

        // SQLite/FTS query errors must surface the search failure, never silently degrade to a title-only search.
        store.conn.execute("DROP TABLE prompt_fts", []).unwrap();
        let error = store.search_sessions(None, "needle_42").err().unwrap();
        assert_eq!(error.to_string(), UNAVAILABLE);
    }
}
