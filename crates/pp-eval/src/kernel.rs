//! Persistent Python / JavaScript kernel child processes, process-group supervision and the NDJSON duplex frame loop.

use crate::js_runtime::probe_js;
use crate::runtime::{PythonRuntime, probe_python};
use anyhow::{Context, Result, bail};
use base64::Engine;
use pp_core::cancellation::{CancellationToken, Cancelled, CleanupError};
use pp_core::events::{AgentEvent, OutputStream};
use pp_workspace::output::OutputSink;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const RUNNER_PY: &str = include_str!("runner.py");
const RUNNER_JS: &str = include_str!("runner.js");
const PREVIEW_INTERVAL: Duration = Duration::from_millis(50);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const INTERRUPT_GRACE: Duration = Duration::from_secs(2);
const SHUTDOWN_GRACE: Duration = Duration::from_millis(800);
const READER_DRAIN_GRACE: Duration = Duration::from_millis(500);
const STDERR_TAIL_CAP: usize = 8 * 1024;
static NEXT_TMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language {
    Python,
    JavaScript,
}

impl Language {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Python => "py",
            Self::JavaScript => "js",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Python => "Python",
            Self::JavaScript => "JavaScript",
        }
    }
}

#[derive(Debug)]
enum KernelFrame {
    Started,
    Stdout {
        id: String,
        data: String,
    },
    Stderr {
        id: String,
        data: String,
    },
    RawStdout(Vec<u8>),
    RawStderr(Vec<u8>),
    Image {
        id: String,
        mime_type: String,
        data: String,
    },
    ToolCall {
        id: String,
        call_id: String,
        name: String,
        args: Value,
    },
    Error {
        id: String,
        ename: String,
        evalue: String,
        traceback: Vec<String>,
    },
    Done {
        id: String,
        status: String,
        execution_count: u64,
        cancelled: bool,
    },
    Eof,
}

#[derive(Deserialize)]
struct RawFrame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    image: Option<RawImage>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    args: Option<Value>,
    #[serde(default)]
    ename: Option<String>,
    #[serde(default)]
    evalue: Option<String>,
    #[serde(default)]
    traceback: Option<Vec<String>>,
    #[serde(default)]
    status: Option<String>,
    #[serde(rename = "executionCount", default)]
    execution_count: Option<u64>,
    #[serde(default)]
    cancelled: Option<bool>,
}

#[derive(Deserialize)]
struct RawImage {
    #[serde(rename = "mimeType")]
    mime_type: String,
    data: String,
}

pub(crate) struct RunningKernel {
    language: Language,
    runtime_name: Option<&'static str>,
    child: Child,
    pgid: libc::pid_t,
    ctrl_in: Option<File>,
    frames: Option<Receiver<KernelFrame>>,
    cell_active: Arc<AtomicBool>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    reader_handle: Option<JoinHandle<()>>,
    reader_done: Option<Receiver<()>>,
    stderr_handle: Option<JoinHandle<()>>,
    stderr_done: Option<Receiver<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedImage {
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

pub struct CellRequest<'a> {
    pub language: Language,
    pub cwd: &'a Path,
    pub cache_dir: &'a Path,
    pub interpreter: Option<&'a str>,
    pub code: &'a str,
    pub call_id: &'a str,
    pub timeout_seconds: u64,
    pub reset: bool,
    pub sandbox: Option<Arc<pp_sandbox::Policy>>,
}

pub struct CellOutcome {
    pub value: Value,
    pub images: Vec<CapturedImage>,
}

fn ensure_cached_script(
    cache_dir: &Path,
    prefix: &str,
    ext: &str,
    content: &str,
) -> Result<PathBuf> {
    fs::create_dir_all(cache_dir)
        .with_context(|| format!("creating eval cache dir {}", cache_dir.display()))?;
    let bytes = content.as_bytes();
    let hash = xxhash_rust::xxh32::xxh32(bytes, 0);
    let target = cache_dir.join(format!("{prefix}-{hash:08x}.{ext}"));
    let needs_write = match fs::read(&target) {
        Ok(existing) => existing != bytes,
        Err(_) => true,
    };
    if needs_write {
        let seq = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
        let tmp = cache_dir.join(format!(
            "{prefix}-{hash:08x}.{}.{seq}.tmp",
            std::process::id()
        ));
        fs::write(&tmp, bytes)
            .with_context(|| format!("writing runner script {}", tmp.display()))?;
        fs::rename(&tmp, &target)
            .with_context(|| format!("installing runner script {}", target.display()))?;
    }
    Ok(target)
}

fn parse_frame(line: &str) -> Option<KernelFrame> {
    let raw: RawFrame = serde_json::from_str(line).ok()?;
    Some(match raw.kind.as_str() {
        "started" => KernelFrame::Started,
        "stdout" => KernelFrame::Stdout {
            id: raw.id,
            data: raw.data.unwrap_or_default(),
        },
        "stderr" => KernelFrame::Stderr {
            id: raw.id,
            data: raw.data.unwrap_or_default(),
        },
        "image" => {
            let img = raw.image?;
            KernelFrame::Image {
                id: raw.id,
                mime_type: img.mime_type,
                data: img.data,
            }
        }
        "tool_call" => KernelFrame::ToolCall {
            id: raw.id,
            call_id: raw.call_id?,
            name: raw.name?,
            args: raw.args.unwrap_or_else(|| json!({})),
        },
        "error" => KernelFrame::Error {
            id: raw.id,
            ename: raw.ename.unwrap_or_else(|| "Error".to_owned()),
            evalue: raw.evalue.unwrap_or_default(),
            traceback: raw.traceback.unwrap_or_default(),
        },
        "done" => KernelFrame::Done {
            id: raw.id,
            status: raw.status.unwrap_or_else(|| "ok".to_owned()),
            execution_count: raw.execution_count.unwrap_or(0),
            cancelled: raw.cancelled.unwrap_or(false),
        },
        _ => return None,
    })
}

fn create_cloexec_pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("creating eval control pipe");
    }
    let r = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let w = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    for fd in [r.as_raw_fd(), w.as_raw_fd()] {
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    Ok((r, w))
}

fn set_nonblocking(fd: libc::c_int) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn append_stderr_tail(stderr_tail: &Arc<Mutex<Vec<u8>>>, chunk: &[u8]) {
    if chunk.is_empty() {
        return;
    }
    if let Ok(mut tail) = stderr_tail.lock() {
        tail.extend_from_slice(chunk);
        if tail.len() > STDERR_TAIL_CAP {
            let drop_len = tail.len() - STDERR_TAIL_CAP;
            tail.drain(..drop_len);
        }
    }
}

/// Drain all immediately readable bytes from a non-blocking raw fd, sending the matching frames while a cell is active.
/// Returns `false` when the channel receiver is gone or a write failed; `eof` is true once the fd reached EOF.
fn drain_raw_fd(
    file: &mut File,
    is_stderr: bool,
    cell_active: &AtomicBool,
    stderr_tail: &Arc<Mutex<Vec<u8>>>,
    tx: &SyncSender<KernelFrame>,
    eof: &mut bool,
) -> bool {
    let mut buf = [0u8; 8192];
    loop {
        match file.read(&mut buf) {
            Ok(0) => {
                *eof = true;
                return true;
            }
            Ok(n) => {
                let slice = &buf[..n];
                if is_stderr {
                    append_stderr_tail(stderr_tail, slice);
                }
                if cell_active.load(Ordering::Acquire) {
                    let frame = if is_stderr {
                        KernelFrame::RawStderr(slice.to_vec())
                    } else {
                        KernelFrame::RawStdout(slice.to_vec())
                    };
                    if tx.send(frame).is_err() {
                        return false;
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return true,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                *eof = true;
                return true;
            }
        }
    }
}

impl RunningKernel {
    fn spawn_python(
        runtime: &PythonRuntime,
        cwd: &Path,
        cache_dir: &Path,
        sandbox: Option<&pp_sandbox::Policy>,
    ) -> Result<Self> {
        let script = ensure_cached_script(cache_dir, "pp-eval-runner", "py", RUNNER_PY)?;
        let mut cmd = if let Some(policy) = sandbox {
            let (wrapper_bin, wrapper_args) = policy
                .wrap_command(
                    &runtime.python_path.to_string_lossy(),
                    &[OsString::from("-u"), script.as_os_str().to_os_string()],
                )
                .context("building sandbox wrapper for Python kernel")?;
            let mut c = Command::new(wrapper_bin);
            c.args(wrapper_args);
            c
        } else {
            let mut c = Command::new(&runtime.python_path);
            c.arg("-u").arg(&script);
            c
        };
        cmd.current_dir(cwd)
            .env_clear()
            .envs(&runtime.env)
            .env("PYTHONUNBUFFERED", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .env("MPLBACKEND", "Agg")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The child runs in its own process group so a timeout/kill also reaps any external children it spawned.
            .process_group(0);
        if let Some(policy) = sandbox {
            cmd.env("PP_SANDBOX", policy.backend());
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning Python kernel {}", runtime.python_path.display()))?;
        let pgid = child.id() as libc::pid_t;
        let stdin = child.stdin.take().context("missing Python kernel stdin")?;
        let ctrl_in = File::from(OwnedFd::from(stdin));
        let stdout = child
            .stdout
            .take()
            .context("missing Python kernel stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("missing Python kernel stderr")?;

        let (tx, rx) = mpsc::sync_channel::<KernelFrame>(512);
        let (reader_done_tx, reader_done_rx) = mpsc::channel::<()>();
        let reader_handle = thread::Builder::new()
            .name("pp-eval-stdout".to_owned())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => {
                            let _ = tx.send(KernelFrame::Eof);
                            break;
                        }
                        Ok(_) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            if let Some(frame) = parse_frame(trimmed)
                                && tx.send(frame).is_err()
                            {
                                break;
                            }
                        }
                    }
                }
                let _ = reader_done_tx.send(());
            })
            .context("spawning Python stdout reader")?;

        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf = stderr_tail.clone();
        let (stderr_done_tx, stderr_done_rx) = mpsc::channel::<()>();
        let stderr_handle = thread::Builder::new()
            .name("pp-eval-stderr".to_owned())
            .spawn(move || {
                let mut reader = stderr;
                let mut buf = [0u8; 2048];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => append_stderr_tail(&stderr_buf, &buf[..n]),
                    }
                }
                let _ = stderr_done_tx.send(());
            })
            .context("spawning Python stderr reader")?;

        Ok(Self {
            language: Language::Python,
            runtime_name: None,
            child,
            pgid,
            ctrl_in: Some(ctrl_in),
            frames: Some(rx),
            cell_active: Arc::new(AtomicBool::new(false)),
            stderr_tail,
            reader_handle: Some(reader_handle),
            reader_done: Some(reader_done_rx),
            stderr_handle: Some(stderr_handle),
            stderr_done: Some(stderr_done_rx),
        })
    }

    fn spawn_js(
        runtime: &crate::js_runtime::JsRuntime,
        cwd: &Path,
        cache_dir: &Path,
        sandbox: Option<&pp_sandbox::Policy>,
    ) -> Result<Self> {
        let script = ensure_cached_script(cache_dir, "pp-eval-runner", "cjs", RUNNER_JS)?;
        let (ctrl_in_r, ctrl_in_w) = create_cloexec_pipe()?;
        let (ctrl_out_r, ctrl_out_w) = create_cloexec_pipe()?;
        let in_r_fd = ctrl_in_r.as_raw_fd();
        let out_w_fd = ctrl_out_w.as_raw_fd();

        let mut cmd = if let Some(policy) = sandbox {
            let (wrapper_bin, wrapper_args) = policy
                .wrap_command(
                    &runtime.executable.to_string_lossy(),
                    &[script.as_os_str().to_os_string()],
                )
                .context("building sandbox wrapper for JavaScript kernel")?;
            let mut c = Command::new(wrapper_bin);
            c.args(wrapper_args);
            c
        } else {
            let mut c = Command::new(&runtime.executable);
            c.arg(&script);
            c
        };

        cmd.current_dir(cwd)
            .env_clear()
            .envs(&runtime.env)
            .env("PP_EVAL_CTRL_IN_FD", "3")
            .env("PP_EVAL_CTRL_OUT_FD", "4")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(policy) = sandbox {
            cmd.env("PP_SANDBOX", policy.backend());
        }

        // Map the child end of the control pipes to fd 3 (CTRL_IN) and fd 4 (CTRL_OUT) and clear FD_CLOEXEC,
        // keeping them out of standard stdio so fs.writeSync(1/2) and child output never pollute NDJSON control frames.
        unsafe {
            cmd.pre_exec(move || {
                let mut src_in = in_r_fd;
                let mut src_out = out_w_fd;
                if src_in == 4 {
                    src_in = libc::fcntl(src_in, libc::F_DUPFD_CLOEXEC, 5);
                    if src_in < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if src_out == 3 {
                    src_out = libc::fcntl(src_out, libc::F_DUPFD_CLOEXEC, 5);
                    if src_out < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if src_in == 3 {
                    if libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                } else {
                    if libc::dup2(src_in, 3) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(src_in);
                }
                if src_out == 4 {
                    if libc::fcntl(4, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                } else {
                    if libc::dup2(src_out, 4) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(src_out);
                }
                Ok(())
            });
        }

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "spawning {} kernel {}",
                runtime.kind.display_name(),
                runtime.executable.display()
            )
        })?;
        drop(ctrl_in_r);
        drop(ctrl_out_w);

        let pgid = child.id() as libc::pid_t;
        let ctrl_in = File::from(ctrl_in_w);
        let mut ctrl_out = File::from(ctrl_out_r);
        let mut raw_stdout = File::from(OwnedFd::from(
            child
                .stdout
                .take()
                .context("missing JavaScript kernel stdout")?,
        ));
        let mut raw_stderr = File::from(OwnedFd::from(
            child
                .stderr
                .take()
                .context("missing JavaScript kernel stderr")?,
        ));

        let _ = set_nonblocking(ctrl_out.as_raw_fd());
        let _ = set_nonblocking(raw_stdout.as_raw_fd());
        let _ = set_nonblocking(raw_stderr.as_raw_fd());

        let (tx, rx) = mpsc::sync_channel::<KernelFrame>(512);
        let cell_active = Arc::new(AtomicBool::new(false));
        let active_flag = Arc::clone(&cell_active);
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf = Arc::clone(&stderr_tail);
        let (reader_done_tx, reader_done_rx) = mpsc::channel::<()>();

        // A single poll thread multiplexes the control channel and raw fd 1/2: drain the raw pipes before every control-frame read/send,
        // guaranteeing strict ordering and zero races between low-level writes such as `fs.writeSync(1, ...)` and the `done` frame.
        let reader_handle = thread::Builder::new()
            .name("pp-eval-js-io".to_owned())
            .spawn(move || {
                let mut ctrl_buf = Vec::<u8>::new();
                let mut read_buf = [0u8; 8192];
                let mut ctrl_eof = false;
                let mut stdout_eof = false;
                let mut stderr_eof = false;

                'io: loop {
                    if !stdout_eof
                        && !drain_raw_fd(
                            &mut raw_stdout,
                            false,
                            &active_flag,
                            &stderr_buf,
                            &tx,
                            &mut stdout_eof,
                        )
                    {
                        break 'io;
                    }
                    if !stderr_eof
                        && !drain_raw_fd(
                            &mut raw_stderr,
                            true,
                            &active_flag,
                            &stderr_buf,
                            &tx,
                            &mut stderr_eof,
                        )
                    {
                        break 'io;
                    }

                    if !ctrl_eof {
                        loop {
                            match ctrl_out.read(&mut read_buf) {
                                Ok(0) => {
                                    ctrl_eof = true;
                                    break;
                                }
                                Ok(n) => {
                                    ctrl_buf.extend_from_slice(&read_buf[..n]);
                                }
                                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                                    continue;
                                }
                                Err(_) => {
                                    ctrl_eof = true;
                                    break;
                                }
                            }
                        }

                        while let Some(nl) = ctrl_buf.iter().position(|&b| b == b'\n') {
                            let line_bytes: Vec<u8> = ctrl_buf.drain(..=nl).collect();
                            let line = String::from_utf8_lossy(&line_bytes);
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            if let Some(frame) = parse_frame(trimmed) {
                                // Drain any raw fd output still buffered in the kernel once more before the control frame enters the channel.
                                if matches!(
                                    frame,
                                    KernelFrame::ToolCall { .. }
                                        | KernelFrame::Error { .. }
                                        | KernelFrame::Done { .. }
                                ) {
                                    if !stdout_eof
                                        && !drain_raw_fd(
                                            &mut raw_stdout,
                                            false,
                                            &active_flag,
                                            &stderr_buf,
                                            &tx,
                                            &mut stdout_eof,
                                        )
                                    {
                                        break 'io;
                                    }
                                    if !stderr_eof
                                        && !drain_raw_fd(
                                            &mut raw_stderr,
                                            true,
                                            &active_flag,
                                            &stderr_buf,
                                            &tx,
                                            &mut stderr_eof,
                                        )
                                    {
                                        break 'io;
                                    }
                                }
                                if tx.send(frame).is_err() {
                                    break 'io;
                                }
                            }
                        }
                    }

                    if ctrl_eof {
                        if !stdout_eof {
                            let _ = drain_raw_fd(
                                &mut raw_stdout,
                                false,
                                &active_flag,
                                &stderr_buf,
                                &tx,
                                &mut stdout_eof,
                            );
                        }
                        if !stderr_eof {
                            let _ = drain_raw_fd(
                                &mut raw_stderr,
                                true,
                                &active_flag,
                                &stderr_buf,
                                &tx,
                                &mut stderr_eof,
                            );
                        }
                        let _ = tx.send(KernelFrame::Eof);
                        break 'io;
                    }

                    let mut pollfds = Vec::with_capacity(3);
                    pollfds.push(libc::pollfd {
                        fd: ctrl_out.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    });
                    if !stdout_eof {
                        pollfds.push(libc::pollfd {
                            fd: raw_stdout.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        });
                    }
                    if !stderr_eof {
                        pollfds.push(libc::pollfd {
                            fd: raw_stderr.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        });
                    }
                    let rc = unsafe {
                        libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, 200)
                    };
                    if rc < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)
                    {
                        let _ = tx.send(KernelFrame::Eof);
                        break 'io;
                    }
                }
                let _ = reader_done_tx.send(());
            })
            .context("spawning JavaScript I/O reader")?;

        Ok(Self {
            language: Language::JavaScript,
            runtime_name: Some(runtime.kind.as_str()),
            child,
            pgid,
            ctrl_in: Some(ctrl_in),
            frames: Some(rx),
            cell_active,
            stderr_tail,
            reader_handle: Some(reader_handle),
            reader_done: Some(reader_done_rx),
            stderr_handle: None,
            stderr_done: None,
        })
    }

    /// Non-destructive check whether the main process already exited (without reaping the zombie), so `pgid` is still
    /// anchored by the current child when `shutdown()` calls `killpg`, preventing a misdirected kill after PID reuse.
    fn leader_exited_nonreap(&self) -> bool {
        let pid = self.child.id() as libc::id_t;
        if pid == 0 {
            return true;
        }
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        loop {
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc != 0 {
                let err = std::io::Error::last_os_error().raw_os_error();
                if err == Some(libc::EINTR) {
                    continue;
                }
                return err == Some(libc::ECHILD);
            }
            return info.si_signo != 0;
        }
    }

    fn is_alive(&mut self) -> bool {
        !self.leader_exited_nonreap()
    }

    fn send_line(&mut self, value: &Value) -> Result<()> {
        let Some(ctrl_in) = self.ctrl_in.as_mut() else {
            bail!(
                "{} kernel control channel is closed",
                self.language.display_name()
            );
        };
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        ctrl_in.write_all(line.as_bytes())?;
        ctrl_in.flush()?;
        Ok(())
    }

    fn send_signal_group(&mut self, sig: libc::c_int) {
        if self.pgid > 0 {
            // Signal the process group only while the leader has not been reaped via `wait`/`try_wait`, avoiding PID reuse.
            unsafe {
                libc::killpg(self.pgid, sig);
            }
        }
    }

    fn send_sigint_leader(&mut self) {
        if !self.leader_exited_nonreap() {
            let pid = self.child.id() as libc::pid_t;
            if pid > 0 {
                unsafe {
                    libc::kill(pid, libc::SIGINT);
                }
            }
        }
    }

    fn wait_exit(&mut self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        loop {
            if self.leader_exited_nonreap() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(15));
        }
    }

    fn stderr_summary(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|buf| String::from_utf8_lossy(&buf).trim().to_owned())
            .unwrap_or_default()
    }

    pub(crate) fn shutdown(mut self) -> Result<()> {
        self.cell_active.store(false, Ordering::Release);
        let _ = self.send_line(&json!({ "type": "exit" }));
        self.ctrl_in.take();
        // Drop the receiver first so a reader thread blocked in sync_channel::send on a full channel cannot self-deadlock.
        self.frames.take();
        if !self.wait_exit(SHUTDOWN_GRACE) {
            self.send_signal_group(libc::SIGTERM);
            if !self.wait_exit(SHUTDOWN_GRACE) {
                self.send_signal_group(libc::SIGKILL);
                if !self.wait_exit(SHUTDOWN_GRACE) {
                    return Err(CleanupError(anyhow::anyhow!(
                        "{} kernel process {} did not exit after SIGKILL",
                        self.language.display_name(),
                        self.child.id()
                    ))
                    .into());
                }
            }
        }
        // While the leader has exited (or crashed) but is not yet reaped, sweep any fork/Popen descendants left in the group,
        // then reap the leader to release the PID.
        self.send_signal_group(libc::SIGKILL);
        let _ = self.child.wait();
        join_bounded(self.reader_handle.take(), self.reader_done.take());
        join_bounded(self.stderr_handle.take(), self.stderr_done.take());
        Ok(())
    }
}

fn join_bounded(handle: Option<JoinHandle<()>>, done: Option<Receiver<()>>) {
    let (Some(handle), Some(done)) = (handle, done) else {
        return;
    };
    if !matches!(
        done.recv_timeout(READER_DRAIN_GRACE),
        Err(RecvTimeoutError::Timeout)
    ) {
        let _ = handle.join();
    }
}

#[derive(Default)]
pub(crate) struct KernelState {
    pub(crate) running: Option<RunningKernel>,
    pub(crate) next_req_id: u64,
    pub(crate) restarted_notice: bool,
    pub(crate) sandbox_fingerprint: Option<(pp_sandbox::EffectiveMode, u64)>,
    /// Test-only: the next in-loop kernel reap returns a fixed `CleanupError`, deterministically covering the cleanup-failure teardown.
    #[cfg(test)]
    fail_next_shutdown: bool,
}

impl KernelState {
    /// Take and reap the current long-running kernel, called uniformly by cleanup paths inside the main loop.
    /// Tests can inject a cleanup fault to simulate the "in-loop shutdown failure" error boundary.
    pub(crate) fn shutdown_running(&mut self) -> Result<()> {
        #[cfg(test)]
        if self.fail_next_shutdown {
            self.fail_next_shutdown = false;
            // Simulate the "SIGKILL sent but process not reaped" cleanup failure: kill the group first, then return the fixed error.
            if let Some(mut kernel) = self.running.take() {
                kernel.send_signal_group(libc::SIGKILL);
                drop(kernel);
            }
            return Err(CleanupError(anyhow::anyhow!(
                "eval kernel shutdown failed (injected for test)"
            ))
            .into());
        }
        match self.running.take() {
            Some(kernel) => kernel.shutdown(),
            None => Ok(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_next_shutdown_for_test(&mut self) {
        self.fail_next_shutdown = true;
    }
}

pub(crate) fn run_cell(
    state: &mut KernelState,
    req: CellRequest<'_>,
    cancellation: &CancellationToken,
    mut sink: OutputSink,
    emit: &mut dyn FnMut(AgentEvent),
    bridge: &mut dyn FnMut(&str, &Value) -> Result<Value>,
) -> Result<CellOutcome> {
    pp_core::cancellation::check(cancellation)?;

    let lang_label = req.language.display_name();
    let started = Instant::now();
    let desired_fp = req.sandbox.as_deref().map(|p| (p.mode(), p.fingerprint()));
    let mut notice: Option<String> = None;

    if req.reset {
        state.shutdown_running()?;
        state.restarted_notice = false;
    } else if state.running.is_some() && state.sandbox_fingerprint != desired_fp {
        let mode_changed = state.sandbox_fingerprint.map(|(m, _)| m) != desired_fp.map(|(m, _)| m);
        state.shutdown_running()?;
        state.restarted_notice = false;
        notice = Some(if mode_changed {
            "Kernel restarted: sandbox mode changed; earlier state is unavailable.".to_owned()
        } else {
            "Kernel restarted: sandbox policy changed; earlier state is unavailable.".to_owned()
        });
    } else if let Some(kernel) = state.running.as_mut()
        && !kernel.is_alive()
        && let Some(dead) = state.running.take()
    {
        let _ = dead.shutdown();
        state.restarted_notice = true;
    }

    if state.running.is_none() {
        let kernel = match req.language {
            Language::Python => {
                let runtime = probe_python(req.cwd, req.interpreter)?;
                RunningKernel::spawn_python(
                    &runtime,
                    req.cwd,
                    req.cache_dir,
                    req.sandbox.as_deref(),
                )?
            }
            Language::JavaScript => {
                let runtime = probe_js(req.cwd, req.interpreter)?;
                RunningKernel::spawn_js(&runtime, req.cwd, req.cache_dir, req.sandbox.as_deref())?
            }
        };
        state.running = Some(kernel);
        state.sandbox_fingerprint = desired_fp;
        if state.restarted_notice {
            notice = Some(format!(
                "Kernel restarted: the previous {lang_label} kernel died; earlier state is unavailable."
            ));
            state.restarted_notice = false;
        }
    }

    let runtime_name = state
        .running
        .as_ref()
        .and_then(|kernel| kernel.runtime_name);

    state.next_req_id = state.next_req_id.saturating_add(1);
    let rid = format!("eval-{}", state.next_req_id);
    let cwd_str = req.cwd.to_string_lossy().into_owned();

    if let Some(kernel) = state.running.as_ref() {
        kernel.cell_active.store(true, Ordering::Release);
    }

    let send_result = state
        .running
        .as_mut()
        .expect("kernel initialized")
        .send_line(&json!({
            "type": "exec",
            "id": rid,
            "code": req.code,
            "cwd": cwd_str,
        }));
    if let Err(err) = send_result {
        if let Some(dead) = state.running.take() {
            let _ = dead.shutdown();
        }
        state.restarted_notice = true;
        return Err(err.context(format!("sending cell to {lang_label} kernel")));
    }

    let mut images = Vec::new();
    let mut dirty = false;
    let mut last_preview = Instant::now();
    let active_budget = Duration::from_secs(req.timeout_seconds);
    let mut active_elapsed = Duration::ZERO;
    let mut last_tick = Instant::now();
    let mut interrupt_sent: Option<Instant> = None;
    let mut timed_out = false;
    let mut cancelled = false;
    let mut exec_status = "ok".to_owned();
    let mut execution_count = 0u64;
    let mut kernel_died = false;
    let mut error_summary: Option<String> = None;

    // The main frame loop sits inside an inner result boundary: any in-loop kernel reap failure (CleanupError)
    // first reaches the uniform, idempotent sink.finish() below, then the error is surfaced,
    // so an artifact never lingers forever in the Store's live table with unsealed metadata.
    let loop_result: Result<()> = (|| {
        loop {
            let now = Instant::now();
            active_elapsed += now.saturating_duration_since(last_tick);
            last_tick = now;

            if interrupt_sent.is_none() {
                if cancellation.is_cancelled() {
                    cancelled = true;
                    interrupt_sent = Some(now);
                    if let Some(kernel) = state.running.as_mut() {
                        let _ = kernel.send_line(&json!({ "type": "interrupt", "id": rid }));
                        kernel.send_sigint_leader();
                    }
                } else if active_elapsed >= active_budget {
                    timed_out = true;
                    interrupt_sent = Some(now);
                    if let Some(kernel) = state.running.as_mut() {
                        let _ = kernel.send_line(&json!({ "type": "interrupt", "id": rid }));
                        kernel.send_sigint_leader();
                    }
                }
            } else if let Some(sent_at) = interrupt_sent
                && now.saturating_duration_since(sent_at) >= INTERRUPT_GRACE
            {
                // The kernel main thread is stuck in an extension/blocking child process and cannot answer SIGINT: escalate by destroying the whole process group.
                state.shutdown_running()?;
                state.restarted_notice = true;
                kernel_died = true;
                exec_status = "error".to_owned();
                if error_summary.is_none() {
                    error_summary = Some(if timed_out {
                        format!(
                            "{lang_label} cell timed out after {}s and did not respond to SIGINT; kernel was terminated.",
                            req.timeout_seconds
                        )
                    } else {
                        format!(
                            "{lang_label} cell cancelled and did not respond to SIGINT; kernel was terminated."
                        )
                    });
                }
                break;
            }

            if dirty && last_preview.elapsed() >= PREVIEW_INTERVAL {
                emit(AgentEvent::ToolOutputSnapshot {
                    id: req.call_id.to_owned(),
                    output: sink.preview(),
                });
                dirty = false;
                last_preview = Instant::now();
            }

            let frame = {
                let Some(kernel) = state.running.as_ref() else {
                    kernel_died = true;
                    break;
                };
                let Some(frames) = kernel.frames.as_ref() else {
                    kernel_died = true;
                    break;
                };
                frames.recv_timeout(POLL_INTERVAL)
            };

            match frame {
                Ok(KernelFrame::Started) => {}
                Ok(KernelFrame::Stdout { id, data }) if id == rid => {
                    sink.append(OutputStream::Stdout, data.as_bytes());
                    dirty = true;
                }
                Ok(KernelFrame::Stderr { id, data }) if id == rid => {
                    sink.append(OutputStream::Stderr, data.as_bytes());
                    dirty = true;
                }
                Ok(KernelFrame::RawStdout(bytes)) => {
                    sink.append(OutputStream::Stdout, &bytes);
                    dirty = true;
                }
                Ok(KernelFrame::RawStderr(bytes)) => {
                    sink.append(OutputStream::Stderr, &bytes);
                    dirty = true;
                }
                Ok(KernelFrame::Stdout { .. } | KernelFrame::Stderr { .. }) => {}
                Ok(KernelFrame::Image {
                    id,
                    mime_type,
                    data,
                }) if id == rid => {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data.trim())
                    {
                        images.push(CapturedImage { mime_type, bytes });
                    }
                }
                Ok(KernelFrame::Image { .. }) => {}
                Ok(KernelFrame::ToolCall {
                    id,
                    call_id: bridge_id,
                    name,
                    args,
                }) if id == rid => {
                    if dirty {
                        emit(AgentEvent::ToolOutputSnapshot {
                            id: req.call_id.to_owned(),
                            output: sink.preview(),
                        });
                        dirty = false;
                        last_preview = Instant::now();
                    }
                    let reply = match bridge(&name, &args) {
                        Ok(value) => json!({
                            "type": "tool_result",
                            "call_id": bridge_id,
                            "value": value,
                        }),
                        Err(err)
                            if err
                                .chain()
                                .any(|c| c.is::<CleanupError>() || c.is::<Cancelled>()) =>
                        {
                            // A nested tool or post-hook was cancelled or failed to clean up: stop the kernel immediately
                            // and bubble the original type chain up, never swallowing it into an exception the script can catch.
                            let shutdown_res = state.shutdown_running();
                            state.restarted_notice = true;
                            return pp_core::cancellation::merge_cleanup_result(
                                Err(err),
                                shutdown_res,
                                &format!("stopping {lang_label} kernel"),
                            );
                        }
                        Err(err) => json!({
                            "type": "tool_result",
                            "call_id": bridge_id,
                            "error": format!("{err:#}"),
                        }),
                    };
                    // Wall-clock time spent inside a nested host tool call is not counted toward the compute budget;
                    // only the timer start is reset afterwards, so a slow host tool cannot extend the compute allowance.
                    last_tick = Instant::now();
                    if let Some(kernel) = state.running.as_mut()
                        && let Err(err) = kernel.send_line(&reply)
                    {
                        let _ = state
                            .running
                            .take()
                            .map(RunningKernel::shutdown)
                            .transpose()?;
                        state.restarted_notice = true;
                        kernel_died = true;
                        exec_status = "error".to_owned();
                        error_summary = Some(format!(
                            "{lang_label} kernel exited during tool bridge: {err:#}"
                        ));
                        break;
                    }
                }
                Ok(KernelFrame::ToolCall { .. }) => {}
                Ok(KernelFrame::Error {
                    id,
                    ename,
                    evalue,
                    traceback,
                }) if id == rid || id.is_empty() => {
                    let text = if traceback.is_empty() {
                        format!("{ename}: {evalue}\n")
                    } else {
                        let mut joined = traceback.join("\n");
                        if !joined.ends_with('\n') {
                            joined.push('\n');
                        }
                        joined
                    };
                    sink.append(OutputStream::Stderr, text.as_bytes());
                    dirty = true;
                    exec_status = "error".to_owned();
                    if error_summary.is_none() {
                        error_summary = Some(if evalue.is_empty() {
                            ename
                        } else {
                            format!("{ename}: {evalue}")
                        });
                    }
                }
                Ok(KernelFrame::Error { .. }) => {}
                Ok(KernelFrame::Done {
                    id,
                    status,
                    execution_count: count,
                    cancelled: kernel_cancelled,
                }) if id == rid => {
                    exec_status = status;
                    execution_count = count;
                    if kernel_cancelled && !timed_out {
                        cancelled = true;
                    }
                    break;
                }
                Ok(KernelFrame::Done { .. }) => {}
                Ok(KernelFrame::Eof) | Err(RecvTimeoutError::Disconnected) => {
                    let mut stderr_note = String::new();
                    if let Some(dead) = state.running.as_ref() {
                        stderr_note = dead.stderr_summary();
                    }
                    state.shutdown_running()?;
                    state.restarted_notice = true;
                    kernel_died = true;
                    exec_status = "error".to_owned();
                    error_summary = Some(if stderr_note.is_empty() {
                        format!("{lang_label} kernel exited unexpectedly.")
                    } else {
                        format!("{lang_label} kernel exited unexpectedly: {stderr_note}")
                    });
                    break;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        Ok(())
    })();

    if let Some(kernel) = state.running.as_ref() {
        kernel.cell_active.store(false, Ordering::Release);
    }

    // Regardless of whether the in-loop shutdown succeeded, do one idempotent teardown first: seal the capture and publish metadata,
    // and only surface errors (including CleanupError) after teardown completes, without faking cell success.
    sink.finish();
    if dirty {
        emit(AgentEvent::ToolOutputSnapshot {
            id: req.call_id.to_owned(),
            output: sink.preview(),
        });
    }
    loop_result?;

    let mut result = sink.value();
    let ok = exec_status == "ok" && !timed_out && !cancelled && !kernel_died;
    result["ok"] = json!(ok);
    result["language"] = json!(req.language.as_str());
    if let Some(runtime_name) = runtime_name {
        result["runtime"] = json!(runtime_name);
    }
    result["execution_count"] = json!(execution_count);
    result["timed_out"] = json!(timed_out);
    result["cancelled"] = json!(cancelled);
    result["timeout_seconds"] = json!(req.timeout_seconds);
    result["duration_ms"] = json!(started.elapsed().as_millis() as u64);
    if kernel_died {
        result["kernel_restarted"] = json!(true);
    }
    if let Some(notice) = notice {
        let merged = match result.get("notice").and_then(Value::as_str) {
            Some(existing) if !existing.is_empty() => format!("{notice} {existing}"),
            _ => notice,
        };
        result["notice"] = json!(merged);
    }
    if timed_out {
        result["error"] = json!(error_summary.unwrap_or_else(|| format!(
            "{lang_label} cell timed out after {}s; kernel was interrupted. Pass reset: true if state appears corrupted.",
            req.timeout_seconds
        )));
    } else if cancelled {
        result["error"] = json!(
            error_summary
                .unwrap_or_else(|| format!("{lang_label} cell cancelled; kernel was interrupted."))
        );
    } else if !ok {
        result["error"] =
            json!(error_summary.unwrap_or_else(|| format!("{lang_label} execution failed.")));
    }
    if !ok
        && let Some(policy) = req.sandbox.as_deref()
        && (result["error"]
            .as_str()
            .is_some_and(pp_sandbox::Policy::is_denied_output)
            || result["output"]
                .as_str()
                .is_some_and(pp_sandbox::Policy::is_denied_output))
    {
        let meta = policy.denial_metadata();
        result["sandbox"] = json!({
            "blocked": true,
            "mode": meta.mode,
            "network": meta.network,
            "writable_roots": meta.writable_roots,
            "hint": meta.hint,
        });
    }

    Ok(CellOutcome {
        value: result,
        images,
    })
}
