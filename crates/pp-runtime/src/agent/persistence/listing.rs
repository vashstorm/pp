use super::locks::SessionLock;
use super::*;
use anyhow::ensure;
use chrono::{DateTime, Datelike, Local, NaiveDateTime, SecondsFormat};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn now_ms() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("session clock is before Unix epoch")?
        .as_millis()
        .try_into()
        .context("session clock overflow")
}

pub(super) fn timestamp(milliseconds: u64) -> Result<String> {
    let datetime = i64::try_from(milliseconds)
        .ok()
        .and_then(DateTime::from_timestamp_millis)
        .context("session clock overflow")?;
    Ok(datetime.to_rfc3339_opts(SecondsFormat::Millis, true))
}

pub(super) fn parse_timestamp(timestamp: &str) -> Result<u64> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
        .context("invalid session record: invalid timestamp")
}

pub(super) const ID_TIME_FORMAT: &str = "%Y-%m-%dT%H-%M-%S";

pub fn validate_id(id: &str) -> Result<()> {
    let valid = id.rsplit_once('-').is_some_and(|(time, random)| {
        time.len() == 19
            && time.bytes().enumerate().all(|(index, byte)| match index {
                4 | 7 | 13 | 16 => byte == b'-',
                10 => byte == b'T',
                _ => byte.is_ascii_digit(),
            })
            && NaiveDateTime::parse_from_str(time, ID_TIME_FORMAT).is_ok()
            && random.len() == 32
            && random
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    ensure!(valid, "invalid session ID");
    Ok(())
}

pub(super) fn create(
    dir: &Path,
    lock_dir: &Path,
    created_at_ms: u64,
    mut random: impl FnMut() -> Result<[u8; 16]>,
) -> Result<Active> {
    use std::fmt::Write as _;
    let created_at = i64::try_from(created_at_ms)
        .ok()
        .and_then(DateTime::from_timestamp_millis)
        .context("session clock overflow")?
        .with_timezone(&Local);
    ensure!(
        (0..=9999).contains(&created_at.year()),
        "session clock year is out of range"
    );
    let time = created_at.format(ID_TIME_FORMAT);
    loop {
        let random = random()?;
        let mut id = String::with_capacity(52);
        write!(&mut id, "{time}-").expect("writing to String");
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in random {
            id.push(char::from(HEX[usize::from(byte >> 4)]));
            id.push(char::from(HEX[usize::from(byte & 15)]));
        }
        let Some(session_lock) = SessionLock::try_acquire(lock_dir, &id)? else {
            continue;
        };
        match OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(format!("{id}.jsonl")))
        {
            Ok(file) => {
                return Ok(Active {
                    id,
                    created_at_ms,
                    file,
                    _lock: session_lock,
                    truncate_to: None,
                    header_written: false,
                    tail: None,
                    used_ids: HashSet::new(),
                    storage: Storage::default(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("persisting session file"),
        }
    }
}

pub(super) fn latest(dir: &Path) -> Result<String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("no saved sessions found")
        }
        Err(error) => return Err(error).context("reading saved sessions"),
    };
    let mut latest = None;
    for entry in entries {
        let entry = entry.context("reading saved session entry")?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".jsonl")) else {
            continue;
        };
        if validate_id(id).is_err() {
            continue;
        }
        let candidate = (entry.metadata()?.modified()?, id.to_owned());
        if latest.as_ref().is_none_or(|old| &candidate > old) {
            latest = Some(candidate);
        }
    }
    latest.map(|(_, id)| id).context("no saved sessions found")
}

pub struct SessionSummary {
    pub id: String,
    pub cwd: PathBuf,
    pub updated_at: SystemTime,
    pub summary: String,
    pub preview: String,
    pub bytes: u64,
    pub done: bool,
    pub pinned: bool,
}

pub struct SessionListing {
    pub sessions: Vec<SessionSummary>,
    pub diagnostics: Vec<String>,
}

pub(super) fn compact_summary(text: &str, limit: usize) -> String {
    let mut result = String::new();
    let mut count = 0;
    let mut whitespace = false;
    for character in text.chars() {
        if character.is_whitespace() || character.is_control() {
            whitespace = !result.is_empty();
            continue;
        }
        if whitespace {
            if count == limit {
                result.push('…');
                break;
            }
            result.push(' ');
            count += 1;
            whitespace = false;
        }
        if count == limit {
            result.push('…');
            break;
        }
        result.push(character);
        count += 1;
    }
    result
}

pub fn delete(dir: &Path, lock_dir: &Path, id: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    validate_id(id)?;
    // 当前会话及其他进程持有的会话都不可删除。
    let _lock = SessionLock::acquire(lock_dir, id)?;
    let path = dir.join(format!("{id}.jsonl"));
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32 | rustix::fs::OFlags::NONBLOCK.bits() as i32,
        )
        .open(&path)
        .context("opening session for deletion")?;
    let metadata = file.metadata().context("reading session metadata")?;
    ensure!(metadata.is_file(), "session is not a regular file");
    let current = fs::symlink_metadata(&path).context("checking session before deletion")?;
    ensure!(
        current.is_file() && current.dev() == metadata.dev() && current.ino() == metadata.ino(),
        "session changed before deletion"
    );
    fs::remove_file(path).context("deleting session")?;
    crate::tools::output::Store::remove(dir, id)
        .context("session deleted, but artifact cleanup failed")
}
