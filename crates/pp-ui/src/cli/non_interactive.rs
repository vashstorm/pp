//! Non-interactive phase 2 infrastructure: `--timeout`/`SIGINT`/`SIGTERM` cancellation
//! guard, piped stdin reads, `--output-schema` loading and atomic `-o` writes.

use anyhow::{Context, Result, bail, ensure};
use pp_core::cancellation::{CancellationToken, Cancelled, SignalCancelled, TimedOut, UsageError};
use std::{
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI32, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const MAX_STDIN_BYTES: usize = 16 * 1024 * 1024;
const MAX_SCHEMA_BYTES: usize = 1024 * 1024;
const MAX_DURATION: Duration = Duration::from_secs(365 * 24 * 60 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CancelCause {
    Timeout(Duration),
    Signal(i32),
}

static SIGNAL_PIPE_WRITER: AtomicI32 = AtomicI32::new(-1);

extern "C" fn handle_termination_signal(signal: libc::c_int) {
    let fd = SIGNAL_PIPE_WRITER.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = signal as u8;
        let _ = unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    }
}

/// Owns the `SIGINT`/`SIGTERM` and `--timeout` cancellation of non-interactive tasks.
pub struct RunGuard {
    token: CancellationToken,
    cause: Arc<Mutex<Option<CancelCause>>>,
    timeout_stop: Option<mpsc::Sender<()>>,
    timeout_thread: Option<JoinHandle<()>>,
    signals: Option<SignalWatcher>,
}

struct SignalWatcher {
    write_fd: libc::c_int,
    read_fd: libc::c_int,
    old_int: libc::sigaction,
    old_term: libc::sigaction,
    thread: Option<JoinHandle<()>>,
}

impl RunGuard {
    /// Installs only the `--timeout` timer (does not take over process-wide signals, so
    /// it suits unit tests and embedded use).
    pub fn with_timeout_only(timeout: Option<Duration>) -> Self {
        let token = CancellationToken::new();
        let cause = Arc::new(Mutex::new(None));
        let (timeout_stop, timeout_thread) = spawn_timeout_watcher(timeout, &token, &cause);
        Self {
            token,
            cause,
            timeout_stop,
            timeout_thread,
            signals: None,
        }
    }

    /// Installs `--timeout` (when configured) plus the `SIGINT`/`SIGTERM` self-pipe
    /// graceful cancellation guard.
    pub fn install(timeout: Option<Duration>) -> Result<Self> {
        let mut guard = Self::with_timeout_only(timeout);
        guard.signals = Some(SignalWatcher::install(&guard.token, &guard.cause)?);
        Ok(guard)
    }

    pub fn token(&self) -> &CancellationToken {
        &self.token
    }

    /// Maps cancellation (`Cancelled` or an error caused by a timeout/signal abort) to a
    /// typed error carrying the semantic exit code.
    pub fn remap_error(&self, error: anyhow::Error) -> anyhow::Error {
        let cause = *self.cause.lock().unwrap_or_else(|e| e.into_inner());
        let is_cancelled = error.chain().any(|cause| cause.is::<Cancelled>());
        if is_cancelled || (self.token.is_cancelled() && cause.is_some()) {
            return match cause {
                Some(CancelCause::Timeout(duration)) => TimedOut { duration }.into(),
                Some(CancelCause::Signal(signal)) => SignalCancelled { signal }.into(),
                None => error,
            };
        }
        error
    }

    /// Builds the matching error object when the task loop returns
    /// `RunOutcome::Cancelled`.
    pub fn cancellation_error(&self) -> anyhow::Error {
        let cause = *self.cause.lock().unwrap_or_else(|e| e.into_inner());
        match cause {
            Some(CancelCause::Timeout(duration)) => TimedOut { duration }.into(),
            Some(CancelCause::Signal(signal)) => SignalCancelled { signal }.into(),
            None => Cancelled.into(),
        }
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.timeout_stop.take();
        if let Some(handle) = self.timeout_thread.take() {
            let _ = handle.join();
        }
        if let Some(mut signals) = self.signals.take() {
            signals.shutdown();
        }
    }
}

fn spawn_timeout_watcher(
    timeout: Option<Duration>,
    token: &CancellationToken,
    cause: &Arc<Mutex<Option<CancelCause>>>,
) -> (Option<mpsc::Sender<()>>, Option<JoinHandle<()>>) {
    let Some(duration) = timeout else {
        return (None, None);
    };
    let (tx, rx) = mpsc::channel();
    let token = token.clone();
    let cause = Arc::clone(cause);
    let handle = thread::Builder::new()
        .name("pp-timeout".into())
        .spawn(move || {
            if matches!(
                rx.recv_timeout(duration),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                let mut guard = cause.lock().unwrap_or_else(|e| e.into_inner());
                guard.get_or_insert(CancelCause::Timeout(duration));
                drop(guard);
                token.cancel();
            }
        })
        .ok();
    (Some(tx), handle)
}

impl SignalWatcher {
    fn install(token: &CancellationToken, cause: &Arc<Mutex<Option<CancelCause>>>) -> Result<Self> {
        let mut fds = [0 as libc::c_int; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error()).context("creating signal pipe");
        }
        let read_fd = fds[0];
        let write_fd = fds[1];
        set_cloexec(read_fd);
        set_cloexec(write_fd);
        set_nonblock(write_fd);

        if SIGNAL_PIPE_WRITER
            .compare_exchange(-1, write_fd, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            unsafe {
                libc::close(read_fd);
                libc::close(write_fd);
            }
            bail!("signal watcher already active in process");
        }

        let old_int = match install_handler(libc::SIGINT, handle_termination_signal) {
            Ok(old) => old,
            Err(error) => {
                SIGNAL_PIPE_WRITER.store(-1, Ordering::SeqCst);
                unsafe {
                    libc::close(read_fd);
                    libc::close(write_fd);
                }
                return Err(error);
            }
        };
        let old_term = match install_handler(libc::SIGTERM, handle_termination_signal) {
            Ok(old) => old,
            Err(error) => {
                unsafe { libc::sigaction(libc::SIGINT, &old_int, std::ptr::null_mut()) };
                SIGNAL_PIPE_WRITER.store(-1, Ordering::SeqCst);
                unsafe {
                    libc::close(read_fd);
                    libc::close(write_fd);
                }
                return Err(error);
            }
        };

        let token = token.clone();
        let cause = Arc::clone(cause);
        let thread = thread::Builder::new()
            .name("pp-signal".into())
            .spawn(move || {
                let mut seen = false;
                let mut buf = [0u8; 1];
                loop {
                    let n = unsafe { libc::read(read_fd, buf.as_mut_ptr().cast(), 1) };
                    if n == 0 {
                        break;
                    }
                    if n < 0 {
                        let err = io::Error::last_os_error();
                        if err.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        break;
                    }
                    let sig = libc::c_int::from(buf[0]);
                    if !seen {
                        seen = true;
                        let mut guard = cause.lock().unwrap_or_else(|e| e.into_inner());
                        guard.get_or_insert(CancelCause::Signal(sig));
                        drop(guard);
                        token.cancel();
                    } else {
                        reset_default_handler(sig);
                        unsafe { libc::raise(sig) };
                    }
                }
            })
            .ok();

        Ok(Self {
            write_fd,
            read_fd,
            old_int,
            old_term,
            thread,
        })
    }

    fn shutdown(&mut self) {
        unsafe {
            libc::sigaction(libc::SIGINT, &self.old_int, std::ptr::null_mut());
            libc::sigaction(libc::SIGTERM, &self.old_term, std::ptr::null_mut());
        }
        SIGNAL_PIPE_WRITER.store(-1, Ordering::SeqCst);
        unsafe { libc::close(self.write_fd) };
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        unsafe { libc::close(self.read_fd) };
    }
}

fn install_handler(
    signal: libc::c_int,
    handler: extern "C" fn(libc::c_int),
) -> Result<libc::sigaction> {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        let mut previous: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(signal, &action, &mut previous) != 0 {
            return Err(io::Error::last_os_error()).context("installing signal handler");
        }
        Ok(previous)
    }
}

fn reset_default_handler(signal: libc::c_int) {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(signal, &action, std::ptr::null_mut());
    }
}

fn set_cloexec(fd: libc::c_int) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

fn set_nonblock(fd: libc::c_int) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// Parses a `--timeout` duration: bare seconds or combined units (`500ms`, `30s`, `5m`,
/// `1h`, `1m30s`).
pub fn parse_duration(raw: &str) -> Result<Duration> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(UsageError("timeout duration must not be empty".into()).into());
    }
    if text
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return parse_unit_duration(text, "s");
    }

    let bytes = text.as_bytes();
    let mut index = 0usize;
    let mut total = Duration::ZERO;
    while index < bytes.len() {
        let num_start = index;
        while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'.') {
            index += 1;
        }
        if index == num_start || index == bytes.len() {
            return Err(UsageError(format!(
                "invalid --timeout duration `{raw}`; expected e.g. 30, 500ms, 30s, 5m, 1m30s"
            ))
            .into());
        }
        let num = &text[num_start..index];
        let unit_start = index;
        while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
            index += 1;
        }
        let unit = text[unit_start..index].to_ascii_lowercase();
        let part = parse_unit_duration(num, &unit)?;
        total = total
            .checked_add(part)
            .filter(|duration| *duration <= MAX_DURATION)
            .ok_or_else(|| UsageError(format!("--timeout duration `{raw}` is too large")))?;
    }

    if total.is_zero() {
        return Err(UsageError("--timeout must be greater than zero".into()).into());
    }
    Ok(total)
}

fn parse_unit_duration(number: &str, unit: &str) -> Result<Duration> {
    let value: f64 = number
        .parse()
        .map_err(|_| UsageError(format!("invalid duration number `{number}`")))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(UsageError("--timeout must be a positive finite duration".into()).into());
    }
    let scale = match unit {
        "ms" => 0.001,
        "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        _ => {
            return Err(UsageError(format!(
                "unsupported duration unit `{unit}`; use ms, s, m, or h"
            ))
            .into());
        }
    };
    let secs = value * scale;
    if !secs.is_finite() || secs > MAX_DURATION.as_secs_f64() {
        return Err(UsageError("--timeout duration is too large".into()).into());
    }
    let duration = Duration::from_secs_f64(secs);
    if duration.is_zero() {
        return Err(UsageError("--timeout must be greater than zero".into()).into());
    }
    Ok(duration)
}

/// Reads input to EOF when stdin is not a terminal (pipe/redirect), capped at 16 MiB.
pub fn read_piped_stdin() -> Result<Option<String>> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return Ok(None);
    }
    let mut handle = stdin.lock().take(MAX_STDIN_BYTES as u64 + 1);
    let mut bytes = Vec::new();
    handle
        .read_to_end(&mut bytes)
        .context("reading prompt from stdin")?;
    if bytes.len() > MAX_STDIN_BYTES {
        return Err(UsageError("stdin prompt exceeds 16 MiB limit".into()).into());
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| UsageError("stdin prompt must be valid UTF-8".into()))?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(text))
}

/// Combines an explicit command-line prompt with piped stdin content; returns
/// `UsageError` when both are empty.
pub fn combine_prompt_and_stdin(prompt: Option<String>, stdin: Option<String>) -> Result<String> {
    if let Some(ref explicit) = prompt
        && explicit.trim().is_empty()
    {
        return Err(UsageError("prompt must not be blank".into()).into());
    }
    let cleaned_stdin = stdin.and_then(|text| {
        let trimmed = text.trim_end_matches(['\r', '\n']);
        (!trimmed.trim().is_empty()).then(|| trimmed.to_owned())
    });
    match (prompt, cleaned_stdin) {
        (Some(explicit), Some(stdin_text)) => Ok(format!("{explicit}\n\n{stdin_text}")),
        (Some(explicit), None) | (None, Some(explicit)) => Ok(explicit),
        (None, None) => Err(UsageError("prompt is required in -p mode".into()).into()),
    }
}

/// Loads and pre-validates `--output-schema` (inline JSON object or a JSON Schema file
/// path).
pub fn load_output_schema(spec: &str) -> Result<serde_json::Value> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(UsageError("--output-schema must not be empty".into()).into());
    }
    let text = if trimmed.starts_with('{') {
        trimmed.to_owned()
    } else {
        let meta = fs::metadata(spec)
            .map_err(|error| UsageError(format!("reading --output-schema `{spec}`: {error}")))?;
        if meta.len() > MAX_SCHEMA_BYTES as u64 {
            return Err(UsageError(format!("--output-schema `{spec}` exceeds 1 MiB limit")).into());
        }
        fs::read_to_string(spec)
            .map_err(|error| UsageError(format!("reading --output-schema `{spec}`: {error}")))?
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| UsageError(format!("invalid JSON in --output-schema: {error}")))?;
    ensure!(
        value.is_object(),
        UsageError("--output-schema must be a JSON object".into())
    );
    pp_tools::validate_schema(&value).map_err(|error| UsageError(format!("{error:#}")))?;
    Ok(value)
}

/// Writes the non-interactive final result to `-o` / `--output-file` via a sibling
/// temporary file plus atomic rename.
pub fn write_output_file(path: &Path, content: &str) -> Result<()> {
    let parent = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    fs::create_dir_all(parent)
        .with_context(|| format!("creating output directory {}", parent.display()))?;

    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|_| anyhow::anyhow!("generating temporary file name"))?;
    let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let temp_path = parent.join(format!(".pp-output-{suffix}.tmp"));

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .with_context(|| format!("creating output file {}", path.display()))?;

    let write_result = (|| -> io::Result<()> {
        file.write_all(content.as_bytes())?;
        if !content.is_empty() && !content.ends_with('\n') {
            file.write_all(b"\n")?;
        }
        file.flush()
    })();
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error).with_context(|| format!("writing output file {}", path.display()));
    }
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error).with_context(|| format!("saving output file {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations_and_rejects_invalid_values() {
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("1m30s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1.5m").unwrap(), Duration::from_secs(90));
        for bad in ["", "0", "0s", "-5s", "10d", "abc", "1mabc"] {
            let err = parse_duration(bad).unwrap_err();
            assert!(err.is::<UsageError>(), "{bad}: {err}");
        }
    }

    #[test]
    fn combines_prompt_and_stdin_cases() {
        assert_eq!(
            combine_prompt_and_stdin(Some("review".into()), Some("diff\n".into())).unwrap(),
            "review\n\ndiff"
        );
        assert_eq!(
            combine_prompt_and_stdin(None, Some("piped\n".into())).unwrap(),
            "piped"
        );
        assert!(
            combine_prompt_and_stdin(None, Some("   \n".into()))
                .unwrap_err()
                .is::<UsageError>()
        );
        assert!(
            combine_prompt_and_stdin(Some("   ".into()), None)
                .unwrap_err()
                .is::<UsageError>()
        );
    }

    #[test]
    fn loads_output_schema_and_writes_output_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let schema_path = dir.path().join("schema.json");
        fs::write(&schema_path, r#"{"type":"object","required":["ok"]}"#).unwrap();

        let from_file = load_output_schema(schema_path.to_str().unwrap()).unwrap();
        assert_eq!(from_file["type"], "object");
        assert!(load_output_schema(r#"{"type":"array"}"#).is_ok());
        assert!(load_output_schema("123").unwrap_err().is::<UsageError>());
        assert!(
            load_output_schema(dir.path().join("missing.json").to_str().unwrap())
                .unwrap_err()
                .is::<UsageError>()
        );

        let out_path = dir.path().join("nested/out.txt");
        write_output_file(&out_path, "hello").unwrap();
        assert_eq!(fs::read_to_string(&out_path).unwrap(), "hello\n");
    }

    #[test]
    fn run_guard_timeout_cancels_and_remaps_to_timed_out() {
        let guard = RunGuard::with_timeout_only(Some(Duration::from_millis(20)));
        for _ in 0..100 {
            if guard.token().is_cancelled() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(guard.token().is_cancelled());
        assert!(guard.cancellation_error().is::<TimedOut>());
        assert!(guard.remap_error(Cancelled.into()).is::<TimedOut>());
    }
}
