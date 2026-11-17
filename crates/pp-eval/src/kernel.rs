//! 持久 Python 内核子进程、进程组监督与 NDJSON 双工帧循环。

use crate::runtime::{PythonRuntime, probe_python};
use anyhow::{Context, Result, bail};
use base64::Engine;
use pp_core::cancellation::{CancellationToken, CleanupError};
use pp_core::events::{AgentEvent, OutputStream};
use pp_workspace::output::OutputSink;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const RUNNER_PY: &str = include_str!("runner.py");
const PREVIEW_INTERVAL: Duration = Duration::from_millis(50);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const INTERRUPT_GRACE: Duration = Duration::from_secs(2);
const SHUTDOWN_GRACE: Duration = Duration::from_millis(800);
const STDERR_TAIL_CAP: usize = 8 * 1024;

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
    child: Child,
    pgid: libc::pid_t,
    stdin: Option<ChildStdin>,
    frames: Receiver<KernelFrame>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    reader_handle: Option<JoinHandle<()>>,
    stderr_handle: Option<JoinHandle<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedImage {
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

pub struct CellRequest<'a> {
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

fn ensure_runner_script(cache_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(cache_dir)
        .with_context(|| format!("creating eval cache dir {}", cache_dir.display()))?;
    let hash = xxhash_rust::xxh32::xxh32(RUNNER_PY.as_bytes(), 0);
    let target = cache_dir.join(format!("pp-eval-runner-{hash:08x}.py"));
    let needs_write = match fs::read(&target) {
        Ok(existing) => existing != RUNNER_PY.as_bytes(),
        Err(_) => true,
    };
    if needs_write {
        let tmp = cache_dir.join(format!(
            "pp-eval-runner-{hash:08x}.{}.tmp",
            std::process::id()
        ));
        fs::write(&tmp, RUNNER_PY.as_bytes())
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

impl RunningKernel {
    fn spawn(
        runtime: &PythonRuntime,
        cwd: &Path,
        cache_dir: &Path,
        sandbox: Option<&pp_sandbox::Policy>,
    ) -> Result<Self> {
        let script = ensure_runner_script(cache_dir)?;
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
            // 子进程运行在独立进程组，确保超时/强杀时连同派生的外部子进程一并回收。
            .process_group(0);
        if sandbox.is_some() {
            cmd.env("PP_SANDBOX", "seatbelt");
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning Python kernel {}", runtime.python_path.display()))?;
        let pgid = child.id() as libc::pid_t;
        let stdin = child.stdin.take().context("missing Python kernel stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("missing Python kernel stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("missing Python kernel stderr")?;

        let (tx, rx) = mpsc::sync_channel::<KernelFrame>(512);
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
            })
            .context("spawning Python stdout reader")?;

        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf = stderr_tail.clone();
        let stderr_handle = thread::Builder::new()
            .name("pp-eval-stderr".to_owned())
            .spawn(move || {
                let mut reader = stderr;
                let mut buf = [0u8; 2048];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut tail) = stderr_buf.lock() {
                                tail.extend_from_slice(&buf[..n]);
                                if tail.len() > STDERR_TAIL_CAP {
                                    let drop_len = tail.len() - STDERR_TAIL_CAP;
                                    tail.drain(..drop_len);
                                }
                            }
                        }
                    }
                }
            })
            .context("spawning Python stderr reader")?;

        Ok(Self {
            child,
            pgid,
            stdin: Some(stdin),
            frames: rx,
            stderr_tail,
            reader_handle: Some(reader_handle),
            stderr_handle: Some(stderr_handle),
        })
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn send_line(&mut self, value: &Value) -> Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            bail!("Python kernel control stdin is closed");
        };
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        stdin.write_all(line.as_bytes())?;
        stdin.flush()?;
        Ok(())
    }

    fn send_signal_group(&mut self, sig: libc::c_int) {
        if matches!(self.child.try_wait(), Ok(None)) && self.pgid > 0 {
            // 仅在直接子进程尚未被 wait 回收前向其进程组发信号，避免 PID 复用误杀。
            unsafe {
                libc::killpg(self.pgid, sig);
            }
        }
    }

    fn send_sigint_leader(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
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
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    thread::sleep(Duration::from_millis(15));
                }
                Err(_) => return false,
            }
        }
    }

    fn stderr_summary(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|buf| String::from_utf8_lossy(&buf).trim().to_owned())
            .unwrap_or_default()
    }

    pub(crate) fn shutdown(mut self) -> Result<()> {
        let _ = self.send_line(&json!({ "type": "exit" }));
        self.stdin.take();
        if !self.wait_exit(SHUTDOWN_GRACE) {
            self.send_signal_group(libc::SIGTERM);
            if !self.wait_exit(SHUTDOWN_GRACE) {
                self.send_signal_group(libc::SIGKILL);
                if !self.wait_exit(SHUTDOWN_GRACE) {
                    return Err(CleanupError(anyhow::anyhow!(
                        "Python kernel process {} did not exit after SIGKILL",
                        self.child.id()
                    ))
                    .into());
                }
            }
        }
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_handle.take() {
            let _ = handle.join();
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct KernelState {
    pub(crate) running: Option<RunningKernel>,
    pub(crate) next_req_id: u64,
    pub(crate) restarted_notice: bool,
    pub(crate) closed: bool,
    pub(crate) sandbox_fingerprint: Option<(pp_sandbox::EffectiveMode, bool)>,
}

pub(crate) fn run_cell(
    state: &mut KernelState,
    req: CellRequest<'_>,
    cancellation: &CancellationToken,
    mut sink: OutputSink,
    emit: &mut dyn FnMut(AgentEvent),
    bridge: &mut dyn FnMut(&str, &Value) -> Result<Value>,
) -> Result<CellOutcome> {
    anyhow::ensure!(!state.closed, "Python eval session is closed.");
    pp_core::cancellation::check(cancellation)?;

    let started = Instant::now();
    let desired_fp = req.sandbox.as_deref().map(|p| (p.mode(), p.network()));
    let mut notice: Option<String> = None;

    if req.reset {
        if let Some(kernel) = state.running.take() {
            kernel.shutdown()?;
        }
        state.restarted_notice = false;
    } else if state.running.is_some() && state.sandbox_fingerprint != desired_fp {
        if let Some(kernel) = state.running.take() {
            kernel.shutdown()?;
        }
        state.restarted_notice = false;
        notice = Some(
            "Kernel restarted: sandbox mode changed; earlier state is unavailable.".to_owned(),
        );
    } else if let Some(kernel) = state.running.as_mut()
        && !kernel.is_alive()
        && let Some(dead) = state.running.take()
    {
        let _ = dead.shutdown();
        state.restarted_notice = true;
    }

    if state.running.is_none() {
        let runtime = probe_python(req.cwd, req.interpreter)?;
        let kernel =
            RunningKernel::spawn(&runtime, req.cwd, req.cache_dir, req.sandbox.as_deref())?;
        state.running = Some(kernel);
        state.sandbox_fingerprint = desired_fp;
        if state.restarted_notice {
            notice = Some(
                "Kernel restarted: the previous Python kernel died; earlier state is unavailable."
                    .to_owned(),
            );
            state.restarted_notice = false;
        }
    }

    state.next_req_id = state.next_req_id.saturating_add(1);
    let rid = format!("eval-{}", state.next_req_id);
    let cwd_str = req.cwd.to_string_lossy().into_owned();

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
        return Err(err.context("sending cell to Python kernel"));
    }

    let mut images = Vec::new();
    let mut dirty = false;
    let mut last_preview = Instant::now();
    let mut active_budget = Duration::from_secs(req.timeout_seconds);
    let mut active_elapsed = Duration::ZERO;
    let mut last_tick = Instant::now();
    let mut interrupt_sent: Option<Instant> = None;
    let mut timed_out = false;
    let mut cancelled = false;
    let mut exec_status = "ok".to_owned();
    let mut execution_count = 0u64;
    let mut kernel_died = false;
    let mut error_summary: Option<String> = None;

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
            // Python 主线程卡在 C 扩展或阻塞子进程无法响应 SIGINT：升级销毁整个进程组。
            if let Some(kernel) = state.running.take() {
                kernel.shutdown()?;
            }
            state.restarted_notice = true;
            kernel_died = true;
            exec_status = "error".to_owned();
            if error_summary.is_none() {
                error_summary = Some(if timed_out {
                    format!(
                        "Python cell timed out after {}s and did not respond to SIGINT; kernel was terminated.",
                        req.timeout_seconds
                    )
                } else {
                    "Python cell cancelled and did not respond to SIGINT; kernel was terminated."
                        .to_owned()
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
            kernel.frames.recv_timeout(POLL_INTERVAL)
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
            Ok(KernelFrame::Stdout { .. } | KernelFrame::Stderr { .. }) => {}
            Ok(KernelFrame::Image {
                id,
                mime_type,
                data,
            }) if id == rid => {
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data.trim()) {
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
                // 嵌套宿主工具调用期间暂停 Python 计算超时窗口，返回后再顺延。
                let bridge_start = Instant::now();
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
                    Err(err) => json!({
                        "type": "tool_result",
                        "call_id": bridge_id,
                        "error": format!("{err:#}"),
                    }),
                };
                let bridge_elapsed = bridge_start.elapsed();
                active_budget = active_budget.saturating_add(bridge_elapsed);
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
                    error_summary =
                        Some(format!("Python kernel exited during tool bridge: {err:#}"));
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
                cancelled: py_cancelled,
            }) if id == rid => {
                exec_status = status;
                execution_count = count;
                if py_cancelled {
                    cancelled = true;
                }
                break;
            }
            Ok(KernelFrame::Done { .. }) => {}
            Ok(KernelFrame::Eof) | Err(RecvTimeoutError::Disconnected) => {
                let stderr_note = state
                    .running
                    .as_ref()
                    .map(RunningKernel::stderr_summary)
                    .unwrap_or_default();
                if let Some(dead) = state.running.take() {
                    dead.shutdown()?;
                }
                state.restarted_notice = true;
                kernel_died = true;
                exec_status = "error".to_owned();
                error_summary = Some(if stderr_note.is_empty() {
                    "Python kernel exited unexpectedly.".to_owned()
                } else {
                    format!("Python kernel exited unexpectedly: {stderr_note}")
                });
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }

    sink.finish();
    if dirty {
        emit(AgentEvent::ToolOutputSnapshot {
            id: req.call_id.to_owned(),
            output: sink.preview(),
        });
    }

    let mut result = sink.value();
    let ok = exec_status == "ok" && !timed_out && !cancelled && !kernel_died;
    result["ok"] = json!(ok);
    result["language"] = json!("py");
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
            "Python cell timed out after {}s; kernel was interrupted. Pass reset: true if state appears corrupted.",
            req.timeout_seconds
        )));
    } else if cancelled {
        result["error"] = json!(
            error_summary
                .unwrap_or_else(|| "Python cell cancelled; kernel was interrupted.".to_owned())
        );
    } else if !ok {
        result["error"] =
            json!(error_summary.unwrap_or_else(|| "Python execution failed.".to_owned()));
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
