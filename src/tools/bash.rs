use super::{StreamText, drain_pipe, emit_output};
use crate::cancellation::{CancellationToken, CleanupError};
use crate::events::{AgentEvent, OutputStream};
use anyhow::{Context, Result};
use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, Read},
    os::{fd::AsFd, unix::process::CommandExt},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(5);
const IDLE_POLL: Duration = Duration::from_millis(50);
const GRACE: Duration = Duration::from_secs(1);
const OUTPUT_LINE_LIMIT: usize = 500;

/// Distinguishes a real exit from the two supervised stop reasons; a single
/// bool cannot keep timeout and cancellation apart when both race.
enum Termination {
    Exited,
    TimedOut,
    Cancelled,
}

fn retain_lines<'a>(bytes: &'a [u8], at_line_start: &mut bool, remaining: &mut usize) -> &'a [u8] {
    for (index, byte) in bytes.iter().enumerate() {
        if *at_line_start {
            if *remaining == 0 {
                return &bytes[..index];
            }
            *remaining -= 1;
        }
        *at_line_start = *byte == b'\n';
    }
    bytes
}

struct ProcessGroup {
    child: Child,
    group: Pid,
    reaped: bool,
}
impl ProcessGroup {
    fn signal(&self, signal: Signal) -> io::Result<()> {
        match kill_process_group(self.group, signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            // macOS 对仅剩僵尸进程的组返回 EPERM，确认直接子进程已退出后视为清理完成。
            Err(rustix::io::Errno::PERM) if cfg!(target_os = "macos") && self.exited()? => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn exited(&self) -> io::Result<bool> {
        loop {
            match waitid(
                WaitId::Pid(self.group),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            ) {
                Ok(status) => return Ok(status.is_some()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn supervise(
        mut self,
        timeout: Duration,
        stop: &AtomicBool,
        readers: &AtomicUsize,
        failed: &AtomicBool,
        cancellation: &CancellationToken,
    ) -> Result<(ExitStatus, Termination)> {
        // 无论监督线程如何退出，都唤醒非阻塞管道读取线程。
        struct Stop<'a>(&'a AtomicBool);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let _stop = Stop(stop);
        let started = Instant::now();
        loop {
            // 清理整个组之前保留直接子进程的等待状态，防止 PID/进程组号被复用。
            let exited = self.exited().context("checking command status")?;
            if exited && readers.load(Ordering::Acquire) == 2 {
                return Ok((self.finish()?, Termination::Exited));
            }
            if failed.load(Ordering::Acquire) {
                return Ok((self.finish()?, Termination::Exited));
            }
            // 同一轮内取消优先于 deadline；超时一旦选定并发出 TERM，
            // 后续 grace 期间的取消不再改判为取消。
            if cancellation.is_cancelled() {
                let status = self
                    .terminate()
                    .map_err(|error| anyhow::Error::new(CleanupError(error)))?;
                return Ok((status, Termination::Cancelled));
            }
            if started.elapsed() >= timeout {
                let status = self
                    .terminate()
                    .context("terminating timed out command process group")?;
                return Ok((status, Termination::TimedOut));
            }
            thread::sleep(POLL);
        }
    }

    fn terminate(&mut self) -> Result<ExitStatus> {
        self.terminate_with_grace(|| thread::sleep(GRACE))
    }

    fn terminate_with_grace(&mut self, grace: impl FnOnce()) -> Result<ExitStatus> {
        self.signal(Signal::TERM)
            .context("terminating command process group")?;
        grace();
        self.finish()
    }

    fn finish(&mut self) -> Result<ExitStatus> {
        self.signal(Signal::KILL)
            .context("cleaning up command process group")?;
        let status = self.child.wait().context("reaping command")?;
        self.reaped = true;
        Ok(status)
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // 正常结束也清理留在同一进程组的后台任务；异常路径负责回收直接子进程。
        if !self.reaped {
            let _ = self.signal(Signal::KILL);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn nonblocking(pipe: &impl AsFd) -> Result<()> {
    fcntl_setfl(pipe, fcntl_getfl(pipe)? | OFlags::NONBLOCK)
        .context("configuring nonblocking command pipe")
}

fn reader(
    pipe: impl Read,
    stream: OutputStream,
    sender: SyncSender<(OutputStream, Vec<u8>)>,
    stop: &AtomicBool,
    done: &AtomicUsize,
    failed: &AtomicBool,
) -> io::Result<bool> {
    struct Done<'a>(&'a AtomicUsize);
    impl Drop for Done<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Release);
        }
    }
    let _done = Done(done);
    let result = drain_pipe(pipe, stream, sender, stop);
    if result.is_err() {
        failed.store(true, Ordering::Release);
    }
    result
}

pub(super) fn run(
    cwd: &Path,
    cache: &Path,
    command: &str,
    timeout_seconds: u64,
    call_id: &str,
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<Value> {
    let mut process = Command::new("bash");
    process.arg("-c").arg(command);
    run_command(
        cwd,
        cache,
        process,
        timeout_seconds,
        call_id,
        cancellation,
        emit,
    )
    .context("running bash")
}

struct SpawnedCommand {
    process: ProcessGroup,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    scratch: tempfile::TempDir,
}

fn spawn_command(cwd: &Path, cache: &Path, mut command: Command) -> Result<SpawnedCommand> {
    let scratch = tempfile::Builder::new()
        .prefix(".pp-tmp-")
        .tempdir_in(cwd)
        .context("creating command temporary directory")?;
    let child = command
        .current_dir(cwd)
        .env("TMPDIR", scratch.path())
        .env("TMP", scratch.path())
        .env("TEMP", scratch.path())
        .env("XDG_CACHE_HOME", cache)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .context("starting command")?;
    let group = Pid::from_raw(child.id() as i32).expect("a spawned child has a positive pid");
    let mut process = ProcessGroup {
        child,
        group,
        reaped: false,
    };
    let stdout = process
        .child
        .stdout
        .take()
        .context("opening command stdout")?;
    let stderr = process
        .child
        .stderr
        .take()
        .context("opening command stderr")?;
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    Ok(SpawnedCommand {
        process,
        stdout,
        stderr,
        scratch,
    })
}

pub(super) fn run_command(
    cwd: &Path,
    cache: &Path,
    command: Command,
    timeout_seconds: u64,
    call_id: &str,
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<Value> {
    let SpawnedCommand {
        process,
        stdout,
        stderr,
        scratch,
    } = spawn_command(cwd, cache, command)?;
    let stop = AtomicBool::new(false);
    let done = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let mut stdout_text = StreamText::default();
    let mut stderr_text = StreamText::default();
    let mut remaining_lines = OUTPUT_LINE_LIMIT;
    let mut stdout_line_start = true;
    let mut stderr_line_start = true;
    let mut stdout_line_truncated = false;
    let mut stderr_line_truncated = false;
    let (status, termination, stdout_truncated, stderr_truncated) =
        thread::scope(|scope| -> Result<_> {
            let (sender, receiver) = sync_channel(16);
            let err_sender = sender.clone();
            let out =
                scope.spawn(|| reader(stdout, OutputStream::Stdout, sender, &stop, &done, &failed));
            let err = scope.spawn(|| {
                reader(
                    stderr,
                    OutputStream::Stderr,
                    err_sender,
                    &stop,
                    &done,
                    &failed,
                )
            });
            let monitor = scope.spawn(|| {
                process.supervise(
                    Duration::from_secs(timeout_seconds),
                    &stop,
                    &done,
                    &failed,
                    cancellation,
                )
            });
            for (stream, bytes) in receiver {
                let (at_line_start, truncated) = match stream {
                    OutputStream::Stdout => (&mut stdout_line_start, &mut stdout_line_truncated),
                    OutputStream::Stderr => (&mut stderr_line_start, &mut stderr_line_truncated),
                };
                let retained = retain_lines(&bytes, at_line_start, &mut remaining_lines);
                *truncated |= retained.len() < bytes.len();
                let text = match stream {
                    OutputStream::Stdout => stdout_text.append(retained),
                    OutputStream::Stderr => stderr_text.append(retained),
                };
                emit_output(call_id, stream, text, emit);
            }
            emit_output(call_id, OutputStream::Stdout, stdout_text.finish(), emit);
            emit_output(call_id, OutputStream::Stderr, stderr_text.finish(), emit);
            let (status, termination) = monitor
                .join()
                .map_err(|_| anyhow::anyhow!("command supervisor thread failed"))??;
            let out = out
                .join()
                .map_err(|_| anyhow::anyhow!("command stdout reader thread failed"))??;
            let err = err
                .join()
                .map_err(|_| anyhow::anyhow!("command stderr reader thread failed"))??;
            Ok((
                status,
                termination,
                out || stdout_line_truncated,
                err || stderr_line_truncated,
            ))
        })?;
    let (ok, timed_out, cancelled) = match termination {
        Termination::Exited => (status.success(), false, false),
        Termination::TimedOut => (false, true, false),
        Termination::Cancelled => (false, false, true),
    };
    let mut result = json!({
        "ok":ok, "exit_code":status.code(), "timed_out":timed_out, "cancelled":cancelled,
        "stdout":stdout_text.text, "stderr":stderr_text.text,
        "stdout_truncated":stdout_truncated, "stderr_truncated":stderr_truncated,
    });
    if cancelled {
        result["error"] = json!("Command cancelled; side effects were not rolled back.");
    }
    if let Err(error) = scratch.close() {
        result["warning"] = json!(format!(
            "command finished, but temporary directory cleanup failed: {error}"
        ));
    }
    Ok(result)
}

#[derive(Default)]
pub(super) struct TaskManager {
    registry: Arc<Mutex<TaskRegistry>>,
}

#[derive(Default)]
struct TaskRegistry {
    next_id: u64,
    closed: bool,
    tasks: BTreeMap<String, Arc<BackgroundTask>>,
}

pub(crate) struct BackgroundCleanup {
    registry: Arc<Mutex<TaskRegistry>>,
}

// A poisoned metadata lock must not prevent process reclamation.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn cleanup_error(message: impl std::fmt::Display) -> anyhow::Error {
    CleanupError(anyhow::anyhow!("{message}")).into()
}

struct BackgroundTask {
    cancellation: CancellationToken,
    stop: CancellationToken,
    state: Mutex<BackgroundState>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

#[derive(Clone)]
struct BackgroundState {
    status: &'static str,
    exit_code: Option<i32>,
    stdout: BackgroundOutput,
    stderr: BackgroundOutput,
    remaining_lines: usize,
    error: Option<String>,
    warning: Option<String>,
    cleanup_error: Option<String>,
}

#[derive(Clone)]
struct BackgroundOutput {
    bytes: Vec<u8>,
    remaining_bytes: usize,
    at_line_start: bool,
    truncated: bool,
}

impl Default for BackgroundOutput {
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            remaining_bytes: super::OUTPUT_LIMIT,
            at_line_start: true,
            truncated: false,
        }
    }
}

impl BackgroundOutput {
    fn append(&mut self, bytes: &[u8], remaining_lines: &mut usize) {
        let keep = bytes.len().min(self.remaining_bytes);
        self.remaining_bytes -= keep;
        let retained = retain_lines(&bytes[..keep], &mut self.at_line_start, remaining_lines);
        self.truncated |= keep < bytes.len() || retained.len() < keep;
        self.bytes.extend_from_slice(retained);
    }

    fn text(&self, terminal: bool) -> String {
        if terminal {
            String::from_utf8_lossy(&self.bytes).into_owned()
        } else {
            let mut text = String::with_capacity(self.bytes.len());
            super::decode_prefix(&self.bytes, &mut text);
            text
        }
    }
}

impl BackgroundTask {
    fn snapshot(&self, id: &str) -> Result<Value> {
        // Clone under the lock, then decode outside it so a large cumulative
        // log does not block the worker draining the pipes.
        let state = lock(&self.state).clone();
        if let Some(error) = &state.cleanup_error {
            return Err(cleanup_error(error));
        }
        let terminal = state.status != "running";
        let mut value = json!({
            "ok": matches!(state.status, "running" | "exited" | "stopped"),
            "task_id": id, "status": state.status, "exit_code": state.exit_code,
            "stdout": state.stdout.text(terminal), "stderr": state.stderr.text(terminal),
            "stdout_truncated": state.stdout.truncated, "stderr_truncated": state.stderr.truncated,
        });
        if let Some(error) = &state.error {
            value["error"] = json!(error);
        }
        if let Some(warning) = &state.warning {
            value["warning"] = json!(warning);
        }
        Ok(value)
    }

    fn join(&self) -> Result<()> {
        // Keep this lock until join completes: concurrent cleanup must also wait.
        let mut worker = lock(&self.worker);
        if let Some(handle) = worker.take()
            && handle.join().is_err()
        {
            lock(&self.state).cleanup_error = Some("background worker thread panicked".into());
        }
        if let Some(error) = &lock(&self.state).cleanup_error {
            return Err(cleanup_error(error));
        }
        Ok(())
    }
}

impl TaskManager {
    pub(super) fn start(
        &self,
        cwd: &Path,
        cache: &Path,
        command: &str,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        let mut registry = lock(&self.registry);
        anyhow::ensure!(!registry.closed, "Background task manager is closed.");
        crate::cancellation::check(cancellation)?;
        let next_id = registry
            .next_id
            .checked_add(1)
            .context("background task ID exhausted")?;
        let id = format!("bg-{next_id}");
        let mut command_process = Command::new("bash");
        command_process.arg("-c").arg(command);
        let spawned = spawn_command(cwd, cache, command_process)?;
        let task = Arc::new(BackgroundTask {
            cancellation: cancellation.clone(),
            stop: CancellationToken::new(),
            state: Mutex::new(BackgroundState {
                status: "running",
                exit_code: None,
                stdout: Default::default(),
                stderr: Default::default(),
                remaining_lines: OUTPUT_LINE_LIMIT,
                error: None,
                warning: None,
                cleanup_error: None,
            }),
            worker: Mutex::new(None),
        });
        let worker_task = task.clone();
        let worker = thread::Builder::new()
            .name(format!("pp-{id}"))
            .spawn(move || background_worker(spawned, &worker_task))
            .context("starting background supervisor")?;
        *lock(&task.worker) = Some(worker);
        registry.next_id = next_id;
        registry.tasks.insert(id.clone(), task.clone());
        drop(registry);
        task.snapshot(&id)
    }

    fn task(&self, id: &str) -> Result<Arc<BackgroundTask>> {
        lock(&self.registry)
            .tasks
            .get(id)
            .cloned()
            .with_context(|| format!("Unknown background task: {id}"))
    }

    pub(super) fn status(&self, id: &str) -> Result<Value> {
        self.task(id)?.snapshot(id)
    }

    pub(super) fn stop(&self, id: &str) -> Result<Value> {
        let task = self.task(id)?;
        task.stop.cancel();
        task.join()?;
        task.snapshot(id)
    }

    pub(super) fn stop_all(&self) -> Result<()> {
        stop_registry(&self.registry, false)
    }

    pub(super) fn cleanup(&self) -> BackgroundCleanup {
        BackgroundCleanup {
            registry: self.registry.clone(),
        }
    }
}

impl Drop for TaskManager {
    fn drop(&mut self) {
        let _ = stop_registry(&self.registry, true);
    }
}

impl BackgroundCleanup {
    pub(crate) fn shutdown(&self) -> Result<()> {
        stop_registry(&self.registry, true)
    }
}

impl Drop for BackgroundCleanup {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn stop_registry(registry: &Mutex<TaskRegistry>, close: bool) -> Result<()> {
    let tasks = {
        let mut registry = lock(registry);
        registry.closed |= close;
        registry
            .tasks
            .iter()
            .map(|(id, task)| (id.clone(), task.clone()))
            .collect::<Vec<_>>()
    };
    for (_, task) in &tasks {
        task.stop.cancel();
    }
    let mut errors = Vec::new();
    for (id, task) in &tasks {
        if let Err(error) = task.join() {
            errors.push(format!("{id}: {error:#}"));
        }
    }
    let mut registry = lock(registry);
    for (id, _) in tasks {
        registry.tasks.remove(&id);
    }
    if !errors.is_empty() {
        return Err(cleanup_error(errors.join("; ")));
    }
    Ok(())
}

fn background_read(
    pipe: &mut (impl Read + ?Sized),
    task: &BackgroundTask,
    stdout: bool,
    max: usize,
) -> Result<usize> {
    let mut buffer = [0; super::PIPE_CHUNK_SIZE];
    let count = loop {
        match pipe.read(&mut buffer[..max.min(super::PIPE_CHUNK_SIZE)]) {
            Ok(count) => break count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(0),
            Err(error) => return Err(error).context("reading background command output"),
        }
    };
    let mut state = lock(&task.state);
    let BackgroundState {
        stdout: out,
        stderr: err,
        remaining_lines,
        ..
    } = &mut *state;
    if stdout { out } else { err }.append(&buffer[..count], remaining_lines);
    Ok(count)
}

fn background_worker(spawned: SpawnedCommand, task: &BackgroundTask) {
    let SpawnedCommand {
        mut process,
        mut stdout,
        mut stderr,
        scratch,
    } = spawned;
    let mut reason = "failed";
    let supervision = (|| -> Result<()> {
        loop {
            if process
                .exited()
                .context("checking background command status")?
            {
                reason = "exited";
                break;
            }
            if task.cancellation.is_cancelled() {
                reason = "cancelled";
                break;
            }
            if task.stop.is_cancelled() {
                reason = "stopped";
                break;
            }
            let out = background_read(&mut stdout, task, true, super::PIPE_CHUNK_SIZE)?;
            let err = background_read(&mut stderr, task, false, super::PIPE_CHUNK_SIZE)?;
            if out + err == 0 {
                thread::sleep(IDLE_POLL);
            }
        }
        Ok(())
    })();
    let mut output_error = None;
    let finished = if matches!(reason, "cancelled" | "stopped") {
        process.terminate_with_grace(|| {
            let started = Instant::now();
            // worker 是两条管道的唯一读取者，TERM 宽限期内也必须继续排空。
            // 每轮限制读取量并检查时限，持续输出不能推迟 KILL。
            while started.elapsed() < GRACE {
                let output = background_read(&mut stdout, task, true, super::PIPE_CHUNK_SIZE)
                    .and_then(|out| {
                        background_read(&mut stderr, task, false, super::PIPE_CHUNK_SIZE)
                            .map(|err| out + err)
                    });
                match output {
                    Ok(0) => thread::sleep(POLL),
                    Ok(_) => {}
                    Err(error) => {
                        output_error = Some(error);
                        break;
                    }
                }
            }
        })
    } else {
        process.finish()
    };
    // RAII also attempts reclamation on signal/wait errors, before publishing status.
    drop(process);
    for (pipe, is_stdout) in [
        (&mut stdout as &mut dyn Read, true),
        (&mut stderr as &mut dyn Read, false),
    ] {
        let mut remaining = super::OUTPUT_LIMIT + 1;
        while remaining > 0 {
            match background_read(pipe, task, is_stdout, remaining) {
                Ok(0) => break,
                Ok(count) => remaining -= count,
                Err(error) => {
                    output_error = Some(error);
                    break;
                }
            }
        }
    }
    let warning = scratch
        .close()
        .err()
        .map(|error| format!("command finished, but temporary directory cleanup failed: {error}"));
    let mut state = lock(&task.state);
    state.warning = warning;
    match finished {
        Ok(status) => {
            state.exit_code = status.code();
            state.status = if reason == "exited" && !status.success() {
                "failed"
            } else {
                reason
            };
            if state.status == "failed" {
                state.error = Some(format!("Background command exited with {status}."));
            }
        }
        Err(error) => {
            state.status = "failed";
            state.cleanup_error = Some(format!("{error:#}"));
        }
    }
    if reason == "cancelled" {
        state.error = Some("Background task cancelled; side effects were not rolled back.".into());
    }
    if let Some(error) = supervision.err().or(output_error) {
        state.status = "failed";
        state.error = Some(format!("{error:#}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_termination_drains_term_handler_output() {
        for method in ["stop", "cancel", "stop_all", "shutdown", "drop"] {
            let dir = tempfile::tempdir().unwrap();
            let manager = TaskManager::default();
            let cancellation = CancellationToken::new();
            // 两条管道都写入超过容量和保存限额的数据，完成标记必须在写入后生成。
            let command = r#"
                cleanup() {
                    for ((i=0; i<64; i++)); do
                        printf '%4096s' ''
                        printf '%4096s' '' >&2
                    done
                    printf done > cleanup.done
                    exit 0
                }
                trap cleanup TERM
                printf '%s' "$TMPDIR" > ready
                while :; do sleep 60 & wait $!; done
            "#;
            let started = manager
                .start(dir.path(), dir.path(), command, &cancellation)
                .unwrap();
            let id = started["task_id"].as_str().unwrap();
            let task = manager.task(id).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let scratch = loop {
                if let Ok(text) = std::fs::read_to_string(dir.path().join("ready"))
                    && !text.is_empty()
                {
                    break text;
                }
                assert!(Instant::now() < deadline, "后台任务未就绪：{method}");
                thread::sleep(POLL);
            };
            match method {
                "stop" => {
                    manager.stop(id).unwrap();
                }
                "cancel" => {
                    cancellation.cancel();
                    task.join().unwrap();
                }
                "stop_all" => manager.stop_all().unwrap(),
                "shutdown" => manager.cleanup().shutdown().unwrap(),
                _ => drop(manager),
            }
            assert_eq!(
                std::fs::read_to_string(dir.path().join("cleanup.done")).unwrap(),
                "done",
                "TERM 清理未完成：{method}"
            );
            let snapshot = task.snapshot(id).unwrap();
            assert_eq!(snapshot["exit_code"], 0, "{method}: {snapshot}");
            assert_eq!(
                snapshot["status"],
                if method == "cancel" {
                    "cancelled"
                } else {
                    "stopped"
                }
            );
            for stream in ["stdout", "stderr"] {
                assert_eq!(
                    snapshot[stream],
                    " ".repeat(super::super::OUTPUT_LIMIT),
                    "{method}: {stream}"
                );
                assert_eq!(
                    snapshot[format!("{stream}_truncated")],
                    true,
                    "{method}: {stream}"
                );
            }
            assert!(!Path::new(&scratch).exists(), "临时目录未清理：{method}");
        }
    }

    #[test]
    fn background_shutdown_closes_registry_and_joins_all() {
        fn started_group(
            manager: &TaskManager,
            cwd: &Path,
            name: &str,
        ) -> (String, Vec<u32>, String) {
            let snapshot = manager.start(cwd, cwd,
                &format!("trap '' TERM; sleep 60 & child=$!; printf '%s\\n%s\\n%s\\n' \"$$\" \"$child\" \"$TMPDIR\" > {name}; wait \"$child\""),
                &CancellationToken::new()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let text = loop {
                if let Ok(text) = std::fs::read_to_string(cwd.join(name))
                    && text.lines().count() == 3
                {
                    break text;
                }
                assert!(
                    Instant::now() < deadline,
                    "missing background process barrier"
                );
                thread::sleep(POLL);
            };
            let lines = text.lines().collect::<Vec<_>>();
            (
                snapshot["task_id"].as_str().unwrap().to_owned(),
                vec![lines[0].parse().unwrap(), lines[1].parse().unwrap()],
                lines[2].to_owned(),
            )
        }
        fn reclaimed(pids: &[u32], scratch: &str) {
            for &pid in pids {
                let deadline = Instant::now() + Duration::from_secs(5);
                while rustix::process::test_kill_process(Pid::from_raw(pid as i32).unwrap()).is_ok()
                {
                    assert!(
                        Instant::now() < deadline,
                        "background PID {pid} survived cleanup"
                    );
                    thread::sleep(POLL);
                }
            }
            assert!(!Path::new(scratch).exists());
        }
        for method in ["stop_all", "shutdown", "drop"] {
            let dir = tempfile::tempdir().unwrap();
            let manager = TaskManager::default();
            let groups = [
                started_group(&manager, dir.path(), "one"),
                started_group(&manager, dir.path(), "two"),
            ];
            match method {
                "stop_all" => {
                    let barrier = std::sync::Barrier::new(2);
                    thread::scope(|scope| {
                        for _ in 0..2 {
                            let manager = &manager;
                            let groups = &groups;
                            let barrier = &barrier;
                            scope.spawn(move || {
                                barrier.wait();
                                manager.stop_all().unwrap();
                                // Each concurrent caller must wait, not just the join owner.
                                for (_, pids, scratch) in groups {
                                    reclaimed(pids, scratch);
                                }
                            });
                        }
                    });
                    let next = manager
                        .start(dir.path(), dir.path(), "exit 0", &CancellationToken::new())
                        .unwrap();
                    assert!(groups.iter().all(|(id, _, _)| next["task_id"] != *id));
                    manager.stop_all().unwrap();
                }
                "shutdown" => {
                    manager.cleanup().shutdown().unwrap();
                    assert_eq!(
                        manager
                            .start(
                                dir.path(),
                                dir.path(),
                                "touch leaked",
                                &CancellationToken::new()
                            )
                            .unwrap_err()
                            .to_string(),
                        "Background task manager is closed."
                    );
                }
                _ => drop(manager),
            }
            for (_, pids, scratch) in groups {
                reclaimed(&pids, &scratch);
            }
            assert!(!dir.path().join("leaked").exists());
        }
        // Start and permanent shutdown share one registry lock. Every successful
        // start is in the shutdown snapshot, or it sees closed before spawning.
        for _ in 0..12 {
            let dir = tempfile::tempdir().unwrap();
            let manager = TaskManager::default();
            let barrier = std::sync::Barrier::new(2);
            thread::scope(|scope| {
                let started = scope.spawn(|| {
                    barrier.wait();
                    manager.start(
                        dir.path(),
                        dir.path(),
                        "printf '%s' $$ > race.pending; mv race.pending race.pid; exec sleep 60",
                        &CancellationToken::new(),
                    )
                });
                barrier.wait();
                manager.cleanup().shutdown().unwrap();
                if let Err(error) = started.join().unwrap() {
                    assert_eq!(error.to_string(), "Background task manager is closed.");
                }
            });
            if let Ok(pid) = std::fs::read_to_string(dir.path().join("race.pid")) {
                reclaimed(&[pid.parse().unwrap()], "");
            }
            assert!(!std::fs::read_dir(dir.path()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".pp-tmp-")
            }));
        }
    }

    #[test]
    fn line_limit_handles_chunk_boundaries_and_unterminated_lines() {
        for line in ["中😀\n", "\r\n", "\n"] {
            for last_newline in [false, true] {
                let mut exact = line.repeat(OUTPUT_LINE_LIMIT);
                if !last_newline {
                    exact.pop();
                }
                for excess in ["", "extra", "extra\n"] {
                    let input = format!("{exact}{excess}");
                    let expected = if last_newline { &exact } else { &input };
                    for chunk_size in [1, 7, 4096] {
                        let mut remaining = OUTPUT_LINE_LIMIT;
                        let mut at_line_start = true;
                        let mut output = Vec::new();
                        let mut truncated = false;
                        for bytes in input.as_bytes().chunks(chunk_size) {
                            let retained = retain_lines(bytes, &mut at_line_start, &mut remaining);
                            truncated |= retained.len() < bytes.len();
                            output.extend_from_slice(retained);
                        }
                        assert_eq!(output, expected.as_bytes());
                        assert_eq!(truncated, last_newline && !excess.is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn shared_line_budget_keeps_an_already_started_line() {
        let mut remaining = 1;
        let mut stdout_line_start = true;
        let mut stderr_line_start = true;
        assert_eq!(
            retain_lines(b"out", &mut stdout_line_start, &mut remaining),
            b"out"
        );
        assert!(retain_lines(b"err\n", &mut stderr_line_start, &mut remaining).is_empty());
        assert_eq!(
            retain_lines(b"-end\nextra", &mut stdout_line_start, &mut remaining),
            b"-end\n"
        );
        assert!(retain_lines(b"extra", &mut stdout_line_start, &mut remaining).is_empty());
    }
}
