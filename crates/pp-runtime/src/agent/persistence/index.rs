//! Incremental indexing of the session directory (JSONL) into the history.db derived
//! projection.
//!
//! Only regular `.jsonl` files accepted by `validate_id` and opened with
//! NOFOLLOW/NONBLOCK are indexed: files whose fingerprint (device/inode/byte
//! count/mtime seconds+nanos) is unchanged are only stat'ed; changed files are
//! streamed and reparsed in a dedicated BEGIN IMMEDIATE transaction that fully
//! replaces that file's directory row, prompt rows and FTS document. Only the
//! actual body of `type=message`, `role=user` records enters the prompt index by
//! its starting byte offset; duplicate copies in native_append/session_state are
//! not indexed. Cancellation is checked at per-file/per-line boundaries: the
//! current file transaction rolls back and Cancelled is returned, while committed
//! files are kept. Derived row deletion never touches `input_history` and never
//! modifies the original JSONL.

use super::{
    parse_timestamp,
    summary::{self, Classified, Head},
    validate_id,
};
use anyhow::{Context, Result};
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{BufReader, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

const UNAVAILABLE: &str = "History database unavailable.";

fn unavailable<E>(_: E) -> anyhow::Error {
    anyhow::anyhow!(UNAVAILABLE)
}

/// Sync the derived projection of every session file under `dir` and return
/// per-file redacted diagnostics (session ID and a fixed reason only, never log
/// bodies, paths or arguments).
pub(crate) fn sync(
    conn: &mut Connection,
    dir: &Path,
    cancellation: &CancellationToken,
) -> Result<Vec<String>> {
    cancellation::check(cancellation)?;
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        // A missing directory is a success verdict: clear the derived rows and
        // rebuild once the directory reappears.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            conn.execute("DELETE FROM sessions", [])
                .map_err(unavailable)?;
            return Ok(Vec::new());
        }
        Err(error) => return Err(error).context("reading session directory"),
    };
    let fingerprints = load_fingerprints(conn)?;
    let mut diagnostics = Vec::new();
    let mut seen = HashSet::new();
    for entry in entries {
        // Mid-iteration failure removes no rows: unvisited entries must not be
        // treated as deleted.
        let entry = entry.context("reading session directory")?;
        let Some(id) = session_id(&entry.file_name()) else {
            continue;
        };
        cancellation::check(cancellation)?;
        let path = entry.path();
        if let Some(expected) = fingerprints.get(&id)
            && fs::symlink_metadata(&path)
                .ok()
                .filter(|metadata| metadata.is_file())
                .is_some_and(|metadata| Fingerprint::of(&metadata) == *expected)
        {
            seen.insert(id);
            continue;
        }
        index_file(conn, &path, &id, cancellation, &mut diagnostics)?;
        seen.insert(id);
    }
    cancellation::check(cancellation)?;
    drop_absent(conn, dir, &seen, cancellation)?;
    Ok(diagnostics)
}

/// A valid session file name: `.jsonl` suffix plus a session ID accepted by `validate_id`.
fn session_id(file_name: &OsStr) -> Option<String> {
    let id = file_name.to_str()?.strip_suffix(".jsonl")?;
    validate_id(id).ok()?;
    Some(id.to_owned())
}

/// Open with NOFOLLOW/NONBLOCK: reject symlinks and never block on unusual files.
fn open_session(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)
}

/// Source-file fingerprint of a derived row: path identity (device/inode) and
/// content boundary (byte count, mtime seconds+nanos). Any changed field triggers
/// a full reparse of that file; rewriting the old body in place on the same inode
/// while growing it would leave a wrong index if only growth were watched, so a
/// changed file is replaced wholesale rather than appended to.
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

/// Re-fstat the opened fd; failure counts as a change (do not commit when stability
/// cannot be verified).
fn refstat(file: &File) -> Option<Fingerprint> {
    file.metadata().ok().map(|stat| Fingerprint::of(&stat))
}

fn load_fingerprints(conn: &Connection) -> Result<HashMap<String, Fingerprint>> {
    let mut statement = conn
        .prepare(
            "SELECT session_id,device,inode,bytes,updated_at_secs,updated_at_nanos FROM sessions",
        )
        .map_err(unavailable)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Fingerprint {
                    device: row.get(1)?,
                    inode: row.get(2)?,
                    bytes: row.get(3)?,
                    secs: row.get(4)?,
                    nanos: row.get(5)?,
                },
            ))
        })
        .map_err(unavailable)?;
    let mut fingerprints = HashMap::new();
    for row in rows {
        let (id, fingerprint) = row.map_err(unavailable)?;
        fingerprints.insert(id, fingerprint);
    }
    Ok(fingerprints)
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

/// Single-file result: true means the file's derived rows should stay in the directory.
fn index_file(
    conn: &mut Connection,
    path: &Path,
    id: &str,
    cancellation: &CancellationToken,
    diagnostics: &mut Vec<String>,
) -> Result<bool> {
    let file = match open_session(path) {
        Ok(file) => file,
        // The file vanished after the directory scan: treat it as an external
        // deletion, remove only the derived rows and do not count it as corruption.
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
        // Unchanged, new projection committed, or rolled back after a concurrent
        // change: keep the directory row.
        Step::Kept => Ok(true),
        // Invalid source (bad header / corruption before the first user / read
        // failure) or no user message: remove the derived rows; the diagnostic was
        // already recorded inside project (silent when there is no user).
        Step::Invalid | Step::Gone => Ok(false),
    }
}

enum Step {
    /// Keep the directory row.
    Kept,
    /// Invalid source, diagnostic already recorded; the caller removes the derived rows.
    Invalid,
    /// Valid source without a user message: not listed, silently removed.
    Gone,
}

/// Rebuild the projection of a single file inside one BEGIN IMMEDIATE. Any early
/// return lets the transaction roll back on drop (no committed data was written).
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
    // fd-bounded read: anything the file gains after opening never enters this projection.
    let mut reader =
        pp_core::session_jsonl::Reader::new(BufReader::new(file.take(current.bytes as u64)));
    let mut line = Vec::new();
    if reader.read_record(&mut line).is_err() {
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

    // Fully replace the file's derived rows: delete the old rows first (cascading to
    // prompt/FTS), then insert a placeholder session row so prompts can be streamed in
    // and fill in summary/preview/fingerprint before commit.
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
        let count = match reader.read_record(&mut line) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData && head.has_summary() => {
                done = false;
                diagnostics.push(format!(
                    "{id}: invalid session record after first user message"
                ));
                break;
            }
            Err(_) => {
                diagnostics.push(format!("{id}: session file could not be read"));
                return Ok(Step::Invalid);
            }
        };
        // An incomplete trailing line at the boundary does not enter the index; the
        // metadata change after completion triggers a rebuild.
        if !line.ends_with(b"\n") {
            done = false;
            break;
        }
        // Per-line cancellation boundary: the transaction rolls back on drop when returning.
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
                // Keep the valid prefix's summary/prompts, set done=false and stop
                // before the bad line.
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
        offset += count as i64;
    }
    drop(insert_prompt);

    let Some((summary, preview)) = head.finish() else {
        // Files without a user message are not listed: silently remove derived rows.
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

/// Remove a single file's derived rows; prompts cascade and FTS is synced by triggers.
fn remove(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM sessions WHERE session_id=?1", [id])
        .map_err(unavailable)?;
    Ok(())
}

/// Remove rows for sessions that no longer exist after a complete directory scan;
/// never touches input_history.
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
    use crate::history::Store;
    use pp_core::cancellation::Cancelled;
    use serde_json::json;

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
            super::super::tests::write_log(&self.path(n), body);
        }

        fn append(&self, n: u8, tail: &str) {
            let mut text = super::super::tests::read_log(&self.path(n));
            text.push_str(tail);
            super::super::tests::write_log(&self.path(n), &text);
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
    fn sqlite_index_incremental_append_inode_replacement_truncation_and_rebuild() {
        {
            let mut fixture = Fixture::new();
            let id = session(1);
            let head = header_text(&id, "/project");
            let first = user("first intro without keywords");
            let needle = user("fix model switch failure needle_42");
            let base = format!("{head}{first}{needle}{}", stopped());
            fixture.write_session(1, &base);

            let diagnostics = fixture.sync();
            assert!(diagnostics.is_empty());
            assert_eq!(
                fixture.prompts(&id),
                vec![
                    (
                        super::super::tests::encode_log(&head).len() as i64,
                        "first intro without keywords".into()
                    ),
                    (
                        super::super::tests::encode_log(&format!("{head}{first}")).len() as i64,
                        "fix model switch failure needle_42".into()
                    ),
                ]
            );
            assert_eq!(fixture.search("needle_42"), vec![id.clone()]);
            let summary = fixture.summary(&id);
            assert!(summary.done);
            assert_eq!(summary.bytes, fs::metadata(fixture.path(1)).unwrap().len());

            // Appending an incomplete trailing line (custom event): not indexed, done=false,
            // no duplicates.
            let partial = "{\"type\":\"custom\",\"customType\":\"pp.event\",\"data\":{";
            fixture.append(1, partial);
            fixture.sync();
            assert_eq!(fixture.count("SELECT COUNT(*) FROM session_prompts"), 2);
            assert!(!fixture.summary(&id).done);

            // Completing the trailing line: a custom record is not a user prompt, so the
            // terminal status falls back to the assistant stop.
            fixture.append(1, "}}\n");
            let complete = fs::read(fixture.path(1)).unwrap();
            fixture.sync();
            assert_eq!(fs::read(fixture.path(1)).unwrap(), complete);
            assert_eq!(fixture.count("SELECT COUNT(*) FROM session_prompts"), 2);
            assert!(fixture.summary(&id).done);

            // Incomplete user trailing line: not indexed and done=false.
            let open_user =
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"补齐前\"}";
            fixture.append(1, open_user);
            fixture.sync();
            assert_eq!(fixture.count("SELECT COUNT(*) FROM session_prompts"), 2);
            assert!(fixture.search("补齐前").is_empty());
            assert!(!fixture.summary(&id).done);

            // Completing the user trailing line: the prompt enters the index, and the last
            // complete record is a user → done=false.
            fixture.append(1, "}\n");
            fixture.sync();
            assert_eq!(fixture.count("SELECT COUNT(*) FROM session_prompts"), 3);
            assert_eq!(fixture.search("补齐前"), vec![id.clone()]);
            assert!(!fixture.summary(&id).done);

            // Repeated sync: an unchanged fingerprint is only stat'ed, no duplicate rows and
            // the JSONL bytes stay identical.
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

            // bytes records the current file length.
            assert_eq!(fixture.summary(&id).bytes, bytes.len() as u64);
        }

        {
            let mut fixture = Fixture::new();
            let id = session(2);
            fixture.write_session(
                2,
                &format!(
                    "{}{}{}",
                    header_text(&id, "/project"),
                    user("old needle_old content"),
                    stopped()
                ),
            );
            fixture
                .store
                .append_input(Path::new("/project"), "kept input", 1)
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

            // Same path, new inode: the projection is fully replaced and old content does
            // not linger.
            fs::remove_file(fixture.path(2)).unwrap();
            fixture.write_session(
                2,
                &format!(
                    "{}{}{}",
                    header_text(&id, "/project"),
                    user("new needle_new content"),
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

            // Truncation: truncated content is no longer searchable.
            fixture.write_session(
                2,
                &format!(
                    "{}{}",
                    header_text(&id, "/project"),
                    user("content after truncation")
                ),
            );
            fixture.sync();
            assert!(fixture.search("needle_new").is_empty());
            assert_eq!(fixture.prompts(&id).len(), 1);

            // External deletion: derived rows are removed, input_history is kept.
            fs::remove_file(fixture.path(2)).unwrap();
            fixture.sync();
            assert_eq!(fixture.count("SELECT COUNT(*) FROM sessions"), 0);
            assert_eq!(fixture.count("SELECT COUNT(*) FROM input_history"), 1);
            assert_eq!(fixture.count("SELECT COUNT(*) FROM prompt_fts"), 0);

            // Whole directory missing (success verdict): derived rows are cleared;
            // input_history is still kept.
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

        {
            let mut fixture = Fixture::new();
            let id = session(4);
            fixture.write_session(
                4,
                &format!(
                    "{}{}{}",
                    header_text(&id, "/project"),
                    user("rebuild needle_rebuild"),
                    stopped()
                ),
            );
            fixture
                .store
                .append_input(Path::new("/project"), "original input kept", 1)
                .unwrap();
            fixture.sync();
            let bytes = fs::read(fixture.path(4)).unwrap();

            fixture.store.rebuild_sessions(&fixture.dir).unwrap();
            assert_eq!(fixture.search("needle_rebuild"), vec![id.clone()]);
            assert_eq!(fixture.count("SELECT COUNT(*) FROM input_history"), 1);
            assert_eq!(fs::read(fixture.path(4)).unwrap(), bytes);
        }
    }

    #[test]
    fn sqlite_index_corruption_diagnostics_locks_symlinks_and_cancellation() {
        {
            let mut fixture = Fixture::new();
            let good = session(5);
            fixture.write_session(
                5,
                &format!(
                    "{}{}{}",
                    header_text(&good, "/project"),
                    user("intact needle_good"),
                    stopped()
                ),
            );
            // Bad header.
            let bad = session(6);
            fixture.write_session(6, "bad header\n");
            // Corruption before the first user: the whole file is invalid.
            let pre = session(7);
            fixture.write_session(
                7,
                &format!(
                    "{}broken before user\n{}",
                    header_text(&pre, "/project"),
                    user("needle_pre")
                ),
            );
            // Corruption after the first user: keep the valid prefix, done=false.
            let post = session(8);
            fixture.write_session(
                8,
                &format!(
                    "{}{}broken after user\n{}{}",
                    header_text(&post, "/project"),
                    user("valid prefix needle_post"),
                    user("second"),
                    stopped()
                ),
            );
            // A symlink is not a regular session file.
            let link = session(9);
            std::os::unix::fs::symlink(fixture.path(5), fixture.dir.join(format!("{link}.jsonl")))
                .unwrap();

            let diagnostics = fixture.sync();
            assert_eq!(fixture.search("needle_good"), vec![good.clone()]);
            assert!(fixture.search("needle_pre").is_empty());
            assert_eq!(fixture.search("needle_post"), vec![post.clone()]);
            let summary = fixture.summary(&post);
            assert!(!summary.done);
            // Stop before the bad line: the second user record after it is not indexed.
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
                && !d.contains("valid prefix")));
            // Resyncing after fixing the bad header restores the index and the diagnostics
            // disappear.
            fixture.write_session(
                6,
                &format!(
                    "{}{}{}",
                    header_text(&bad, "/project"),
                    user("fixed needle_fixed"),
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

        {
            let mut fixture = Fixture::new();
            let id = session(10);
            fixture.write_session(
                10,
                &format!(
                    "{}{}{}",
                    header_text(&id, "/project"),
                    user("locked needle_locked"),
                    stopped()
                ),
            );
            // An open session is indexed read-only: it stays searchable while the session
            // lock is held, but no lock is acquired here.
            let lock = SessionLock::acquire(&fixture.dir.join(".locks"), &id).unwrap();
            fixture.sync();
            assert_eq!(fixture.search("needle_locked"), vec![id.clone()]);
            drop(lock);

            // An already-cancelled sync returns Cancelled immediately and keeps committed
            // projections.
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

        {
            let mut fixture = Fixture::new();
            let target = session(11);
            let replaced = session(12);
            let content = format!(
                "{}{}{}",
                header_text(&target, "/project"),
                user("target needle_target"),
                stopped()
            );
            fixture.write_session(11, &content);
            fixture.write_session(
                12,
                &format!(
                    "{}{}{}",
                    header_text(&replaced, "/project"),
                    user("to replace needle_replaced"),
                    stopped()
                ),
            );
            assert!(fixture.sync().is_empty());
            assert_eq!(fixture.search("needle_replaced"), vec![replaced.clone()]);

            fs::remove_file(fixture.path(12)).unwrap();
            std::os::unix::fs::symlink(fixture.path(11), fixture.path(12)).unwrap();

            let diagnostics = fixture.sync();
            assert!(diagnostics.iter().any(|d| d.starts_with(&replaced)));
            assert!(fixture.search("needle_replaced").is_empty());
            assert_eq!(fixture.search("needle_target"), vec![target]);
        }
    }
}
