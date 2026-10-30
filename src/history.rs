//! history.db（`~/.pp/agent/history.db`）的持久输入与会话派生索引存储。
//!
//! 安全打开、WAL/外键/FULL/250ms 由 `crate::sqlite::open` 统一负责；本模块只拥有
//! history.db 的固定 schema 与领域操作。所有存储错误都转换为不含 SQL 参数与正文
//! 的固定领域提示。`input_history` 是独立持久数据（已接受的原始输入原文），
//! `sessions`/`session_prompts` 及其 FTS 是会话日志的派生投影，可随时由
//! `sync_sessions`（persistence 索引模块）重建；派生行的删除绝不触碰 `input_history`。

use crate::agent::persistence::SessionSummary;
use crate::cancellation::CancellationToken;
use anyhow::{Result, ensure};
use rusqlite::{Connection, TransactionBehavior, params_from_iter, types::Value as SqlValue};
use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

const UNAVAILABLE: &str = "History database unavailable.";
const UNSUPPORTED_VERSION: &str = "History database schema version is not supported.";
const CORRUPT: &str = "History database is corrupt.";
const BLANK_INPUT: &str = "History input text is blank.";

/// `sessions`/`session_prompts` 之外，两个 FTS 虚拟表也属于必要 schema；
/// user_version=1 的既有文件缺失其中任意对象一律按损坏拒绝，不当空库重建。
const REQUIRED_TABLES: &[&str] = &[
    "input_history",
    "sessions",
    "session_prompts",
    "session_fts",
    "prompt_fts",
];

const SCHEMA: &str = "
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

/// 一条已接受的持久输入原文；`id` 为递增游标，供分页读取。
pub(crate) struct InputEntry {
    pub id: i64,
    pub text: String,
}

pub(crate) struct Store {
    /// 后续 `src/agent/persistence/index.rs` 需在同一连接上执行逐文件事务。
    pub(crate) conn: Connection,
}

fn unavailable<E>(_: E) -> anyhow::Error {
    anyhow::anyhow!(UNAVAILABLE)
}

impl Store {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let mut conn = crate::sqlite::open(path).map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(unavailable)?;
        ensure!(version <= 1, UNSUPPORTED_VERSION);
        if version < 1 {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(unavailable)?;
            // 在写锁内重读版本，避免与并发初始化竞争。
            let version: i64 = tx
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(unavailable)?;
            ensure!(version <= 1, UNSUPPORTED_VERSION);
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
        Ok(Self { conn })
    }

    /// 记录一条已接受的原始输入，返回其游标 `id`；空白文本拒绝，
    /// 重复文本保留为独立事件（无 UNIQUE 约束、不去重）。
    pub(crate) fn append_input(
        &mut self,
        cwd: &Path,
        text: &str,
        created_at_ms: i64,
    ) -> Result<i64> {
        ensure!(!text.trim().is_empty(), BLANK_INPUT);
        let cwd = cwd.to_string_lossy();
        self.conn
            .execute(
                "INSERT INTO input_history(cwd,text,created_at_ms) VALUES (?1,?2,?3)",
                rusqlite::params![cwd.as_ref(), text, created_at_ms],
            )
            .map_err(unavailable)?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 按 `cwd` 读取一页输入。`before_id` 为上一页返回的最小 `id`（None 表示首页）；
    /// 固定按 id DESC 取页，页内翻转为旧到新返回。调用方控制页大小（当前 100）。
    pub(crate) fn load_inputs(
        &self,
        cwd: &Path,
        before_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<InputEntry>> {
        let cwd = cwd.to_string_lossy();
        let mut statement = self
            .conn
            .prepare(
                "SELECT id,text FROM input_history
                 WHERE cwd=?1 AND (?2 IS NULL OR id<?2)
                 ORDER BY id DESC LIMIT ?3",
            )
            .map_err(unavailable)?;
        let rows = statement
            .query_map(
                rusqlite::params![cwd.as_ref(), before_id, limit as i64],
                |row| {
                    Ok(InputEntry {
                        id: row.get(0)?,
                        text: row.get(1)?,
                    })
                },
            )
            .map_err(unavailable)?;
        let mut entries = Vec::with_capacity(limit);
        for entry in rows {
            entries.push(entry.map_err(unavailable)?);
        }
        entries.reverse();
        Ok(entries)
    }

    /// 同步会话目录投影：委托 persistence 索引模块在同一连接上执行逐文件事务，
    /// 返回逐文件脱敏诊断。取消时由索引模块回滚当前文件事务并返回 Cancelled。
    pub(crate) fn sync_sessions(
        &mut self,
        dir: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Vec<String>> {
        crate::agent::persistence::index::sync(&mut self.conn, dir, cancellation)
    }

    /// 会话目录检索。`cwd` 为 None 表示全部项目，否则按绝对路径精确相等过滤；
    /// 空查询返回 scope 内全部目录行。所有词都必须命中，且同一个词可命中
    /// summary/preview/id/cwd 或该会话任意一条 prompt（各词独立判断，
    /// 不要求位于同一条消息）。结果按 (updated_at_secs, nanos, session_id) 降序。
    pub(crate) fn search_sessions(
        &self,
        cwd: Option<&Path>,
        query: &str,
    ) -> Result<Vec<SessionSummary>> {
        let folded = query.to_lowercase();
        let mut values = vec![cwd.map_or(SqlValue::Null, |cwd| {
            SqlValue::Text(cwd.to_string_lossy().into_owned())
        })];
        let mut sql = String::from(
            "SELECT s.session_id,s.cwd,s.updated_at_secs,s.updated_at_nanos,
                    s.summary,s.preview,s.bytes,s.done FROM sessions s
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
            // validate_id 限定 ASCII ID，唯一的大写字符是固定日期分隔符 T。
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
        sql.push_str(" ORDER BY s.updated_at_secs DESC,s.updated_at_nanos DESC,s.session_id DESC");
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
                ))
            })
            .map_err(unavailable)?;
        let mut summaries = Vec::new();
        for row in rows {
            let (id, cwd, secs, nanos, summary, preview, bytes, done) = row.map_err(unavailable)?;
            let entry = SessionSummary {
                id,
                cwd: PathBuf::from(cwd),
                updated_at: to_system_time(secs, nanos)?,
                summary,
                preview,
                bytes: u64::try_from(bytes).map_err(unavailable)?,
                done,
            };
            ensure!(entry.cwd.is_absolute(), UNAVAILABLE);
            summaries.push(entry);
        }
        Ok(summaries)
    }

    /// 验收帮助（仅测试）：删除派生表数据后重新同步，验证投影可完整重建
    /// 且不触碰 input_history。DELETE sessions 经外键级联清空 prompts，
    /// 触发器同步移除 FTS 文档。
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

/// FTS5 双引号 literal phrase：内部双引号加倍转义，保证 OR/引号/操作符按字面处理。
fn fts_phrase(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

/// 目录行的秒/纳秒整数还原为 SystemTime；纳秒来自 duration 的亚秒部分，非负。
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
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn temp_store(name: &str) -> (tempfile::TempDir, Store) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(name);
        let store = Store::open(&path).unwrap();
        (root, store)
    }

    fn cwd(root: &Path, name: &str) -> std::path::PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn match_rows(conn: &Connection, fts: &str, phrase: &str) -> Vec<i64> {
        let sql = format!("SELECT rowid FROM {fts} WHERE {fts} MATCH ?1 ORDER BY rowid");
        let mut statement = conn.prepare(&sql).unwrap();
        let rows = statement
            .query_map([phrase], |row| row.get::<_, i64>(0))
            .unwrap();
        rows.map(|row| row.unwrap()).collect()
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn sqlite_input_history_pagination_and_cwd_isolation() {
        let (root, mut store) = temp_store("history.db");
        let a = cwd(root.path(), "project-a");
        let b = cwd(root.path(), "project-b");

        for text in ["", "   ", "\n\t ", "  \u{3000}  "] {
            let error = store.append_input(&a, text, 0).unwrap_err();
            assert_eq!(error.to_string(), BLANK_INPUT);
        }

        let mut a_ids = Vec::new();
        for (index, text) in ["first slash", "second", "duplicate", "duplicate", "fifth"]
            .into_iter()
            .enumerate()
        {
            a_ids.push(store.append_input(&a, text, 1000 + index as i64).unwrap());
        }
        let mut b_ids = Vec::new();
        for index in 0..3 {
            b_ids.push(
                store
                    .append_input(&b, &format!("other {index}"), 2000 + index)
                    .unwrap(),
            );
        }

        // 首页：id DESC 取最新 limit 条，页内翻转为旧到新；重复文本保留为独立事件。
        let page = store.load_inputs(&a, None, 3).unwrap();
        assert_eq!(
            page.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            a_ids[2..5].to_vec()
        );
        assert_eq!(
            page.iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            ["duplicate", "duplicate", "fifth"]
        );

        // 全量读取按旧到新；两个 cwd 完全隔离。
        let all = store.load_inputs(&a, None, 100).unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(
            all.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            a_ids.clone()
        );
        assert_eq!(
            all.iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            ["first slash", "second", "duplicate", "duplicate", "fifth"]
        );
        assert_eq!(
            store
                .load_inputs(&b, None, 100)
                .unwrap()
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            b_ids
        );
        let empty = cwd(root.path(), "project-c");
        assert!(store.load_inputs(&empty, None, 100).unwrap().is_empty());

        // 游标分页：before_id 为上一页最小 id，返回其之前的一页（旧到新）。
        let older = store.load_inputs(&a, Some(a_ids[3]), 2).unwrap();
        assert_eq!(
            older.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            a_ids[1..3].to_vec()
        );
        let exhausted = store.load_inputs(&a, Some(a_ids[0]), 10).unwrap();
        assert!(exhausted.is_empty());
        assert!(store.load_inputs(&a, None, 0).unwrap().is_empty());

        // 超过一页：150 条输入分两页取完，内容按写入顺序还原。
        let c = cwd(root.path(), "project-c-long");
        for index in 0..150 {
            store
                .append_input(&c, &format!("line {index}"), 3000 + index)
                .unwrap();
        }
        let first = store.load_inputs(&c, None, 100).unwrap();
        assert_eq!(first.len(), 100);
        assert_eq!(first[0].text, "line 50");
        assert_eq!(first[99].text, "line 149");
        let second = store.load_inputs(&c, Some(first[0].id), 100).unwrap();
        assert_eq!(second.len(), 50);
        assert_eq!(second[0].text, "line 0");
        assert_eq!(second[49].text, "line 49");
    }

    #[test]
    fn sqlite_concurrent_input_append() {
        let (root, mut store) = temp_store("history.db");
        let a = cwd(root.path(), "project-a");
        for index in 0..25 {
            store
                .append_input(&a, &format!("main {index}"), 1000 + index)
                .unwrap();
        }
        let path = root.path().join("history.db");
        let worker = std::thread::spawn(move || {
            let mut store = Store::open(&path).unwrap();
            let b = path.parent().unwrap().join("project-b");
            fs::create_dir_all(&b).unwrap();
            for index in 0..25 {
                store
                    .append_input(&b, &format!("side {index}"), 2000 + index)
                    .unwrap();
            }
        });
        worker.join().unwrap();
        assert_eq!(count(&store.conn, "SELECT COUNT(*) FROM input_history"), 50);
        assert_eq!(store.load_inputs(&a, None, 100).unwrap().len(), 25);
        assert_eq!(
            store
                .load_inputs(&root.path().join("project-b"), None, 100)
                .unwrap()
                .len(),
            25
        );
    }

    #[test]
    fn sqlite_rejects_unsupported_or_foreign_schema() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("history.db");
        {
            let store = Store::open(&path).unwrap();
            store.conn.pragma_update(None, "user_version", 2).unwrap();
        }
        let error = Store::open(&path).err().unwrap();
        assert_eq!(error.to_string(), UNSUPPORTED_VERSION);

        let foreign = root.path().join("foreign.db");
        fs::write(&foreign, "not a database at all").unwrap();
        fs::set_permissions(&foreign, fs::Permissions::from_mode(0o600)).unwrap();
        let error = Store::open(&foreign).err().unwrap();
        assert_eq!(error.to_string(), UNAVAILABLE);
    }

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

        // trigram 短语匹配：长词命中（含仅出现在第二条 prompt 的词）。
        assert_eq!(match_rows(&store.conn, "prompt_fts", "\"模型切\""), [2]);
        assert_eq!(match_rows(&store.conn, "session_fts", "\"示例标题\""), [1]);
        assert_eq!(match_rows(&store.conn, "session_fts", "\"预览内容\""), [1]);
        assert_eq!(
            match_rows(&store.conn, "session_fts", "\"project-a\"").len(),
            1
        );

        // 更新触发器：先删 OLD 再插 NEW，新旧内容检索一致。
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

        // 直接删除单条 prompt：FTS 文档同步移除。
        store
            .conn
            .execute(
                "DELETE FROM session_prompts WHERE session_rowid=?1 AND entry_offset=1",
                [session_rowid],
            )
            .unwrap();
        assert!(match_rows(&store.conn, "prompt_fts", "\"needle_42\"").is_empty());

        // 删除会话：FK 级联清空 prompts，两个 FTS 索引无残留；input_history 不受影响。
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

        // 重建派生投影后的索引一致性验收：外部内容 FTS 的 integrity-check 必须通过。
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

    fn write_session(
        dir: &Path,
        id: &str,
        cwd: &Path,
        mtime: SystemTime,
        messages: &[serde_json::Value],
    ) {
        let mut text = serde_json::json!({
            "type": "session",
            "version": 3,
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
        fs::write(&path, text).unwrap();
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

        // A：首条摘要/助手预览均无关键词，第二条 user 才包含 needle。
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
        // B：同一项目较新文件；prompt 覆盖 %、_、引号、OR 等字面输入。
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
        // C：另一项目；用于 scope 隔离与纳秒级排序。
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

        // 空查询返回 scope 内全部行；目录行摘要/预览/done/纳秒更新时间还原一致。
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

        // 关键词只在第二条 user prompt：2 字符词走 instr，3 字符词走 FTS 候选。
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

        // 多词 AND，且两个词分布在不同消息（摘要 + 第二条 prompt）。
        assert_eq!(
            summary_ids(&store.search_sessions(None, "running needle_42").unwrap()),
            std::slice::from_ref(&id_a)
        );
        // Unicode 大小写折叠：查询大小写与原文无关。
        assert_eq!(
            summary_ids(&store.search_sessions(None, "RUNNING").unwrap()),
            std::slice::from_ref(&id_a)
        );
        assert_eq!(
            summary_ids(&store.search_sessions(None, "Running").unwrap()),
            std::slice::from_ref(&id_a)
        );
        // 1 字符短词走 instr，不因长度返回假空。
        assert_eq!(
            summary_ids(&store.search_sessions(None, "模").unwrap()),
            [id_b.clone(), id_a.clone(), id_c.clone()]
        );
        // %、_、OR、引号、NOT 均按字面处理，绝不作为查询语法。
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

        // 派生投影可完整重建：清空 sessions 级联后 rebuild 恢复检索，input_history 不动。
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

        // SQLite/FTS 查询错误必须展示检索失败，不可静默降级为只搜标题。
        store.conn.execute("DROP TABLE prompt_fts", []).unwrap();
        let error = store.search_sessions(None, "needle_42").err().unwrap();
        assert_eq!(error.to_string(), UNAVAILABLE);
    }
}
