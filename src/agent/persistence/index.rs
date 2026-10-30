//! 会话目录（JSONL）到 history.db 派生投影的增量索引。
//!
//! 只索引 `validate_id` 认可、NOFOLLOW/NONBLOCK 打开的普通 `.jsonl`：
//! fingerprint（device/inode/字节数/mtime 秒+纳秒）未变化的文件只 stat；
//! 变化的文件在单独的 BEGIN IMMEDIATE 事务中流式重解析，并完整替换该
//! 文件的目录行、prompt 行与 FTS 文档。只有 `type=message`、`role=user`
//! 的实际正文按起始 byte offset 进入 prompt 索引；native_append/
//! session_state 中的重复副本不索引。取消在逐文件/逐行边界检查：当前
//! 文件事务回滚并返回 Cancelled，已提交的文件保留。派生行的删除绝不
//! 触碰 `input_history`，也不修改原 JSONL。

use super::{
    parse_timestamp,
    summary::{self, Classified, Head},
    validate_id,
};
use crate::cancellation::{self, CancellationToken};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::{
    collections::HashSet,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

const UNAVAILABLE: &str = "History database unavailable.";

fn unavailable<E>(_: E) -> anyhow::Error {
    anyhow::anyhow!(UNAVAILABLE)
}

/// 同步 `dir` 下全部会话文件的派生投影，返回逐文件脱敏诊断（仅会话 ID
/// 与固定原因，不含日志正文、路径或参数）。
pub(crate) fn sync(
    conn: &mut Connection,
    dir: &Path,
    cancellation: &CancellationToken,
) -> Result<Vec<String>> {
    cancellation::check(cancellation)?;
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        // 目录缺失是成功判定：清空派生行，等待目录重新出现时重建。
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            conn.execute("DELETE FROM sessions", [])
                .map_err(unavailable)?;
            return Ok(Vec::new());
        }
        Err(error) => return Err(error).context("reading session directory"),
    };
    let mut diagnostics = Vec::new();
    let mut seen = HashSet::new();
    for entry in entries {
        // 迭代中途失败时不移除任何行：未遍历项不能当作已删除。
        let entry = entry.context("reading session directory")?;
        let Some(id) = session_id(&entry.file_name()) else {
            continue;
        };
        cancellation::check(cancellation)?;
        index_file(conn, &entry.path(), &id, cancellation, &mut diagnostics)?;
        seen.insert(id);
    }
    cancellation::check(cancellation)?;
    drop_absent(conn, dir, &seen, cancellation)?;
    Ok(diagnostics)
}

/// 有效的会话文件名：`.jsonl` 后缀 + `validate_id` 认可的会话 ID。
fn session_id(file_name: &OsStr) -> Option<String> {
    let id = file_name.to_str()?.strip_suffix(".jsonl")?;
    validate_id(id).ok()?;
    Some(id.to_owned())
}

/// NOFOLLOW/NONBLOCK 打开：拒绝符号链接，不阻塞在非常规文件上。
fn open_session(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)
}

/// 派生行的源文件 fingerprint：路径身份（device/inode）与内容边界
/// （字节数、mtime 秒+纳秒）。任一字段变化都触发该文件完整重解析；
/// 同一 inode 修改旧正文并增长时仅看增长会留下错误索引，因此采用
/// 按变更文件整体替换而不是追加。
#[derive(Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    device: i64,
    inode: i64,
    bytes: i64,
    secs: i64,
    nanos: i64,
}

impl Fingerprint {
    fn of(stat: &std::fs::Metadata) -> Self {
        Self {
            device: stat.dev() as i64,
            inode: stat.ino() as i64,
            bytes: stat.len() as i64,
            secs: stat.mtime(),
            nanos: stat.mtime_nsec(),
        }
    }
}

/// 重新 fstat 打开的 fd；失败视为变化（无法验证稳定性时不提交）。
fn refstat(file: &File) -> Option<Fingerprint> {
    file.metadata().ok().map(|stat| Fingerprint::of(&stat))
}

fn recorded_fingerprint(tx: &Transaction<'_>, id: &str) -> Result<Option<Fingerprint>> {
    tx.query_row(
        "SELECT device,inode,bytes,updated_at_secs,updated_at_nanos
         FROM sessions WHERE session_id=?1",
        [id],
        |row| {
            Ok(Fingerprint {
                device: row.get(0)?,
                inode: row.get(1)?,
                bytes: row.get(2)?,
                secs: row.get(3)?,
                nanos: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(unavailable)
}

/// 单文件结果：true 表示该文件的派生行应保留在目录中。
fn index_file(
    conn: &mut Connection,
    path: &Path,
    id: &str,
    cancellation: &CancellationToken,
    diagnostics: &mut Vec<String>,
) -> Result<bool> {
    let file = match open_session(path) {
        Ok(file) => file,
        // 目录扫描后文件消失：等同外部删除，仅移除派生行，不算损坏。
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return remove(conn, id).map(|_| false);
        }
        Err(_) => {
            diagnostics.push(format!("{id}: session file is not readable"));
            return remove(conn, id).map(|_| false);
        }
    };
    let stat = match file.metadata() {
        Ok(stat) if stat.is_file() => stat,
        Ok(_) => {
            diagnostics.push(format!("{id}: session is not a regular file"));
            return remove(conn, id).map(|_| false);
        }
        Err(_) => {
            diagnostics.push(format!("{id}: session file is not readable"));
            return remove(conn, id).map(|_| false);
        }
    };
    let current = Fingerprint::of(&stat);

    let step = project(
        conn.transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(unavailable)?,
        &file,
        path,
        id,
        &current,
        cancellation,
        diagnostics,
    )?;
    match step {
        // 未变化、已提交新投影或并发变化后回滚：目录行保留。
        Step::Kept => Ok(true),
        // 源无效（坏 header/首条 user 前损坏/读取失败）或没有 user 消息：
        // 移除派生行；诊断已在 project 内记录（无 user 时静默）。
        Step::Invalid | Step::Gone => Ok(false),
    }
}

enum Step {
    /// 目录行保留。
    Kept,
    /// 源无效，诊断已记录；调用方移除派生行。
    Invalid,
    /// 源有效但没有 user 消息：不列出，静默移除。
    Gone,
}

/// 在一笔 BEGIN IMMEDIATE 中重建单个文件的投影。任何提前返回都让事务
/// 随 drop 回滚（未写入任何已提交数据）。
fn project(
    tx: Transaction<'_>,
    file: &File,
    path: &Path,
    id: &str,
    current: &Fingerprint,
    cancellation: &CancellationToken,
    diagnostics: &mut Vec<String>,
) -> Result<Step> {
    let recorded = recorded_fingerprint(&tx, id)?;
    if !unchanged(path, file, current) {
        diagnostics.push(format!("{id}: file changed while indexing"));
        return Ok(Step::Kept);
    }
    if recorded.as_ref() == Some(current) {
        return Ok(Step::Kept);
    }
    let diagnostic_start = diagnostics.len();
    let step = project_contents(&tx, file, id, current, cancellation, diagnostics)?;
    cancellation::check(cancellation)?;
    if !unchanged(path, file, current) {
        diagnostics.truncate(diagnostic_start);
        diagnostics.push(format!("{id}: file changed while indexing"));
        return Ok(Step::Kept);
    }
    if matches!(step, Step::Invalid | Step::Gone) {
        tx.execute("DELETE FROM sessions WHERE session_id=?1", [id])
            .map_err(unavailable)?;
    }
    tx.commit().map_err(unavailable)?;
    Ok(step)
}

fn unchanged(path: &Path, file: &File, current: &Fingerprint) -> bool {
    refstat(file) == Some(*current)
        && fs::symlink_metadata(path)
            .ok()
            .filter(|metadata| metadata.is_file())
            .is_some_and(|metadata| Fingerprint::of(&metadata) == *current)
}

fn project_contents(
    tx: &Transaction<'_>,
    file: &File,
    id: &str,
    current: &Fingerprint,
    cancellation: &CancellationToken,
    diagnostics: &mut Vec<String>,
) -> Result<Step> {
    // fd 边界受限读取：打开后文件增长的部分永不进入本次投影。
    let mut reader = BufReader::new(file.take(current.bytes as u64));
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line).is_err() {
        diagnostics.push(format!("{id}: session file could not be read"));
        return Ok(Step::Invalid);
    }
    let header = match summary::header(&line, id) {
        Ok(header) => header,
        Err(error) => {
            diagnostics.push(format!("{id}: {error}"));
            return Ok(Step::Invalid);
        }
    };

    // 完整替换该文件的派生行：先删除旧行（级联 prompt/FTS），再插入占位
    // 会话行以流式写入 prompt，提交前补齐 summary/preview/fingerprint。
    tx.execute("DELETE FROM sessions WHERE session_id=?1", [id])
        .map_err(unavailable)?;
    let created_ms = parse_timestamp(&header.timestamp).map_err(unavailable)?;
    let cwd = header.cwd.to_string_lossy().into_owned();
    tx.execute(
        "INSERT INTO sessions(session_id,cwd,created_at_ms,updated_at_secs,
         updated_at_nanos,summary,preview,bytes,done,device,inode,
         summary_folded,preview_folded,cwd_folded)
         VALUES (?1,?2,?3,?4,?5,'','',?6,0,?7,?8,'','',?9)",
        rusqlite::params![
            id,
            cwd,
            i64::try_from(created_ms).map_err(unavailable)?,
            current.secs,
            current.nanos,
            current.bytes,
            current.device,
            current.inode,
            cwd.to_lowercase(),
        ],
    )
    .map_err(unavailable)?;
    let session_rowid = tx.last_insert_rowid();

    let mut insert_prompt = tx
        .prepare(
            "INSERT INTO session_prompts(session_rowid,entry_offset,text,text_folded)
             VALUES (?1,?2,?3,?4)",
        )
        .map_err(unavailable)?;
    let mut head = Head::default();
    let mut offset = line.len() as i64;
    let mut done = false;
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => {
                diagnostics.push(format!("{id}: session file could not be read"));
                return Ok(Step::Invalid);
            }
        }
        // 边界处的不完整尾行不入索引；补齐后的元数据变化会触发重建。
        if !line.ends_with(b"\n") {
            done = false;
            break;
        }
        // 逐行取消边界：返回时事务随 drop 回滚。
        cancellation::check(cancellation)?;
        match summary::classify(&line) {
            Ok(record) => {
                if let Some(status) = record.status() {
                    done = status;
                }
                if let Classified::User(content) = &record {
                    insert_prompt
                        .execute(rusqlite::params![
                            session_rowid,
                            offset,
                            content,
                            content.to_lowercase(),
                        ])
                        .map_err(unavailable)?;
                }
                head.feed(record);
            }
            Err(_) if head.has_summary() => {
                done = false;
                // 保留有效前缀的摘要/prompts，done=false，停止在坏行前。
                diagnostics.push(format!(
                    "{id}: invalid session record after first user message"
                ));
                break;
            }
            Err(_) => {
                diagnostics.push(format!(
                    "{id}: invalid session record before first user message"
                ));
                return Ok(Step::Invalid);
            }
        }
        offset += line.len() as i64;
    }
    drop(insert_prompt);

    let Some((summary, preview)) = head.finish() else {
        // 没有 user 消息的文件不列出：静默移除派生行。
        return Ok(Step::Gone);
    };
    tx.execute(
        "UPDATE sessions SET summary=?2,preview=?3,bytes=?4,done=?5,
         summary_folded=?6,preview_folded=?7 WHERE session_id=?1",
        rusqlite::params![
            id,
            summary,
            preview,
            current.bytes,
            done,
            summary.to_lowercase(),
            preview.to_lowercase(),
        ],
    )
    .map_err(unavailable)?;

    Ok(Step::Kept)
}

/// 移除单个文件的派生行；级联删除 prompts 并经触发器同步 FTS。
fn remove(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM sessions WHERE session_id=?1", [id])
        .map_err(unavailable)?;
    Ok(())
}

/// 目录扫描完整成功后移除已不存在的会话行；绝不触碰 input_history。
fn drop_absent(
    conn: &mut Connection,
    dir: &Path,
    seen: &HashSet<String>,
    cancellation: &CancellationToken,
) -> Result<()> {
    let absent = {
        let mut statement = conn
            .prepare("SELECT session_id FROM sessions")
            .map_err(unavailable)?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(unavailable)?;
        let mut absent = Vec::new();
        for id in ids {
            let id = id.map_err(unavailable)?;
            if !seen.contains(&id) {
                absent.push(id);
            }
        }
        absent
    };
    for id in absent {
        cancellation::check(cancellation)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(unavailable)?;
        match fs::symlink_metadata(dir.join(format!("{id}.jsonl"))) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tx.execute("DELETE FROM sessions WHERE session_id=?1", [&id])
                    .map_err(unavailable)?;
            }
            Err(_) => return Err(anyhow::anyhow!("Session directory unavailable.")),
            Ok(_) => {}
        }
        tx.commit().map_err(unavailable)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::persistence::locks::SessionLock;
    use crate::cancellation::Cancelled;
    use crate::history::Store;
    use serde_json::json;
    use std::io::Write as _;

    struct Fixture {
        _root: tempfile::TempDir,
        store: Store,
        dir: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let store = Store::open(&root.path().join("history.db")).unwrap();
            let dir = root.path().join("sessions");
            fs::create_dir_all(&dir).unwrap();
            Self {
                _root: root,
                store,
                dir,
            }
        }

        fn sync(&mut self) -> Vec<String> {
            self.store
                .sync_sessions(&self.dir, &CancellationToken::new())
                .unwrap()
        }

        fn path(&self, n: u8) -> std::path::PathBuf {
            self.dir.join(format!("{}.jsonl", session(n)))
        }

        fn write_session(&self, n: u8, body: &str) {
            fs::write(self.path(n), body).unwrap();
        }

        fn append(&self, n: u8, tail: &str) {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(self.path(n))
                .unwrap();
            file.write_all(tail.as_bytes()).unwrap();
        }

        fn count(&self, sql: &str) -> i64 {
            self.store
                .conn
                .query_row(sql, [], |row| row.get(0))
                .unwrap()
        }

        fn prompts(&self, session: &str) -> Vec<(i64, String)> {
            let mut statement = self
                .store
                .conn
                .prepare(
                    "SELECT p.entry_offset,p.text FROM session_prompts p
                     JOIN sessions s ON s.rowid=p.session_rowid
                     WHERE s.session_id=?1 ORDER BY p.entry_offset",
                )
                .unwrap();
            let rows = statement
                .query_map([session], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap();
            rows.map(|row| row.unwrap()).collect()
        }

        fn search(&self, query: &str) -> Vec<String> {
            self.store
                .search_sessions(None, query)
                .unwrap()
                .into_iter()
                .map(|summary| summary.id)
                .collect()
        }

        fn summary(&self, session: &str) -> crate::agent::persistence::SessionSummary {
            self.store
                .search_sessions(None, "")
                .unwrap()
                .into_iter()
                .find(|summary| summary.id == session)
                .unwrap()
        }
    }

    fn session(n: u8) -> String {
        format!("2026-10-17T01-02-03-{n:032x}")
    }

    fn header_text(id: &str, cwd: &str) -> String {
        format!(
            "{}\n",
            json!({"type":"session","version":3,"id":id,"timestamp":"2026-10-17T01:02:03Z","cwd":cwd})
        )
    }

    fn record(value: serde_json::Value) -> String {
        format!("{value}\n")
    }

    fn user(text: &str) -> String {
        record(json!({"type":"message","message":{"role":"user","content":text}}))
    }

    fn stopped() -> String {
        record(json!({"type":"message","message":{"role":"assistant","stopReason":"stop"}}))
    }

    #[test]
    fn sqlite_index_incremental_appends_partials_and_completion() {
        let mut fixture = Fixture::new();
        let id = session(1);
        let head = header_text(&id, "/project");
        let first = user("首条 无关键词的介绍");
        let needle = user("修复模型切换失败 needle_42");
        let base = format!("{head}{first}{needle}{}", stopped());
        fixture.write_session(1, &base);

        let diagnostics = fixture.sync();
        assert!(diagnostics.is_empty());
        assert_eq!(
            fixture.prompts(&id),
            vec![
                (head.len() as i64, "首条 无关键词的介绍".into()),
                (
                    (head.len() + first.len()) as i64,
                    "修复模型切换失败 needle_42".into()
                ),
            ]
        );
        assert_eq!(fixture.search("needle_42"), vec![id.clone()]);
        let summary = fixture.summary(&id);
        assert!(summary.done);
        assert_eq!(summary.bytes, base.len() as u64);

        // 追加不完整尾行（自定义事件）：不入索引，done=false，不重复。
        let partial = "{\"type\":\"custom\",\"customType\":\"pp.event\",\"data\":{";
        fixture.append(1, partial);
        fixture.sync();
        assert_eq!(fixture.count("SELECT COUNT(*) FROM session_prompts"), 2);
        assert!(!fixture.summary(&id).done);

        // 尾行补齐：custom 记录不是 user prompt，末尾状态回退到 assistant stop。
        fixture.append(1, "}}\n");
        let complete = fs::read(fixture.path(1)).unwrap();
        fixture.sync();
        assert_eq!(fs::read(fixture.path(1)).unwrap(), complete);
        assert_eq!(fixture.count("SELECT COUNT(*) FROM session_prompts"), 2);
        assert!(fixture.summary(&id).done);

        // 不完整的 user 尾行：不入索引且 done=false。
        let open_user =
            "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"补齐前\"}";
        fixture.append(1, open_user);
        fixture.sync();
        assert_eq!(fixture.count("SELECT COUNT(*) FROM session_prompts"), 2);
        assert!(fixture.search("补齐前").is_empty());
        assert!(!fixture.summary(&id).done);

        // user 尾行补齐：prompt 入索引，最后一条完整记录是 user → done=false。
        fixture.append(1, "}\n");
        fixture.sync();
        assert_eq!(fixture.count("SELECT COUNT(*) FROM session_prompts"), 3);
        assert_eq!(fixture.search("补齐前"), vec![id.clone()]);
        assert!(!fixture.summary(&id).done);

        // 重复同步：fingerprint 未变化只 stat，无重复行，JSONL 字节不变。
        let before = (
            fixture.count("SELECT COUNT(*) FROM sessions"),
            fixture.count("SELECT COUNT(*) FROM session_prompts"),
        );
        let bytes = fs::read(fixture.path(1)).unwrap();
        fixture.sync();
        fixture.sync();
        assert_eq!(fs::read(fixture.path(1)).unwrap(), bytes);
        assert_eq!(
            before,
            (
                fixture.count("SELECT COUNT(*) FROM sessions"),
                fixture.count("SELECT COUNT(*) FROM session_prompts")
            )
        );

        // bytes 记录当前文件长度。
        assert_eq!(fixture.summary(&id).bytes, bytes.len() as u64);
    }

    #[test]
    fn sqlite_index_replaces_inode_truncation_deletion_and_missing_dir() {
        let mut fixture = Fixture::new();
        let id = session(2);
        fixture.write_session(
            2,
            &format!(
                "{}{}{}",
                header_text(&id, "/project"),
                user("旧的 needle_old 内容"),
                stopped()
            ),
        );
        fixture
            .store
            .append_input(Path::new("/project"), "保留的输入", 1)
            .unwrap();
        fixture.sync();
        assert_eq!(fixture.search("needle_old"), vec![id.clone()]);
        let old: (i64, i64) = fixture
            .store
            .conn
            .query_row(
                "SELECT device,inode FROM sessions WHERE session_id=?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        // 同路径换 inode：完整替换投影，旧内容不残留。
        fs::remove_file(fixture.path(2)).unwrap();
        fixture.write_session(
            2,
            &format!(
                "{}{}{}",
                header_text(&id, "/project"),
                user("新的 needle_new 内容"),
                stopped()
            ),
        );
        fixture.sync();
        assert!(fixture.search("needle_old").is_empty());
        assert_eq!(fixture.search("needle_new"), vec![id.clone()]);
        assert_eq!(fixture.prompts(&id).len(), 1);
        let new: (i64, i64) = fixture
            .store
            .conn
            .query_row(
                "SELECT device,inode FROM sessions WHERE session_id=?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_ne!(old, new);

        // 截短：截掉的内容不再可搜。
        fixture.write_session(
            2,
            &format!("{}{}", header_text(&id, "/project"), user("截短后的内容")),
        );
        fixture.sync();
        assert!(fixture.search("needle_new").is_empty());
        assert_eq!(fixture.prompts(&id).len(), 1);

        // 外部删除文件：派生行移除，input_history 保留。
        fs::remove_file(fixture.path(2)).unwrap();
        fixture.sync();
        assert_eq!(fixture.count("SELECT COUNT(*) FROM sessions"), 0);
        assert_eq!(fixture.count("SELECT COUNT(*) FROM input_history"), 1);
        assert_eq!(fixture.count("SELECT COUNT(*) FROM prompt_fts"), 0);

        // 目录整体缺失（成功判定）：清空派生行；input_history 仍保留。
        let other = session(3);
        fixture.write_session(
            3,
            &format!(
                "{}{}",
                header_text(&other, "/project"),
                user("needle_dir_gone")
            ),
        );
        fixture.sync();
        assert_eq!(fixture.count("SELECT COUNT(*) FROM sessions"), 1);
        fs::remove_dir_all(&fixture.dir).unwrap();
        assert!(fixture.sync().is_empty());
        assert_eq!(fixture.count("SELECT COUNT(*) FROM sessions"), 0);
        assert_eq!(fixture.count("SELECT COUNT(*) FROM input_history"), 1);
    }

    #[test]
    fn sqlite_index_rebuild_preserves_inputs_and_bytes() {
        let mut fixture = Fixture::new();
        let id = session(4);
        fixture.write_session(
            4,
            &format!(
                "{}{}{}",
                header_text(&id, "/project"),
                user("重建 needle_rebuild"),
                stopped()
            ),
        );
        fixture
            .store
            .append_input(Path::new("/project"), "原始输入保留", 1)
            .unwrap();
        fixture.sync();
        let bytes = fs::read(fixture.path(4)).unwrap();

        fixture.store.rebuild_sessions(&fixture.dir).unwrap();
        assert_eq!(fixture.search("needle_rebuild"), vec![id.clone()]);
        assert_eq!(fixture.count("SELECT COUNT(*) FROM input_history"), 1);
        assert_eq!(fs::read(fixture.path(4)).unwrap(), bytes);
    }

    #[test]
    fn sqlite_index_reports_corrupt_sources_with_sanitized_diagnostics() {
        let mut fixture = Fixture::new();
        let good = session(5);
        fixture.write_session(
            5,
            &format!(
                "{}{}{}",
                header_text(&good, "/project"),
                user("完好的 needle_good"),
                stopped()
            ),
        );
        // 坏 header。
        let bad = session(6);
        fixture.write_session(6, "bad header\n");
        // 首条 user 前损坏：整个文件无效。
        let pre = session(7);
        fixture.write_session(
            7,
            &format!(
                "{}broken before user\n{}",
                header_text(&pre, "/project"),
                user("needle_pre")
            ),
        );
        // 首条 user 后损坏：保留有效前缀，done=false。
        let post = session(8);
        fixture.write_session(
            8,
            &format!(
                "{}{}broken after user\n{}{}",
                header_text(&post, "/project"),
                user("有效前缀 needle_post"),
                user("第二条"),
                stopped()
            ),
        );
        // 符号链接不是普通会话文件。
        let link = session(9);
        std::os::unix::fs::symlink(fixture.path(5), fixture.dir.join(format!("{link}.jsonl")))
            .unwrap();

        let diagnostics = fixture.sync();
        assert_eq!(fixture.search("needle_good"), vec![good.clone()]);
        assert!(fixture.search("needle_pre").is_empty());
        assert_eq!(fixture.search("needle_post"), vec![post.clone()]);
        let summary = fixture.summary(&post);
        assert!(!summary.done);
        // 停止在坏行前：坏行之后的第二条 user 未入索引。
        assert_eq!(fixture.prompts(&post).len(), 1);
        assert_eq!(diagnostics.len(), 4);
        assert!(
            diagnostics
                .iter()
                .all(|d| d.starts_with("2026-10-17T01-02-03-"))
        );
        assert!(diagnostics.iter().all(|d| !d.contains("needle")
            && !d.contains("bad header")
            && !d.contains("broken")
            && !d.contains("有效前缀")));
        // 修复坏 header 后重新同步：恢复索引，诊断消失。
        fixture.write_session(
            6,
            &format!(
                "{}{}{}",
                header_text(&bad, "/project"),
                user("修复后 needle_fixed"),
                stopped()
            ),
        );
        assert!(
            fixture
                .sync()
                .iter()
                .all(|diagnostic| !diagnostic.starts_with(&bad))
        );
        assert_eq!(fixture.search("needle_fixed"), vec![bad.clone()]);
    }

    #[test]
    fn sqlite_index_searches_held_locks_and_honours_cancellation() {
        let mut fixture = Fixture::new();
        let id = session(10);
        fixture.write_session(
            10,
            &format!(
                "{}{}{}",
                header_text(&id, "/project"),
                user("持锁 needle_locked"),
                stopped()
            ),
        );
        // 打开中的会话只读索引：持有 session 锁仍可搜索，但不获取锁。
        let lock = SessionLock::acquire(&fixture.dir.join(".locks"), &id).unwrap();
        fixture.sync();
        assert_eq!(fixture.search("needle_locked"), vec![id.clone()]);
        drop(lock);

        // 已取消的同步立即返回 Cancelled，已提交的投影保留。
        let token = CancellationToken::new();
        token.cancel();
        let error = fixture
            .store
            .sync_sessions(&fixture.dir, &token)
            .unwrap_err();
        assert!(error.is::<Cancelled>());
        assert_eq!(fixture.count("SELECT COUNT(*) FROM sessions"), 1);
        assert_eq!(fixture.search("needle_locked"), vec![id.clone()]);
    }
}
