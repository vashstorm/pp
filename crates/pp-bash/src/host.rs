//! Embedded Bash host executor.
//!
//! This module runs the brush interpreter directly on the caller's thread: it creates no helper shell
//! process, writes no host stdio and mutates no process-level host state. External programs are still
//! supervised by [`super::supervisor::Supervisor`]; this module only owns the logical shell, the
//! temporary directory, logical FDs and output draining.

use super::streams::{PipeRun, RunEnd};
use super::supervisor::Supervisor;
use anyhow::{Context, Result};
use brush_builtins::{BuiltinSet, default_builtins};
use brush_core::{
    ProcessSupervisor, ProfileLoadBehavior, RcLoadBehavior, Shell, ShellFd, ShellVariable,
    SourceInfo,
    extensions::DefaultShellExtensions,
    openfiles::{self, OpenFile},
};
use pp_core::cancellation::CancellationToken;
use pp_core::events::AgentEvent;
use pp_core::events::OutputStream;
use pp_sandbox::Policy;
use pp_workspace::output::OutputSink;
use std::{
    collections::HashMap,
    io::{self, PipeReader, PipeWriter, Read},
    path::Path,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Consistent with the tool card: publish a changed preview at most once every 50ms while running.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(50);
/// Upper bound for waiting until both logical output streams drain after the interpreter and supervisor
/// have finished. Descendants that deliberately left the process group may still hold inherited write
/// ends; report not-drained instead of hanging forever.
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(1);
const OUTPUT_CHUNK: usize = 64 * 1024;

pub(crate) use super::supervisor::Termination;

/// Internal execution entry parameters shared by the foreground and background paths. Paths and command
/// are only borrowed for the duration of this call.
pub(crate) struct HostConfig<'a> {
    pub(crate) cwd: &'a Path,
    pub(crate) cache: &'a Path,
    pub(crate) command: &'a str,
    pub(crate) timeout_seconds: u64,
    pub(crate) call_id: &'a str,
    /// Parent task cancellation: the interpreter and external groups finish as `Cancelled`.
    pub(crate) cancellation: &'a CancellationToken,
    /// The explicit stop handle is owned by the caller (the background TaskManager); the run finishes as
    /// `Stopped`.
    pub(crate) stop: &'a CancellationToken,
    pub(crate) umask: u32,
    pub(crate) pipes: &'a PipeRun,
    pub(crate) sandbox: Option<&'a Arc<Policy>>,
}

/// Real terminal state of the interpreter; `Termination` is decided by the supervisor and cannot be
/// inferred from output or the exit code.
pub(crate) struct HostOutcome {
    pub(crate) exit_code: Option<i32>,
    pub(crate) termination: Termination,
    pub(crate) output_drained: bool,
    pub(crate) sandbox_blocked: bool,
}

struct SinkShared {
    sink: Arc<Mutex<OutputSink>>,
    dirty: AtomicBool,
    /// After the host seals the sink, late readers must discard data instead of writing into the finished
    /// sink.
    closed: AtomicBool,
}

struct Readers {
    handles: [JoinHandle<()>; 2],
    done: Receiver<(OutputStream, io::Result<()>)>,
}

struct DrainResult {
    drained: bool,
    error: Option<anyhow::Error>,
}

impl Readers {
    fn drain(self, grace: Duration) -> DrainResult {
        let started = Instant::now();
        let mut done = [false; 2];
        let mut remaining = 2usize;
        let mut drained = true;
        let mut errors: Vec<anyhow::Error> = Vec::new();
        while remaining > 0 {
            let left = grace.checked_sub(started.elapsed()).unwrap_or_default();
            if left.is_zero() {
                drained = false;
                break;
            }
            match self.done.recv_timeout(left) {
                Ok((stream, result)) => {
                    let index = match stream {
                        OutputStream::Stdout => 0,
                        OutputStream::Stderr => 1,
                    };
                    if done[index] {
                        continue;
                    }
                    done[index] = true;
                    remaining -= 1;
                    if let Err(error) = result {
                        drained = false;
                        errors.push(
                            anyhow::Error::new(error).context("reading embedded shell output"),
                        );
                    }
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    drained = false;
                    break;
                }
            }
        }
        for (index, handle) in self.handles.into_iter().enumerate() {
            if done[index] && handle.join().is_err() {
                drained = false;
                errors.push(anyhow::anyhow!(
                    "embedded shell output reader thread panicked"
                ));
            }
            // Readers that have not finished are still blocked on pipes held by the host; dropping the handle
            // lets them exit on their own, because joining must never block run shutdown forever.
        }
        DrainResult {
            drained,
            error: errors.into_iter().next(),
        }
    }
}

struct RunReport {
    exit_code: Option<i32>,
    error: Option<anyhow::Error>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Execute one embedded shell on the current caller thread until both the interpreter and managed
/// external programs have finished.
pub(crate) fn run_host(
    config: HostConfig<'_>,
    sink: Arc<Mutex<OutputSink>>,
    emit: Option<&mut dyn FnMut(AgentEvent)>,
) -> Result<HostOutcome> {
    let pipes = config.pipes.clone();
    let result = run_host_inner(config, sink, emit);
    if result.is_err() && !pipes.is_finished() {
        pipes.finish(RunEnd::interrupted("host_setup_failed"));
    }
    result
}

fn run_host_inner(
    config: HostConfig<'_>,
    sink: Arc<Mutex<OutputSink>>,
    mut emit: Option<&mut dyn FnMut(AgentEvent)>,
) -> Result<HostOutcome> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("creating embedded shell runtime")?;
    let scratch = tempfile::Builder::new()
        .prefix(".pp-tmp-")
        .tempdir_in(config.cwd)
        .context("creating command temporary directory")?;
    let (stdout_reader, stdout_writer) =
        io::pipe().context("creating embedded shell stdout pipe")?;
    let (stderr_reader, stderr_writer) =
        io::pipe().context("creating embedded shell stderr pipe")?;
    let shared = Arc::new(SinkShared {
        sink,
        dirty: AtomicBool::new(false),
        closed: AtomicBool::new(false),
    });
    // 0 means no deadline; cancellation and explicit stop are still handled by the supervisor.
    let deadline = if config.timeout_seconds == 0 {
        None
    } else {
        Some(
            Instant::now()
                .checked_add(Duration::from_secs(config.timeout_seconds))
                .context("command timeout is too large")?,
        )
    };
    let run_policy = config
        .sandbox
        .map(|policy| policy.with_extra_writable(scratch.path()));
    let supervisor = Supervisor::new_with_stop(
        deadline,
        config.cancellation.clone(),
        config.stop.clone(),
        run_policy,
    )
    .context("starting command supervisor")?;
    let readers = spawn_readers(stdout_reader, stderr_reader, shared.clone())?;

    let report = runtime.block_on(run_interpreter(
        &config,
        &supervisor,
        scratch.path(),
        stdout_writer,
        stderr_writer,
        &shared,
        &mut emit,
    ));
    // After the interpreter finishes, logical background tasks can only be discarded through the runtime;
    // cancel them first, then drain the write ends.
    runtime.shutdown_timeout(Duration::ZERO);

    // First confirm every managed external group has finished, then wait for pipes holding inherited write
    // ends to really reach EOF.
    supervisor.interpreter_finished();
    let supervisor_outcome = supervisor.wait();
    let cleanup_error = supervisor_outcome.cleanup_error;
    let termination = supervisor_outcome.termination;
    let sandbox_blocked = supervisor_outcome.sandbox_blocked;
    if let Err(error) = scratch.close() {
        tracing::warn!(
            event = "embedded_shell_scratch_cleanup_failed",
            error = %error,
            "command finished, but temporary directory cleanup failed"
        );
    }
    let drain = readers.drain(OUTPUT_DRAIN_GRACE);
    let DrainResult {
        drained,
        error: drain_error,
    } = drain;
    {
        let mut output = lock(&shared.sink);
        // Shares the sink lock with the readers: data arriving after sealing is discarded.
        shared.closed.store(true, Ordering::Release);
        output.finish();
    }

    let (exit_code, mut failure) = match report {
        Ok(RunReport { exit_code, error }) => (exit_code, error),
        Err(error) => (None, Some(error)),
    };
    if let Some(drain_error) = drain_error {
        failure = Some(match failure {
            Some(primary) => {
                drain_error.context(format!("embedded shell also failed: {primary:#}"))
            }
            None => drain_error,
        });
    }
    if let Some(cleanup) = cleanup_error {
        failure = Some(match failure {
            Some(error) => anyhow::Error::new(cleanup)
                .context(format!("embedded shell also failed: {error:#}")),
            None => anyhow::Error::new(cleanup),
        });
    }
    let run_end = match termination {
        Termination::Exited => RunEnd::exited(exit_code),
        Termination::TimedOut => RunEnd::interrupted("timed_out"),
        Termination::Cancelled => RunEnd::interrupted("cancelled"),
        Termination::Stopped => RunEnd::interrupted("stopped"),
    };
    config.pipes.finish(run_end);
    if let Some(snapshot) = config.pipes.shell_snapshot()
        && let Some(callback) = emit.as_mut()
    {
        (*callback)(AgentEvent::ToolShellSnapshot {
            id: config.call_id.to_owned(),
            snapshot,
        });
    }
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(HostOutcome {
        exit_code,
        termination,
        output_drained: drained,
        sandbox_blocked,
    })
}

fn spawn_readers(
    stdout: PipeReader,
    stderr: PipeReader,
    shared: Arc<SinkShared>,
) -> Result<Readers> {
    let (done, done_receiver) = channel();
    let stdout_handle = spawn_reader(stdout, OutputStream::Stdout, shared.clone(), done.clone())
        .context("starting embedded shell stdout reader")?;
    let stderr_handle = match spawn_reader(stderr, OutputStream::Stderr, shared, done.clone()) {
        Ok(handle) => handle,
        Err(error) => {
            // The first reader exits naturally once the write end is released when this failure returns.
            drop(stdout_handle);
            return Err(error).context("starting embedded shell stderr reader");
        }
    };
    drop(done);
    Ok(Readers {
        handles: [stdout_handle, stderr_handle],
        done: done_receiver,
    })
}

fn spawn_reader(
    mut reader: PipeReader,
    stream: OutputStream,
    shared: Arc<SinkShared>,
    done: Sender<(OutputStream, io::Result<()>)>,
) -> io::Result<JoinHandle<()>> {
    let name = match stream {
        OutputStream::Stdout => "pp-bash-stdout",
        OutputStream::Stderr => "pp-bash-stderr",
    };
    thread::Builder::new().name(name.into()).spawn(move || {
        let mut buffer = vec![0u8; OUTPUT_CHUNK];
        let result = loop {
            match reader.read(&mut buffer) {
                Ok(0) => break Ok(()),
                Ok(count) => {
                    let mut output = lock(&shared.sink);
                    if shared.closed.load(Ordering::Acquire) {
                        break Ok(());
                    }
                    // Hold the lock only briefly and append block by block; the interpreter thread is not involved.
                    output.append(stream, &buffer[..count]);
                    drop(output);
                    shared.dirty.store(true, Ordering::Release);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => break Err(error),
            }
        };
        let _ = done.send((stream, result));
    })
}

async fn run_interpreter(
    config: &HostConfig<'_>,
    supervisor: &Arc<Supervisor>,
    scratch: &Path,
    stdout_writer: PipeWriter,
    stderr_writer: PipeWriter,
    shared: &Arc<SinkShared>,
    emit: &mut Option<&mut dyn FnMut(AgentEvent)>,
) -> Result<RunReport> {
    let stdin = openfiles::null().context("opening embedded shell stdin")?;
    let fds: HashMap<ShellFd, OpenFile> = HashMap::from([
        (openfiles::OpenFiles::STDIN_FD, stdin),
        (
            openfiles::OpenFiles::STDOUT_FD,
            OpenFile::PipeWriter(stdout_writer),
        ),
        (
            openfiles::OpenFiles::STDERR_FD,
            OpenFile::PipeWriter(stderr_writer),
        ),
    ]);
    let builtins = default_builtins::<DefaultShellExtensions>(BuiltinSet::BashMode);
    let mut shell = Shell::builder()
        .builtins(builtins)
        .profile(ProfileLoadBehavior::Skip)
        .rc(RcLoadBehavior::Skip)
        .command_string_mode(true)
        .working_dir(config.cwd.to_path_buf())
        .fds(fds)
        .build()
        .await
        .context("creating embedded shell")?;
    // Embedded mode must be entered right after the build: BASHPID/PPID are removed and missing logical
    // fd0/1/2 resolve to null instead of ever falling back to pp's own inherited stdio.
    shell.set_embedded_mode(true);
    shell.set_umask(config.umask);
    let dyn_supervisor: Arc<dyn ProcessSupervisor> = supervisor.clone();
    shell.set_process_supervisor(dyn_supervisor);
    // The temporary directory and cache are specific to this run and only written into the logical Shell;
    // the host environment is untouched.
    set_exported_path(&mut shell, "TMPDIR", scratch)?;
    set_exported_path(&mut shell, "TMP", scratch)?;
    set_exported_path(&mut shell, "TEMP", scratch)?;
    set_exported_path(&mut shell, "XDG_CACHE_HOME", config.cache)?;
    if let Some(policy) = config.sandbox {
        set_exported_env(&mut shell, "PP_SANDBOX", policy.backend())?;
    }

    // run_string does not push this frame itself; mirror run_dash_c_command.
    shell.start_command_string_mode();
    let mut params = shell.default_exec_params();
    // The supervisor cancels this token on deadline/cancellation/explicit stop; the core exits
    // cooperatively at its boundaries.
    params.set_cancel_token(supervisor.execution_token());
    params.set_pipeline_observer(config.pipes.observer());

    let run_result = {
        let source = SourceInfo::from("-c");
        // tokio's select!/pin! need the macros feature; this uses time plus std::pin only.
        let mut run = std::pin::pin!(shell.run_string(config.command, &source, &params));
        let mut last_pipe_revision = config.pipes.revision();
        let mut last_shell_revision = 0;
        loop {
            match tokio::time::timeout(PREVIEW_INTERVAL, run.as_mut()).await {
                Ok(outcome) => break outcome,
                Err(_) => {
                    let shell_revision = config.pipes.shell_revision();
                    if shell_revision != last_shell_revision {
                        if let Some(snapshot) = config.pipes.shell_snapshot()
                            && let Some(callback) = emit.as_mut()
                        {
                            (*callback)(AgentEvent::ToolShellSnapshot {
                                id: config.call_id.to_owned(),
                                snapshot,
                            });
                        }
                        last_shell_revision = shell_revision;
                    }
                    let pipe_revision = config.pipes.revision();
                    if pipe_revision != last_pipe_revision {
                        if let Some(callback) = emit.as_mut() {
                            (*callback)(AgentEvent::ToolPipelineSnapshot {
                                id: config.call_id.to_owned(),
                                pipes: config.pipes.pipes_json(),
                                preview: config.pipes.preview(),
                            });
                        }
                        last_pipe_revision = pipe_revision;
                    }
                    let changed = shared.dirty.swap(false, Ordering::AcqRel);
                    if changed && emit.is_some() {
                        // The preview is copied under the lock and emitted outside it; a blocking UI callback does
                        // not block the readers.
                        let preview = lock(&shared.sink).preview();
                        if let Some(callback) = emit.as_mut() {
                            (*callback)(AgentEvent::ToolOutputSnapshot {
                                id: config.call_id.to_owned(),
                                output: preview,
                            });
                        }
                    }
                }
            }
        }
    };

    let exec_replaced = shell.was_exec_replaced();
    let _ = shell.end_command_string_mode();
    let (exit_code, error) = match run_result {
        Ok(result) => (Some(i32::from(u8::from(result.exit_code))), None),
        Err(error) => (None, Some(anyhow::Error::new(error))),
    };
    // A successful exec has already replaced the logical shell with a managed external program, so the old
    // EXIT trap can no longer run. A normal exit (ExitShell) still needs to run. The EXIT handler reuses this
    // round's managed parameters: deadline/cancellation can still stop it at the interpreter boundary, and
    // pipelines inside it still reach the observer as usual.
    if error.is_none() && !exec_replaced {
        let _ = shell.on_exit_with_params(&params).await;
    }
    drop(params);
    // Release the logical output write end so the single reader of this round sees EOF.
    drop(shell);
    Ok(RunReport { exit_code, error })
}

fn set_exported_path(shell: &mut Shell, name: &str, value: &Path) -> Result<()> {
    set_exported_env(shell, name, &value.to_string_lossy())
}

fn set_exported_env(shell: &mut Shell, name: &str, value: &str) -> Result<()> {
    let mut variable = ShellVariable::new(value.to_owned());
    variable.export();
    shell
        .set_env_global(name, variable)
        .with_context(|| format!("setting {name} in the embedded shell"))
}
