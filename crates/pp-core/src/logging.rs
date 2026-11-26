mod debug;
mod diagnostics;
pub mod perf;
mod rotate;

pub use diagnostics::{FailureContext, diagnostic_text, register_secret};
use rotate::{FileLock, Namespace, RotatingFile, parse_prompt_name};

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use anyhow::{Context, ensure};
use chrono::NaiveDate;
use rustix::fs::OFlags;
use tracing_subscriber::{
    filter::Targets, fmt::writer::MakeWriter, layer::SubscriberExt, util::SubscriberInitExt,
};

pub(crate) const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

pub struct LoggingGuard {
    state: Arc<Mutex<WriterState>>,
}

impl LoggingGuard {
    pub fn take_error(&self) -> Option<io::Error> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .error
            .take()
    }
}

pub fn init() -> anyhow::Result<LoggingGuard> {
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
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(
            event = "panic",
            pid = std::process::id(),
            detail = diagnostic_text(&info.to_string()),
            backtrace = diagnostic_text(&std::backtrace::Backtrace::force_capture().to_string()),
        );
        previous_hook(info);
    }));
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
pub fn gc(cutoff: NaiveDate) -> anyhow::Result<(u64, u64)> {
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
            Namespace::ALL
                .iter()
                .find_map(|namespace| namespace.parse_file_name(name).map(|(date, _)| date))
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

/// 清理已删除会话的首轮提示词日志，沿用日志写入的跨进程锁。
pub fn remove_session_prompt(id: &str) -> anyhow::Result<bool> {
    let name = format!("prompt-debug.{id}");
    ensure!(
        parse_prompt_name(&name).is_some(),
        "invalid session prompt log name"
    );
    let dir = logs_dir()?;
    let path = dir.join(name);
    if !regular_file_exists(&path)? {
        return Ok(false);
    }
    ensure!(
        fs::symlink_metadata(&dir)?.is_dir(),
        "invalid log directory"
    );
    let lock = Arc::new(open_regular(&dir.join(".lock"), false, true)?);
    let _lock = FileLock::acquire(&lock)?;
    if !regular_file_exists(&path)? {
        return Ok(false);
    }
    fs::remove_file(path)?;
    Ok(true)
}

fn regular_file_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Opt-in human-readable debug text log. It shares `~/.pp/logs` and its `.lock` with the
/// ordinary pp log but owns an independent `debug.DATE.NNNN` namespace with
/// the same size limit. Initial prompts instead use one immutable
/// `prompt-debug.SESSION_ID` file per session. Neither path touches the global
/// tracing subscriber; files are only created when a record is written.
pub struct DebugLog {
    file: RotatingFile,
}

impl DebugLog {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            file: RotatingFile::new(logs_dir()?)?,
        })
    }

    /// Writes the session's initial prompt once, without rotating or appending.
    pub fn record_prompt(
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
    pub fn record(&mut self, record: &impl serde::Serialize) -> io::Result<()> {
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

fn open_regular(path: &Path, create_new: bool, create: bool) -> io::Result<File> {
    open_regular_with_len(path, create_new, create).map(|(file, _)| file)
}

fn open_regular_with_len(path: &Path, create_new: bool, create: bool) -> io::Result<(File, u64)> {
    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(create)
        .create_new(create_new)
        .mode(0o600)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "log path is not a regular file",
        ));
    }
    Ok((file, metadata.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use rustix::fs::{FlockOperation, flock};
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
            if entry
                .file_name()
                .to_str()
                .and_then(|name| Namespace::Pp.parse_file_name(name))
                .is_none()
            {
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

        // Debug 文本日志共用同一体积上限：超限记录整条拒绝，文件保持原样。
        let mut debug = DebugLog {
            file: RotatingFile::new(dir.path().join("debug-logs")).unwrap(),
        };
        debug.record(&serde_json::json!({"turn": 1})).unwrap();
        let path = debug
            .file
            .path_for(Namespace::Debug, Local::now().date_naive(), 1);
        let before = fs::read(&path).unwrap();
        let oversize = serde_json::json!({"pad": "x".repeat(MAX_LOG_BYTES as usize)});
        assert_eq!(
            debug.record(&oversize).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(fs::read(&path).unwrap(), before);
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
    fn prompt_logs_are_immutable_bounded_and_retained_separately() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let mut log = DebugLog {
            file: RotatingFile::new(logs.clone()).unwrap(),
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
