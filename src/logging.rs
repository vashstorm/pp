use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use anyhow::{Context, ensure};
use chrono::{Days, Local, NaiveDate};
use rustix::fs::{FlockOperation, OFlags, flock};
use tracing_subscriber::{
    filter::Targets, fmt::writer::MakeWriter, layer::SubscriberExt, util::SubscriberInitExt,
};

pub(crate) const RETENTION_DAYS: u64 = 30;
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
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .context("HOME is required to locate ~/.pp/logs")?;
    let home = PathBuf::from(home);
    ensure!(home.is_absolute(), "HOME must be an absolute path");
    let mut file = RotatingFile::new(home.join(".pp/logs"))?;
    let error = file.cleanup_error.take();
    let writer = EventMakeWriter(Arc::new(Mutex::new(WriterState {
        file,
        buffer: Vec::new(),
        error,
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
            if let Some(error) = self.state.file.cleanup_error.take() {
                self.state.remember(error);
            }
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
        loop {
            match flock(self.0.as_ref(), FlockOperation::Unlock) {
                Err(rustix::io::Errno::INTR) => continue,
                _ => break,
            }
        }
    }
}

struct RotatingFile {
    dir: PathBuf,
    lock: Arc<File>,
    current: Option<(NaiveDate, u64)>,
    cleaned: Option<NaiveDate>,
    cleanup_error: Option<io::Error>,
}

impl RotatingFile {
    fn new(dir: PathBuf) -> io::Result<Self> {
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
            cleaned: None,
            cleanup_error: None,
        };
        let _lock = FileLock::acquire(&file.lock)?;
        let today = Local::now().date_naive();
        file.cleanup(today);
        file.current = file.scan(today)?.map(|number| (today, number));
        Ok(file)
    }

    fn append_record(&mut self, record: &[u8]) -> io::Result<()> {
        let _lock = FileLock::acquire(&self.lock)?;
        self.append_record_locked(record, Local::now().date_naive())
    }

    // Caller holds .lock. The explicit date keeps boundary tests clock-independent.
    fn append_record_locked(&mut self, record: &[u8], today: NaiveDate) -> io::Result<()> {
        if record.len() as u64 > MAX_LOG_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log event too large",
            ));
        }
        if self.cleaned != Some(today) {
            self.cleanup(today);
        }
        let mut number = match self.current {
            Some((date, number)) if date == today => {
                match fs::symlink_metadata(self.path(today, number)) {
                    Ok(_) => Some(number),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => self.scan(today)?,
                    Err(error) => return Err(error),
                }
            }
            _ => self.scan(today)?,
        };
        if let Some(mut latest) = number {
            // Other pp instances create contiguous successors while holding .lock.
            while let Some(next) = latest.checked_add(1) {
                match fs::symlink_metadata(self.path(today, next)) {
                    Ok(_) => {
                        open_regular(&self.path(today, next), false, false)?;
                        latest = next;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                    Err(error) => return Err(error),
                }
            }
            number = Some(latest);
        }
        if let Some(number) = number {
            let mut file = open_regular(&self.path(today, number), false, false)?;
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
            match open_regular(&self.path(today, next), true, false) {
                Ok(mut file) => {
                    self.current = Some((today, next));
                    return file.write_all(record);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    // Validate conflicts rather than skipping unsafe paths.
                    open_regular(&self.path(today, next), false, false)?;
                    number = self.scan(today)?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn path(&self, date: NaiveDate, number: u64) -> PathBuf {
        self.dir.join(format!("pp.{date}.{number}.log"))
    }

    fn scan(&self, today: NaiveDate) -> io::Result<Option<u64>> {
        let mut maximum = None;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if let Some((date, number)) = entry.file_name().to_str().and_then(parse_name)
                && date == today
            {
                open_regular(&entry.path(), false, false)?;
                maximum = Some(maximum.map_or(number, |old: u64| old.max(number)));
            }
        }
        Ok(maximum)
    }

    fn cleanup(&mut self, today: NaiveDate) {
        self.cleaned = Some(today);
        let result = self.cleanup_before(today.checked_sub_days(Days::new(RETENTION_DAYS - 1)));
        if let Err(error) = result {
            self.remember_cleanup(error);
        }
    }

    fn remember_cleanup(&mut self, error: io::Error) {
        if self.cleanup_error.is_none() {
            self.cleanup_error = Some(error);
        }
    }

    fn cleanup_before(&mut self, cutoff: Option<NaiveDate>) -> io::Result<()> {
        let Some(cutoff) = cutoff else {
            return Ok(());
        };
        for entry in fs::read_dir(&self.dir)? {
            let result = (|| {
                let entry = entry?;
                if entry
                    .file_name()
                    .to_str()
                    .and_then(parse_name)
                    .is_some_and(|(date, _)| date < cutoff)
                    && entry.file_type()?.is_file()
                {
                    fs::remove_file(entry.path())?;
                }
                Ok::<_, io::Error>(())
            })();
            if let Err(error) = result {
                self.remember_cleanup(error);
            }
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::{
        collections::BTreeSet,
        os::unix::fs::{PermissionsExt, symlink},
        time::{Duration, SystemTime},
    };

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 19).unwrap()
    }

    fn append(file: &mut RotatingFile, record: &[u8], day: NaiveDate) -> io::Result<()> {
        let _lock = FileLock::acquire(&file.lock)?;
        file.append_record_locked(record, day)
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
        // Reopen the real lock without applying the host clock's retention policy.
        let lock = Arc::new(open_regular(&path.join(".lock"), false, false).unwrap());
        let mut file = RotatingFile {
            dir: path,
            lock,
            current: None,
            cleaned: None,
            cleanup_error: None,
        };
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
    fn retention_uses_strict_names_dates_and_not_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = RotatingFile::new(dir.path().join("logs")).unwrap();
        for name in [
            "pp.2026-09-19.1.log",
            "pp.2026-09-20.1.log",
            "pp.2026-10-19.1.log",
            "pp.2026-11-19.1.log",
            "pp.2026-02-30.1.log",
            "pp.2026-09-19.01.log",
            "pp.2026-09-19.0.log",
            "other",
        ] {
            fs::write(file.dir.join(name), b"{}\n").unwrap();
        }
        let old_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        File::options()
            .write(true)
            .open(file.path(date(), 1))
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        let sentinel = dir.path().join("sentinel");
        fs::write(&sentinel, b"untouched").unwrap();
        symlink(&sentinel, file.dir.join("pp.2026-09-18.1.log")).unwrap();
        fs::create_dir(file.dir.join("pp.2026-09-17.1.log")).unwrap();
        file.cleaned = None;
        append(&mut file, b"{}\n", date()).unwrap();
        assert!(!file.dir.join("pp.2026-09-19.1.log").exists());
        for name in [
            "pp.2026-09-20.1.log",
            "pp.2026-11-19.1.log",
            "pp.2026-02-30.1.log",
            "pp.2026-09-19.01.log",
            "pp.2026-09-19.0.log",
            "other",
            "pp.2026-09-18.1.log",
            "pp.2026-09-17.1.log",
        ] {
            assert!(file.dir.join(name).exists(), "{name}");
        }
        assert_eq!(fs::read(&sentinel).unwrap(), b"untouched");
        append(&mut file, b"{}\n", date().succ_opt().unwrap()).unwrap();
        assert!(!file.dir.join("pp.2026-09-20.1.log").exists());
        assert_eq!(
            fs::read(file.path(date().succ_opt().unwrap(), 1)).unwrap(),
            b"{}\n"
        );
        assert!(
            fs::read(file.path(date(), 1))
                .unwrap()
                .ends_with(b"{}\n{}\n")
        );
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
}
