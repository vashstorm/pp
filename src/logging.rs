mod debug;

use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use anyhow::{Context, ensure};
use chrono::{Local, NaiveDate};
use rustix::fs::{FlockOperation, OFlags, flock};
use tracing_subscriber::{
    filter::Targets, fmt::writer::MakeWriter, layer::SubscriberExt, util::SubscriberInitExt,
};

pub(crate) const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

pub(crate) struct LoggingGuard {
    state: Arc<Mutex<WriterState>>,
}

impl LoggingGuard {
    pub(crate) fn take_error(&self) -> Option<io::Error> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .error
            .take()
    }
}

pub(crate) fn init() -> anyhow::Result<LoggingGuard> {
    let file = RotatingFile::new(logs_dir()?)?;
    let writer = EventMakeWriter(Arc::new(Mutex::new(WriterState {
        file,
        buffer: Vec::new(),
        error: None,
    })));
    tracing_subscriber::registry()
        .with(Targets::new().with_target("pp", tracing::Level::INFO))
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(false)
                .with_current_span(false)
                .with_span_list(false)
                .log_internal_errors(false)
                .with_writer(writer.clone()),
        )
        .try_init()?;
    Ok(LoggingGuard { state: writer.0 })
}

fn logs_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .context("HOME is required to locate ~/.pp/logs")?;
    let home = PathBuf::from(home);
    ensure!(home.is_absolute(), "HOME must be an absolute path");
    Ok(home.join(".pp/logs"))
}

/// Returns (total before cleanup, removed) for recognized regular log files.
/// Session journals, symlinks, directories, and foreign files are not counted or removed.
pub(crate) fn gc(cutoff: NaiveDate) -> anyhow::Result<(u64, u64)> {
    let dir = logs_dir()?;
    let metadata = match fs::symlink_metadata(&dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir(),
        "log directory must be a directory, not a symlink"
    );
    let lock = Arc::new(open_regular(&dir.join(".lock"), false, true)?);
    let _lock = FileLock::acquire(&lock)?;
    let mut total = 0;
    let mut removed = 0;
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let date = name.to_str().and_then(|name| {
            parse_name(name)
                .or_else(|| parse_debug_name(name))
                .map(|(date, _)| date)
                .or_else(|| parse_prompt_name(name))
        });
        if let Some(date) = date
            && entry.file_type()?.is_file()
        {
            total += 1;
            if date < cutoff {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
    }
    Ok((total, removed))
}

/// Opt-in human-readable debug text log. It shares `~/.pp/logs` and its `.lock` with the
/// ordinary pp log but owns an independent `debug.DATE.NNNN` namespace with
/// the same size limit. Initial prompts instead use one immutable
/// `prompt-debug.SESSION_ID` file per session. Neither path touches the global
/// tracing subscriber; files are only created when a record is written.
pub(crate) struct DebugLog {
    file: RotatingFile,
}

impl DebugLog {
    pub(crate) fn new() -> anyhow::Result<Self> {
        Ok(Self {
            file: RotatingFile::for_namespace(Namespace::Debug, logs_dir()?)?,
        })
    }

    /// Writes the session's initial prompt once, without rotating or appending.
    pub(crate) fn record_prompt(
        &mut self,
        session_id: &str,
        record: &impl serde::Serialize,
    ) -> io::Result<()> {
        let name = format!("prompt-debug.{session_id}");
        if parse_prompt_name(&name).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid prompt log ID",
            ));
        }
        let value = serde_json::to_value(record).map_err(io::Error::other)?;
        let text = debug::format_record(&value)?;
        if text.len() as u64 > MAX_LOG_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log event too large",
            ));
        }
        let _lock = FileLock::acquire(&self.file.lock)?;
        let mut file = open_regular(&self.file.dir.join(name), true, false)?;
        file.write_all(&text)
    }

    /// Appends one complete text section. Formatting and write failures surface
    /// as io errors; nothing is printed or logged here.
    pub(crate) fn record(&mut self, record: &impl serde::Serialize) -> io::Result<()> {
        let value = serde_json::to_value(record).map_err(io::Error::other)?;
        let text = debug::format_record(&value)?;
        self.file.append_record_for(Namespace::Debug, &text)
    }
}

struct WriterState {
    file: RotatingFile,
    buffer: Vec<u8>,
    error: Option<io::Error>,
}

impl WriterState {
    fn remember(&mut self, error: io::Error) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }
}

#[derive(Clone)]
struct EventMakeWriter(Arc<Mutex<WriterState>>);

impl<'a> MakeWriter<'a> for EventMakeWriter {
    type Writer = EventWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.buffer.clear();
        EventWriter {
            state,
            failed: false,
        }
    }
}

struct EventWriter<'a> {
    state: MutexGuard<'a, WriterState>,
    failed: bool,
}

impl Write for EventWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.failed || bytes.len() > MAX_LOG_BYTES as usize - self.state.buffer.len() {
            self.failed = true;
            self.state.remember(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log event too large",
            ));
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log event too large",
            ));
        }
        self.state.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for EventWriter<'_> {
    fn drop(&mut self) {
        if !self.failed {
            let WriterState { file, buffer, .. } = &mut *self.state;
            let result = file.append_record(buffer);
            if let Err(error) = result {
                self.state.remember(error);
            }
        }
    }
}

// Share the open lock fd without duplicating it for every event. The guard
// does not borrow RotatingFile, allowing its date and volume cache to change.
struct FileLock(Arc<File>);

impl FileLock {
    fn acquire(file: &Arc<File>) -> io::Result<Self> {
        let file = Arc::clone(file);
        loop {
            match flock(file.as_ref(), FlockOperation::LockExclusive) {
                Ok(()) => return Ok(Self(file)),
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        while let Err(rustix::io::Errno::INTR) = flock(self.0.as_ref(), FlockOperation::Unlock) {}
    }
}

struct RotatingFile {
    dir: PathBuf,
    lock: Arc<File>,
    current: Option<(NaiveDate, u64)>,
}

impl RotatingFile {
    fn new(dir: PathBuf) -> io::Result<Self> {
        Self::for_namespace(Namespace::Pp, dir)
    }

    // Both namespaces share one directory and one .lock file; only their file
    // naming differs, so scanning and rotation stay isolated.
    fn for_namespace(namespace: Namespace, dir: PathBuf) -> io::Result<Self> {
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let metadata = fs::symlink_metadata(&dir)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log directory is not a directory",
            ));
        }
        let lock = Arc::new(open_regular(&dir.join(".lock"), false, true)?);
        let mut file = Self {
            dir,
            lock,
            current: None,
        };
        let _lock = FileLock::acquire(&file.lock)?;
        let today = Local::now().date_naive();
        file.current = file.scan(namespace, today)?.map(|number| (today, number));
        Ok(file)
    }

    fn append_record(&mut self, record: &[u8]) -> io::Result<()> {
        let _lock = FileLock::acquire(&self.lock)?;
        self.append_record_locked(record, Local::now().date_naive())
    }

    fn append_record_for(&mut self, namespace: Namespace, record: &[u8]) -> io::Result<()> {
        let _lock = FileLock::acquire(&self.lock)?;
        self.append_record_locked_for(namespace, record, Local::now().date_naive())
    }

    fn append_record_locked(&mut self, record: &[u8], today: NaiveDate) -> io::Result<()> {
        self.append_record_locked_for(Namespace::Pp, record, today)
    }

    // Caller holds .lock. The explicit date keeps boundary tests clock-independent.
    fn append_record_locked_for(
        &mut self,
        namespace: Namespace,
        record: &[u8],
        today: NaiveDate,
    ) -> io::Result<()> {
        if record.len() as u64 > MAX_LOG_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log event too large",
            ));
        }
        let mut number = match self.current {
            Some((date, number)) if date == today => {
                match fs::symlink_metadata(self.path_for(namespace, today, number)) {
                    Ok(_) => Some(number),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        self.scan(namespace, today)?
                    }
                    Err(error) => return Err(error),
                }
            }
            _ => self.scan(namespace, today)?,
        };
        if let Some(mut latest) = number {
            // Other instances create contiguous successors while holding .lock.
            while let Some(next) = latest.checked_add(1) {
                match fs::symlink_metadata(self.path_for(namespace, today, next)) {
                    Ok(_) => {
                        open_regular(&self.path_for(namespace, today, next), false, false)?;
                        latest = next;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                    Err(error) => return Err(error),
                }
            }
            number = Some(latest);
        }
        if let Some(number) = number {
            let mut file = open_regular(&self.path_for(namespace, today, number), false, false)?;
            let length = file.metadata()?.len();
            if length <= MAX_LOG_BYTES && record.len() as u64 <= MAX_LOG_BYTES - length {
                self.current = Some((today, number));
                return file.write_all(record);
            }
        }
        loop {
            let next = match number {
                Some(number) => number
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("log volume overflow"))?,
                None => 1,
            };
            match open_regular(&self.path_for(namespace, today, next), true, false) {
                Ok(mut file) => {
                    self.current = Some((today, next));
                    return file.write_all(record);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    // Validate conflicts rather than skipping unsafe paths.
                    open_regular(&self.path_for(namespace, today, next), false, false)?;
                    number = self.scan(namespace, today)?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[cfg(test)]
    fn path(&self, date: NaiveDate, number: u64) -> PathBuf {
        self.path_for(Namespace::Pp, date, number)
    }

    fn path_for(&self, namespace: Namespace, date: NaiveDate, number: u64) -> PathBuf {
        self.dir.join(namespace.file_name(date, number))
    }

    fn scan(&self, namespace: Namespace, today: NaiveDate) -> io::Result<Option<u64>> {
        let mut maximum = None;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if let Some((date, number)) = entry
                .file_name()
                .to_str()
                .and_then(|name| namespace.parse_file_name(name))
                && date == today
            {
                open_regular(&entry.path(), false, false)?;
                maximum = Some(maximum.map_or(number, |old: u64| old.max(number)));
            }
        }
        Ok(maximum)
    }
}

fn open_regular(path: &Path, create_new: bool, create: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(create)
        .create_new(create_new)
        .mode(0o600)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "log path is not a regular file",
        ));
    }
    Ok(file)
}

#[derive(Clone, Copy)]
enum Namespace {
    Pp,
    Debug,
}

impl Namespace {
    fn file_name(self, date: NaiveDate, number: u64) -> String {
        match self {
            Self::Pp => format!("pp.{date}.{number}.log"),
            // Debug volumes are padded to four digits minimum, no ".log".
            Self::Debug => format!("debug.{date}.{number:04}"),
        }
    }

    fn parse_file_name(self, name: &str) -> Option<(NaiveDate, u64)> {
        match self {
            Self::Pp => parse_name(name),
            Self::Debug => parse_debug_name(name),
        }
    }
}

fn parse_name(name: &str) -> Option<(NaiveDate, u64)> {
    let body = name.strip_prefix("pp.")?.strip_suffix(".log")?;
    let (date, number) = body.rsplit_once('.')?;
    if date.len() != 10 || number.starts_with('0') || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    if parsed.to_string() != date {
        return None;
    }
    Some((parsed, number.parse().ok()?))
}

fn parse_prompt_name(name: &str) -> Option<NaiveDate> {
    let (time, random) = name.strip_prefix("prompt-debug.")?.rsplit_once('-')?;
    if time.len() != 19
        || random.len() != 32
        || !random
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let timestamp = chrono::NaiveDateTime::parse_from_str(time, "%Y-%m-%dT%H-%M-%S").ok()?;
    (timestamp.format("%Y-%m-%dT%H-%M-%S").to_string() == time).then_some(timestamp.date())
}

fn parse_debug_name(name: &str) -> Option<(NaiveDate, u64)> {
    let body = name.strip_prefix("debug.")?;
    let (date, digits) = body.rsplit_once('.')?;
    if date.len() != 10 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    if parsed.to_string() != date {
        return None;
    }
    let number: u64 = digits.parse().ok()?;
    // Only the exact {:04} spelling is a volume: unpadded or over-padded
    // numbers and volume zero are foreign files, never rotated into.
    (number > 0 && format!("{number:04}") == digits).then_some((parsed, number))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::{
        collections::BTreeSet,
        os::unix::fs::{PermissionsExt, symlink},
    };

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 19).unwrap()
    }

    fn append(file: &mut RotatingFile, record: &[u8], day: NaiveDate) -> io::Result<()> {
        let _lock = FileLock::acquire(&file.lock)?;
        file.append_record_locked(record, day)
    }

    fn debug_append(file: &mut RotatingFile, record: &[u8], day: NaiveDate) -> io::Result<()> {
        let _lock = FileLock::acquire(&file.lock)?;
        file.append_record_locked_for(Namespace::Debug, record, day)
    }

    fn padded_record(size: usize) -> Vec<u8> {
        let mut record = vec![b' '; size];
        record[..2].copy_from_slice(b"{}");
        record[size - 1] = b'\n';
        record
    }

    fn records(dir: &Path) -> Vec<Value> {
        let mut values = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name().to_str().and_then(parse_name).is_none() {
                continue;
            }
            let bytes = fs::read(entry.path()).unwrap();
            assert!(bytes.len() as u64 <= MAX_LOG_BYTES);
            assert!(bytes.ends_with(b"\n"));
            for line in bytes.strip_suffix(b"\n").unwrap().split(|b| *b == b'\n') {
                values.push(serde_json::from_slice(line).unwrap());
            }
        }
        values
    }

    #[test]
    fn whole_record_size_boundary_and_oversize_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = RotatingFile::new(dir.path().join("logs")).unwrap();
        let record = b"{\"mark\":1}\n";
        let first = padded_record(MAX_LOG_BYTES as usize - record.len());
        fs::write(file.path(date(), 1), &first).unwrap();
        append(&mut file, record, date()).unwrap();
        assert_eq!(
            fs::metadata(file.path(date(), 1)).unwrap().len(),
            MAX_LOG_BYTES
        );
        append(&mut file, record, date()).unwrap();
        assert_eq!(fs::read(file.path(date(), 2)).unwrap(), record);
        let before = fs::read(file.path(date(), 2)).unwrap();
        assert_eq!(
            append(&mut file, &vec![b'x'; MAX_LOG_BYTES as usize + 1], date())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(fs::read(file.path(date(), 2)).unwrap(), before);
        assert_eq!(records(&file.dir).len(), 3);

        let writer = EventMakeWriter(Arc::new(Mutex::new(WriterState {
            file,
            buffer: Vec::new(),
            error: None,
        })));
        {
            let mut event = writer.make_writer();
            event.write_all(b"{\"partial\":").unwrap();
            assert!(
                event
                    .write_all(&vec![b'x'; MAX_LOG_BYTES as usize])
                    .is_err()
            );
        }
        let state = writer.0.lock().unwrap();
        assert_eq!(
            state.error.as_ref().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(fs::read(state.file.path(date(), 2)).unwrap(), before);
        assert_eq!(records(&state.file.dir).len(), 3);
    }

    #[test]
    fn numeric_volumes_restart_and_deleted_cache_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = RotatingFile::new(dir.path().join("logs")).unwrap();
        fs::write(file.path(date(), 2), b"{}\n").unwrap();
        fs::write(file.path(date(), 10), b"{\"old\":true}\n").unwrap();
        append(&mut file, b"{\"new\":true}\n", date()).unwrap();
        let prefix = fs::read(file.path(date(), 10)).unwrap();
        let path = file.dir.clone();
        drop(file);
        let mut file = RotatingFile::new(path).unwrap();
        append(&mut file, b"{}\n", date()).unwrap();
        assert!(
            fs::read(file.path(date(), 10))
                .unwrap()
                .starts_with(&prefix)
        );
        assert!(!file.path(date(), 11).exists());
        fs::write(file.path(date(), 10), padded_record(MAX_LOG_BYTES as usize)).unwrap();
        append(&mut file, b"{}\n", date()).unwrap();
        assert_eq!(fs::read(file.path(date(), 11)).unwrap(), b"{}\n");
        fs::remove_file(file.path(date(), 11)).unwrap();
        append(&mut file, b"{\"recovered\":true}\n", date()).unwrap();
        assert_eq!(
            fs::read(file.path(date(), 11)).unwrap(),
            b"{\"recovered\":true}\n"
        );
        assert_eq!(records(&file.dir).len(), 3);
    }

    #[test]
    fn opening_writing_and_crossing_dates_preserve_old_logs() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        fs::create_dir(&logs).unwrap();
        let names = ["pp.2000-01-01.1.log", "debug.2000-01-01.0001"];
        for name in names {
            fs::write(logs.join(name), b"old log\n").unwrap();
        }
        let mut pp = RotatingFile::new(logs.clone()).unwrap();
        let mut debug = RotatingFile::for_namespace(Namespace::Debug, logs.clone()).unwrap();
        for day in [date(), date().succ_opt().unwrap()] {
            append(&mut pp, b"{}\n", day).unwrap();
            debug_append(&mut debug, b"{}\n", day).unwrap();
            for name in names {
                assert_eq!(fs::read(logs.join(name)).unwrap(), b"old log\n");
            }
        }
        drop(pp);
        drop(debug);
        let _pp = RotatingFile::new(logs.clone()).unwrap();
        let _debug = RotatingFile::for_namespace(Namespace::Debug, logs.clone()).unwrap();
        for name in names {
            assert_eq!(fs::read(logs.join(name)).unwrap(), b"old log\n");
        }
    }

    #[test]
    fn threaded_events_are_complete_unique_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let file = RotatingFile::new(dir.path().join("logs")).unwrap();
        let path = file.dir.clone();
        let writer = EventMakeWriter(Arc::new(Mutex::new(WriterState {
            file,
            buffer: Vec::new(),
            error: None,
        })));
        let dispatch = tracing::Dispatch::new(
            tracing_subscriber::registry().with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(writer.clone()),
            ),
        );
        std::thread::scope(|scope| {
            for thread in 0..8 {
                let dispatch = &dispatch;
                scope.spawn(move || {
                    tracing::dispatcher::with_default(dispatch, || {
                        for index in 0..50 {
                            tracing::info!(
                                event = "thread_record",
                                pid = std::process::id(),
                                marker = thread * 50 + index
                            );
                        }
                    })
                });
            }
        });
        let values = records(&path);
        let markers: BTreeSet<_> = values
            .iter()
            .map(|value| value["fields"]["marker"].as_u64().unwrap())
            .collect();
        assert_eq!(markers, (0..400).collect());
        assert_eq!(values.len(), markers.len());
        assert!(writer.0.lock().unwrap().error.is_none());
    }

    #[test]
    fn unsafe_paths_fail_without_following_or_leaking_lock() {
        for kind in ["symlink", "fifo", "directory"] {
            for lock_path in [true, false] {
                let dir = tempfile::tempdir().unwrap();
                let logs = dir.path().join("logs");
                fs::create_dir(&logs).unwrap();
                let sentinel = dir.path().join("sentinel");
                fs::write(&sentinel, b"untouched").unwrap();
                let path = logs.join(if lock_path {
                    ".lock".to_owned()
                } else {
                    format!("pp.{}.1.log", Local::now().date_naive())
                });
                match kind {
                    "symlink" => symlink(&sentinel, &path).unwrap(),
                    "fifo" => assert!(
                        std::process::Command::new("mkfifo")
                            .arg(&path)
                            .status()
                            .unwrap()
                            .success()
                    ),
                    _ => fs::create_dir(&path).unwrap(),
                }
                assert!(
                    RotatingFile::new(logs.clone()).is_err(),
                    "{kind}, lock={lock_path}"
                );
                assert_eq!(fs::read(&sentinel).unwrap(), b"untouched");
                if !lock_path {
                    let lock = open_regular(&logs.join(".lock"), false, false).unwrap();
                    flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
                    flock(&lock, FlockOperation::Unlock).unwrap();
                }
                if kind == "directory" {
                    fs::remove_dir(&path).unwrap();
                } else {
                    fs::remove_file(&path).unwrap();
                }
                let mut file = RotatingFile::new(logs).unwrap();
                file.append_record(b"{}\n").unwrap();
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        symlink(&target, dir.path().join("logs")).unwrap();
        assert!(RotatingFile::new(dir.path().join("logs")).is_err());
        assert_eq!(fs::read_dir(target).unwrap().count(), 0);
    }

    #[test]
    fn cached_writer_follows_successors_and_resets_date_volume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs");
        let mut first = RotatingFile::new(path.clone()).unwrap();
        let mut second = RotatingFile::new(path).unwrap();
        append(&mut first, b"{\"first\":1}\n", date()).unwrap();
        append(&mut second, b"{\"second\":1}\n", date()).unwrap();
        fs::write(first.path(date(), 1), padded_record(MAX_LOG_BYTES as usize)).unwrap();
        append(&mut first, b"{\"first\":2}\n", date()).unwrap();
        append(&mut second, b"{\"second\":2}\n", date()).unwrap();
        assert_eq!(
            fs::read(first.path(date(), 2)).unwrap(),
            b"{\"first\":2}\n{\"second\":2}\n"
        );
        let tomorrow = date().succ_opt().unwrap();
        fs::write(first.path(tomorrow, 10), b"{}\n").unwrap();
        append(&mut first, b"{\"next\":true}\n", tomorrow).unwrap();
        assert_eq!(
            fs::read(first.path(tomorrow, 10)).unwrap(),
            b"{}\n{\"next\":true}\n"
        );
        assert!(!first.path(tomorrow, 2).exists());
    }

    #[test]
    fn failed_append_releases_lock_and_next_event_still_writes() {
        let dir = tempfile::tempdir().unwrap();
        let file = RotatingFile::new(dir.path().join("logs")).unwrap();
        let path = file.path(Local::now().date_naive(), 1);
        let sentinel = dir.path().join("sentinel");
        fs::write(&sentinel, b"untouched").unwrap();
        symlink(&sentinel, &path).unwrap();
        let writer = EventMakeWriter(Arc::new(Mutex::new(WriterState {
            file,
            buffer: Vec::new(),
            error: None,
        })));
        writer
            .make_writer()
            .write_all(b"{\"lost\":true}\n")
            .unwrap();
        assert!(writer.0.lock().unwrap().error.is_some());
        let lock = open_regular(
            &writer.0.lock().unwrap().file.dir.join(".lock"),
            false,
            false,
        )
        .unwrap();
        flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
        flock(&lock, FlockOperation::Unlock).unwrap();
        assert_eq!(fs::read(&sentinel).unwrap(), b"untouched");
        fs::remove_file(&path).unwrap();
        writer
            .make_writer()
            .write_all(b"{\"recovered\":true}\n")
            .unwrap();
        assert_eq!(fs::read(path).unwrap(), b"{\"recovered\":true}\n");
        let guard = LoggingGuard {
            state: writer.0.clone(),
        };
        assert!(guard.take_error().is_some());
        assert!(guard.take_error().is_none());
    }

    #[test]
    fn permissions_overfull_and_volume_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = RotatingFile::new(dir.path().join("logs")).unwrap();
        assert_eq!(
            fs::metadata(&file.dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            file.lock.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(
            file.path(date(), 1),
            padded_record(MAX_LOG_BYTES as usize + 1),
        )
        .unwrap();
        append(&mut file, b"{}\n", date()).unwrap();
        assert_eq!(fs::read(file.path(date(), 2)).unwrap(), b"{}\n");
        assert_eq!(
            fs::metadata(file.path(date(), 2))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::write(
            file.path(date(), u64::MAX),
            padded_record(MAX_LOG_BYTES as usize),
        )
        .unwrap();
        file.current = None;
        assert!(append(&mut file, b"{}\n", date()).is_err());
        assert_eq!(fs::read(file.path(date(), 2)).unwrap(), b"{}\n");
    }

    #[test]
    fn debug_volumes_are_padded_names_isolated_from_pp_and_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let mut file = RotatingFile::for_namespace(Namespace::Debug, logs.clone()).unwrap();
        // Nothing is eagerly created beyond the shared .lock.
        let only: Vec<String> = fs::read_dir(&logs)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(only, [".lock".to_owned()]);
        // pp volumes and non-canonical debug names must not seed numbering.
        for name in [
            "pp.2026-10-19.1.log",
            "pp.2026-10-19.7.log",
            "debug.2026-10-19.1",
            "debug.2026-10-19.0000",
            "debug.2026-10-19.00001",
            "debug.2026-10-19.0001.log",
        ] {
            fs::write(logs.join(name), b"{}\n").unwrap();
        }
        debug_append(&mut file, b"{\"first\":true}\n", date()).unwrap();
        assert_eq!(
            fs::read(logs.join("debug.2026-10-19.0001")).unwrap(),
            b"{\"first\":true}\n"
        );
        fs::write(
            logs.join("debug.2026-10-19.0001"),
            padded_record(MAX_LOG_BYTES as usize),
        )
        .unwrap();
        debug_append(&mut file, b"{\"second\":true}\n", date()).unwrap();
        assert_eq!(
            fs::read(logs.join("debug.2026-10-19.0002")).unwrap(),
            b"{\"second\":true}\n"
        );
        // Every foreign or pp file is still present, byte for byte.
        let names: BTreeSet<String> = fs::read_dir(&logs)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                ".lock",
                "debug.2026-10-19.1",
                "debug.2026-10-19.0000",
                "debug.2026-10-19.00001",
                "debug.2026-10-19.0001",
                "debug.2026-10-19.0001.log",
                "debug.2026-10-19.0002",
                "pp.2026-10-19.1.log",
                "pp.2026-10-19.7.log",
            ]
            .into_iter()
            .map(String::from)
            .collect()
        );
    }

    #[test]
    fn debug_reopen_follows_successors_and_rolls_dates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs");
        let mut first = RotatingFile::for_namespace(Namespace::Debug, path.clone()).unwrap();
        let mut second = RotatingFile::for_namespace(Namespace::Debug, path).unwrap();
        debug_append(&mut first, b"{\"one\":1}\n", date()).unwrap();
        debug_append(&mut second, b"{\"two\":2}\n", date()).unwrap();
        assert_eq!(
            fs::read(first.path_for(Namespace::Debug, date(), 1)).unwrap(),
            b"{\"one\":1}\n{\"two\":2}\n"
        );
        let tomorrow = date().succ_opt().unwrap();
        debug_append(&mut first, b"{\"next\":true}\n", tomorrow).unwrap();
        assert_eq!(
            fs::read(first.path_for(Namespace::Debug, tomorrow, 1)).unwrap(),
            b"{\"next\":true}\n"
        );
        assert!(!first.path_for(Namespace::Debug, tomorrow, 2).exists());
    }

    #[test]
    fn debug_record_rejects_oversize_without_partial_write() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = DebugLog {
            file: RotatingFile::for_namespace(Namespace::Debug, dir.path().join("logs")).unwrap(),
        };
        log.record(&serde_json::json!({"turn": 1, "note": "a\nb"}))
            .unwrap();
        log.record(&serde_json::json!({"turn": 2})).unwrap();
        let today = Local::now().date_naive();
        let path = log.file.path_for(Namespace::Debug, today, 1);
        let bytes = fs::read(&path).unwrap();
        // Oversize records are rejected whole, leaving the file unchanged.
        let oversize = serde_json::json!({"pad": "x".repeat(MAX_LOG_BYTES as usize)});
        assert_eq!(
            log.record(&oversize).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn prompt_logs_are_immutable_bounded_and_retained_separately() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let mut log = DebugLog {
            file: RotatingFile::for_namespace(Namespace::Debug, logs.clone()).unwrap(),
        };
        let today = Local::now().date_naive();
        let stale = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
        let id = format!("{today}T12-00-00-{}", "ab".repeat(16));
        let stale_name = format!("prompt-debug.{stale}T12-00-00-{}", "ab".repeat(16));
        let foreign = format!("{stale_name}.bak");
        for name in [
            &stale_name,
            &foreign,
            "debug.2000-01-01.0001",
            "pp.2000-01-01.1.log",
        ] {
            fs::write(logs.join(name), "sentinel").unwrap();
        }
        log.record_prompt(&id, &serde_json::json!({"prompt": "initial"}))
            .unwrap();
        let path = logs.join(format!("prompt-debug.{id}"));
        let before = fs::read(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for name in [
            &stale_name,
            &foreign,
            "debug.2000-01-01.0001",
            "pp.2000-01-01.1.log",
        ] {
            assert_eq!(fs::read_to_string(logs.join(name)).unwrap(), "sentinel");
        }
        assert_eq!(
            log.record_prompt(&id, &"replacement").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        let oversize_id = format!("{today}T12-00-00-{}", "cd".repeat(16));
        assert_eq!(
            log.record_prompt(&oversize_id, &"x".repeat(MAX_LOG_BYTES as usize))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!logs.join(format!("prompt-debug.{oversize_id}")).exists());
        let symlink_id = format!("{today}T12-00-00-{}", "ef".repeat(16));
        let target = dir.path().join("target");
        fs::write(&target, "untouched").unwrap();
        symlink(&target, logs.join(format!("prompt-debug.{symlink_id}"))).unwrap();
        assert!(log.record_prompt(&symlink_id, &"replacement").is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "untouched");
    }
}
