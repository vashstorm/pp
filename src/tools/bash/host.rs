//! 嵌入式 Bash 宿主执行器。
//!
//! 该模块在调用方线程上直接运行 brush 解释器：不创建辅助 shell 进程，不写宿主
//! stdio，也不修改宿主进程级状态。外部程序仍由 [`super::supervisor::Supervisor`]
//! 监督；本模块只负责逻辑 shell、临时目录、逻辑 FD 与输出排空。

use super::streams::{PipeRun, RunEnd};
use super::supervisor::Supervisor;
use crate::cancellation::CancellationToken;
use crate::events::{AgentEvent, OutputStream};
use crate::tools::output::OutputSink;
use anyhow::{Context, Result};
use brush_builtins::{BuiltinSet, default_builtins};
use brush_core::{
    ProcessSupervisor, ProfileLoadBehavior, RcLoadBehavior, Shell, ShellFd, ShellVariable,
    SourceInfo,
    extensions::DefaultShellExtensions,
    openfiles::{self, OpenFile},
};
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

/// 与工具卡片一致：运行中最多每 50ms 发布一次有变化的预览。
const PREVIEW_INTERVAL: Duration = Duration::from_millis(50);
/// 解释器与监督器都已结束后，等待两条逻辑输出流排空的上限。
/// 故意脱离组的后代可能仍持有继承的写端；此时报告未排空而不是永久挂起。
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(1);
const OUTPUT_CHUNK: usize = 64 * 1024;

pub(crate) use super::supervisor::Termination;

/// 前台/后台共用的内部执行入口参数。路径与命令只在本次调用期间借用。
pub(crate) struct HostConfig<'a> {
    pub(crate) cwd: &'a Path,
    pub(crate) cache: &'a Path,
    pub(crate) command: &'a str,
    pub(crate) timeout_seconds: u64,
    pub(crate) call_id: &'a str,
    /// 父任务取消：解释器与外部组按 `Cancelled` 收尾。
    pub(crate) cancellation: &'a CancellationToken,
    /// 显式停止句柄由调用方（后台 TaskManager）持有；按 `Stopped` 收尾。
    pub(crate) stop: &'a CancellationToken,
    pub(crate) umask: u32,
    pub(crate) pipes: &'a PipeRun,
}

/// 解释器的真实终态；`Termination` 由监督器判定，不能从输出或退出码反推。
pub(crate) struct HostOutcome {
    pub(crate) exit_code: Option<i32>,
    pub(crate) termination: Termination,
    pub(crate) output_drained: bool,
}

struct SinkShared {
    sink: Arc<Mutex<OutputSink>>,
    dirty: AtomicBool,
    /// 宿主封存 sink 后，迟到的 reader 必须丢弃数据而不是写入已结束的 sink。
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
            if done[index] {
                if handle.join().is_err() {
                    drained = false;
                    errors.push(anyhow::anyhow!(
                        "embedded shell output reader thread panicked"
                    ));
                }
            }
            // 未完成的 reader 仍阻塞在宿主持有的管道上；丢弃句柄让其自行退出，
            // 不能为了 join 而永久阻塞运行收尾。
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

/// 在当前调用方线程上执行一次嵌入式 shell，直到解释器与受管外部程序都收尾。
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
    emit: Option<&mut dyn FnMut(AgentEvent)>,
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
    let now = Instant::now();
    let timeout = Duration::from_secs(config.timeout_seconds);
    // 避免 u64 秒数在 Instant 上溢出的 panic；极大值等同于“不超时”。
    let deadline = now
        .checked_add(timeout)
        .unwrap_or(now + Duration::from_secs(100 * 365 * 24 * 60 * 60));
    let supervisor =
        Supervisor::new_with_stop(deadline, config.cancellation.clone(), config.stop.clone())
            .context("starting command supervisor")?;
    let readers = spawn_readers(stdout_reader, stderr_reader, shared.clone())?;

    let report = runtime.block_on(run_interpreter(
        &config,
        &supervisor,
        scratch.path(),
        stdout_writer,
        stderr_writer,
        &shared,
        emit,
    ));
    // 解释器结束后，逻辑后台任务只能靠 runtime 丢弃；先取消它们再排空写端。
    runtime.shutdown_timeout(Duration::ZERO);

    // 先确认所有受管外部组已收尾，再等待继承写端的管道真正到 EOF。
    supervisor.interpreter_finished();
    let supervisor_outcome = supervisor.wait();
    let cleanup_error = supervisor_outcome.cleanup_error;
    let termination = supervisor_outcome.termination;
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
        // 与 reader 共用 sink 锁：封存后到达的数据会被丢弃。
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
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(HostOutcome {
        exit_code,
        termination,
        output_drained: drained,
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
            // 第一个 reader 在写端随本次失败返回释放后自然退出。
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
                    // 只持有短锁，逐块直接追加；不经解释器线程中转。
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
    mut emit: Option<&mut dyn FnMut(AgentEvent)>,
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
    // 必须在构建后立即进入 embedded 模式：移除 BASHPID/PPID，并让缺失的
    // 逻辑 fd0/1/2 解析为 null，绝不回退到 pp 自己继承的 stdio。
    shell.set_embedded_mode(true);
    shell.set_umask(config.umask);
    let dyn_supervisor: Arc<dyn ProcessSupervisor> = supervisor.clone();
    shell.set_process_supervisor(dyn_supervisor);
    // 本次 run 专属的临时目录与缓存只写入逻辑 Shell，不改宿主环境。
    set_exported_path(&mut shell, "TMPDIR", scratch)?;
    set_exported_path(&mut shell, "TMP", scratch)?;
    set_exported_path(&mut shell, "TEMP", scratch)?;
    set_exported_path(&mut shell, "XDG_CACHE_HOME", config.cache)?;

    // run_string does not push this frame itself; mirror run_dash_c_command.
    shell.start_command_string_mode();
    let mut params = shell.default_exec_params();
    // 监督器在 deadline/取消/显式 stop 时取消该 token；core 在边界协作退出。
    params.set_cancel_token(supervisor.execution_token());
    params.set_pipeline_observer(config.pipes.observer());

    let run_result = {
        let source = SourceInfo::from("-c");
        // tokio 的 select!/pin! 需要 macros feature；这里只用 time + std::pin。
        let mut run = std::pin::pin!(shell.run_string(config.command, &source, &params));
        let mut last_pipe_revision = config.pipes.revision();
        loop {
            match tokio::time::timeout(PREVIEW_INTERVAL, run.as_mut()).await {
                Ok(outcome) => break outcome,
                Err(_) => {
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
                        // 预览复制在锁内，emit 在锁外；阻塞的 UI 回调不阻塞 reader。
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
    // 成功 exec 已把逻辑 shell 替换为受管外部程序，不能再执行旧 EXIT trap；
    // 正常 exit（ExitShell）仍需运行。EXIT handler 复用本轮受管参数：deadline/
    // 取消仍能在解释器边界停止它，其中的管道照常进入观察器。
    if error.is_none() && !exec_replaced {
        let _ = shell.on_exit_with_params(&params).await;
    }
    drop(params);
    // 释放逻辑输出写端，让本轮唯一的 reader 看到 EOF。
    drop(shell);
    Ok(RunReport { exit_code, error })
}

fn set_exported_path(shell: &mut Shell, name: &str, value: &Path) -> Result<()> {
    let mut variable = ShellVariable::new(value.to_string_lossy().into_owned());
    variable.export();
    shell
        .set_env_global(name, variable)
        .with_context(|| format!("setting {name} in the embedded shell"))
}
