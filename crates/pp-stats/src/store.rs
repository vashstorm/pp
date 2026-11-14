use super::{
    COLUMNS, CostSnapshot, Range, RequestRow, RequestSnapshot, SessionScope, ToolCall, call_tools,
    ensure_schema, latest_sql, row, session_requests_sql,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    fs::{self, DirBuilder, OpenOptions},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) struct Store {
    pub conn: Connection,
    pub(super) models_path: PathBuf,
}
impl Store {
    pub(super) fn session_cost(
        &self,
        session: &Path,
    ) -> Result<pp_config::task_status::HistoricalCost> {
        let scope = SessionScope::new(
            session.to_string_lossy().into_owned(),
            format!(
                "{}/",
                session
                    .with_extension("subagents")
                    .join("sessions")
                    .display()
            ),
        )?;
        let mut statement = self.conn.prepare(&session_requests_sql("cost_json"))?;
        let mut rows = statement.query(params![scope.root, scope.prefix, scope.upper])?;
        let mut result = pp_config::task_status::HistoricalCost::default();
        while let Some(row) = rows.next()? {
            let cost: Option<String> = row.get(0)?;
            match cost.as_deref().map(serde_json::from_str::<CostSnapshot>) {
                Some(Ok(cost)) if cost.total().is_finite() => {
                    result.usd = Some(result.usd.unwrap_or(0.0) + cost.total());
                }
                // NULL 或损坏的快照：只标记覆盖不完整，不臆造金额。
                _ => result.incomplete = true,
            }
        }
        Ok(result)
    }

    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_models(path, pp_config::config::global_dir()?.join("models.yml"))
    }
    pub(super) fn open_with_models(path: &Path, models_path: PathBuf) -> Result<Self> {
        let dir = path.parent().context("statistics directory unavailable")?;
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        ensure!(
            !fs::symlink_metadata(dir)?.file_type().is_symlink(),
            "statistics directory must not be a symlink"
        );
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path)?;
        ensure!(
            file.metadata()?.is_file(),
            "statistics database must be a file"
        );
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        ensure_schema(&mut conn)?;
        Ok(Self { conn, models_path })
    }
    #[cfg(test)]
    pub fn insert(&mut self, record: &RequestRow, output: &Value) -> Result<()> {
        self.insert_with_context(record, output, None)
    }
    pub fn insert_with_context(
        &mut self,
        record: &RequestRow,
        output: &Value,
        context: Option<&RequestSnapshot>,
    ) -> Result<()> {
        let cost_json = record
            .cost
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let rate_json = record
            .rates
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let tx = self.conn.transaction()?;
        let inserted = tx.execute("INSERT INTO requests (request_key,timestamp,model,provider,model_reference,api,project,cwd,purpose,session_file,duration_ms,ttft_ms,input_tokens,output_tokens,cached_tokens,status,stop_reason,error_message,cache_write_tokens,cost_json,rate_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21) ON CONFLICT(request_key) DO NOTHING",params![record.request_key,record.timestamp,record.model,record.provider,record.model_reference,record.api,record.project,record.cwd,record.purpose,record.session_file,record.duration_ms,record.ttft_ms,record.input_tokens,record.output_tokens,record.cached_tokens,record.status,record.stop_reason,record.error_message,record.cache_write_tokens,cost_json,rate_json])?;
        if inserted != 0 {
            let request_id = tx.last_insert_rowid();
            for (tool, chars) in call_tools(output) {
                tx.execute(
                    "INSERT INTO request_tools(request_id,tool,args_chars) VALUES (?1,?2,?3)",
                    params![
                        request_id,
                        tool,
                        i64::try_from(chars).context("tool argument length exceeds i64")?
                    ],
                )?;
            }
            if let Some(context) = context {
                tx.execute(
                    "INSERT INTO request_contexts(request_id,request_json,skills_json) VALUES (?1,?2,?3)",
                    params![
                        request_id,
                        serde_json::to_string(&context.request)?,
                        serde_json::to_string(&context.skills)?
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub fn link(&mut self, key: &str, file: &Path) -> Result<()> {
        self.conn.execute(
            "UPDATE requests SET session_file=?1 WHERE request_key=?2",
            params![file.to_string_lossy(), key],
        )?;
        Ok(())
    }
    /// 幂等写入一次工具执行：同一会话内同一个 call ID 只保留最后一次结果。
    pub(crate) fn tool(&mut self, call: &ToolCall<'_>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tool_calls (request_key,session_file,call_id,tool,timestamp,status,error_message,duration_ms,result_chars)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(session_file,call_id) DO UPDATE SET
             request_key=COALESCE(excluded.request_key,tool_calls.request_key),tool=excluded.tool,
             timestamp=excluded.timestamp,status=excluded.status,
             error_message=excluded.error_message,duration_ms=excluded.duration_ms,result_chars=excluded.result_chars",
            params![
                call.request_key,
                call.session_file.to_string_lossy(),
                call.call_id,
                call.tool,
                call.timestamp,
                call.status.as_str(),
                call.error_message,
                call.duration_ms,
                call.result_chars.map(i64::try_from).transpose()?
            ],
        )?;
        Ok(())
    }
    pub fn latest(&self, since: i64, until: i64, limit: usize) -> Result<Vec<RequestRow>> {
        Ok(self
            .latest_with_context(since, until, limit)?
            .into_iter()
            .map(|(row, _)| row)
            .collect())
    }
    pub fn latest_with_context(
        &self,
        since: i64,
        until: i64,
        limit: usize,
    ) -> Result<Vec<(RequestRow, bool)>> {
        Ok(self
            .conn
            .prepare(&latest_sql())?
            .query_map(params![since, until, limit as i64], |r| {
                Ok((row(r)?, r.get(23)?))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn detail(&self, id: i64) -> Result<Option<Value>> {
        let record = self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM requests WHERE id=?1"),
                [id],
                row,
            )
            .optional()?;
        let Some(record) = record else {
            return Ok(None);
        };
        let context: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT request_json,skills_json FROM request_contexts WHERE request_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let request_context = context
            .map(|(request, skills)| -> Result<RequestSnapshot> {
                Ok(RequestSnapshot {
                    request: serde_json::from_str(&request)?,
                    skills: skills
                        .map(|skills| serde_json::from_str(&skills))
                        .transpose()?
                        .unwrap_or_default(),
                })
            })
            .transpose()?;
        Ok(Some(
            json!({"request":record.value()?,"request_context":request_context}),
        ))
    }
    pub fn overview(&self, range: Range, now: i64) -> Result<Value> {
        let mut value = self.analytics(range, now)?;
        value["latest"] = json!(
            self.latest(range.since(now), now, 12)?
                .iter()
                .map(|r| r.value())
                .collect::<Result<Vec<_>>>()?
        );
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use std::time::Instant;

    #[test]
    fn stats_creation_reopen_dedup_and_snapshot_are_transactional() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats/stats.db");
        let mut store = open_store(&path).unwrap();
        let record = fixture("one", 1000);
        let snapshot = RequestSnapshot {
            request: json!({"instructions":"hello"}),
            skills: Vec::new(),
        };
        store
            .insert_with_context(
                &record,
                &json!({"tool_calls":[{"function":{"name":"read","arguments":"abc"}}]}),
                Some(&snapshot),
            )
            .unwrap();
        store
            .insert_with_context(
                &record,
                &json!({"tool_calls":[{"function":{"name":"bash","arguments":"x"}}]}),
                None,
            )
            .unwrap();
        store.link("one", Path::new("/tmp/session.jsonl")).unwrap();
        drop(store);
        let store = open_store(&path).unwrap();
        assert_eq!(store.latest(0, 2000, 500).unwrap().len(), 1);
        let detail = store.detail(1).unwrap().unwrap();
        assert_eq!(
            detail["request_context"],
            serde_json::to_value(&snapshot).unwrap()
        );
        assert!(detail.get("output_message").is_none());
        assert!(detail.get("session_entry").is_none());
        // 重复键不再补写工具行；entry_id 不再由 link 写入。
        assert_eq!(
            store
                .conn
                .query_row("SELECT tool FROM request_tools", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "read"
        );
        assert!(detail["request"]["entry_id"].is_null());
        assert_eq!(detail["request"]["session_file"], "/tmp/session.jsonl");
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            store
                .conn
                .pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
    }
    #[test]
    fn stats_multiple_connections_read_during_write_and_bound_lock_wait() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        open_store(&path).unwrap();
        std::thread::scope(|scope| {
            for i in 0..4 {
                let path = &path;
                scope.spawn(move || {
                    let mut store = open_store(path).unwrap();
                    for j in 0..15 {
                        store
                            .insert(&fixture(&format!("{i}-{j}"), 1000), &Value::Null)
                            .unwrap();
                    }
                });
            }
        });
        let mut writer = open_store(&path).unwrap();
        let mut reader = open_store(&path).unwrap();
        assert_eq!(reader.latest(0, 2000, 500).unwrap().len(), 60);
        let tx = writer
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("UPDATE requests SET project='pending'", [])
            .unwrap();
        assert_eq!(reader.latest(0, 2000, 500).unwrap()[0].project, "project");
        let started = Instant::now();
        assert!(
            reader
                .insert(&fixture("locked", 1000), &Value::Null)
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(tx);
        reader
            .insert(&fixture("unlocked", 1000), &Value::Null)
            .unwrap();
    }
    #[test]
    fn stats_symlinks_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        open_store(&path).unwrap();
        let link = dir.path().join("link.db");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(open_store(&link).is_err());
    }
}
