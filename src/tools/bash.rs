mod host;
mod relay;
mod streams;
mod supervisor;

pub(crate) use streams::PipeStore;

use super::output::OutputSink;
use crate::cancellation::{CancellationToken, CleanupError};
use crate::events::AgentEvent;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::Instant,
};

fn new_pipe_run(store: &PipeStore, owner: &str) -> Result<streams::PipeRun> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).context("allocating pipeline run identifier")?;
    let mut id = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(id, "{byte:02x}").expect("writing to a String cannot fail");
    }
    store
        .start_run(id, owner)
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    cwd: &Path,
    cache: &Path,
    command: &str,
    timeout_seconds: u64,
    call_id: &str,
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
    sink: OutputSink,
    defaults: crate::host_defaults::HostDefaults,
    pipe_store: &PipeStore,
) -> Result<Value> {
    let pipes = new_pipe_run(pipe_store, call_id)?;
    let stop = CancellationToken::new();
    let sink = Arc::new(Mutex::new(sink));
    let outcome = host::run_host(
        host::HostConfig {
            cwd,
            cache,
            command,
            timeout_seconds,
            call_id,
            cancellation,
            stop: &stop,
            umask: defaults.umask,
            pipes: &pipes,
        },
        sink.clone(),
        Some(emit),
    )
    .context("running embedded bash")?;
    let mut result = lock(&sink).value();
    result["pipes"] = pipes.pipes_json();
    let (ok, timed_out, cancelled) = match outcome.termination {
        host::Termination::Exited => (outcome.exit_code == Some(0), false, false),
        host::Termination::TimedOut => (false, true, false),
        host::Termination::Cancelled => (false, false, true),
        host::Termination::Stopped => (false, false, false),
    };
    result["ok"] = json!(ok);
    result["exit_code"] = json!(outcome.exit_code);
    result["timed_out"] = json!(timed_out);
    result["cancelled"] = json!(cancelled);
    if cancelled {
        result["error"] = json!("Command cancelled; side effects were not rolled back.");
    }
    if !outcome.output_drained {
        result["warning"] = json!(
            "Command ended, but an inherited output descriptor remained open; captured output may be incomplete."
        );
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

/// 执行核心自身失败时是否属于“进程/临时资源无法收尾”。
/// 这类错误不能降级成普通的命令失败，必须让调用方看到清理问题。
fn is_cleanup_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<CleanupError>())
}

/// 后台任务线程独占的一份输入；路径与命令已复制，线程不借用调用方栈。
struct BackgroundRun {
    cwd: PathBuf,
    cache: PathBuf,
    command: String,
    call_id: String,
    umask: u32,
    sink: Arc<Mutex<OutputSink>>,
    cancellation: CancellationToken,
}

struct BackgroundTask {
    label: String,
    started: Instant,
    stop: CancellationToken,
    /// 运行中的管道 run 句柄；任务完成后由 [`BackgroundState::final_pipes`]
    /// 取代并释放，避免已从 registry 淘汰的环形缓存仍被任务永久强引用。
    pipes: Mutex<Option<streams::PipeRun>>,
    state: Mutex<BackgroundState>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

struct BackgroundState {
    duration_ms: Option<u64>,
    status: &'static str,
    exit_code: Option<i32>,
    output: Arc<Mutex<OutputSink>>,
    error: Option<String>,
    warning: Option<String>,
    cleanup_error: Option<String>,
    /// 完成时从 run 复制出的有界阶段/流元数据（不含阶段正文）。
    final_pipes: Option<Value>,
}

impl BackgroundTask {
    fn snapshot(&self, id: &str) -> Result<Value> {
        // 仅复制有界预览及状态，完整日志由独立资源快照读取。
        let state = lock(&self.state);
        if let Some(error) = &state.cleanup_error {
            return Err(cleanup_error(error));
        }
        let mut value = lock(state.output.as_ref()).value();
        let fields = json!({
            "ok": matches!(state.status, "running" | "exited" | "stopped"),
            "task_id": id, "status": state.status, "exit_code": state.exit_code,

        });
        value
            .as_object_mut()
            .expect("输出总是对象")
            .extend(fields.as_object().expect("状态总是对象").clone());
        if let Some(error) = &state.error {
            value["error"] = json!(error);
        }
        if let Some(warning) = &state.warning {
            value["warning"] = json!(warning);
        }
        let final_pipes = state.final_pipes.clone();
        drop(state);
        // 运行中读同一份 live run；完成后只读有界元数据，不重新持有已可淘汰的
        // run 状态与环形缓存。
        let pipes =
            final_pipes.or_else(|| lock(&self.pipes).as_ref().map(streams::PipeRun::pipes_json));
        if let Some(pipes) = pipes {
            value["pipes"] = pipes;
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
    pub(super) fn wait_jobs(&self, ids: &[String]) -> Result<Vec<super::wait::Job>> {
        let tasks = {
            let registry = lock(&self.registry);
            for id in ids {
                anyhow::ensure!(
                    registry.tasks.contains_key(id),
                    "Unknown background task: {id}"
                );
            }
            registry
                .tasks
                .iter()
                .filter(|(id, _)| ids.is_empty() || ids.contains(id))
                .map(|(id, task)| (id.clone(), task.clone()))
                .collect::<Vec<_>>()
        };
        tasks
            .into_iter()
            .map(|(id, task)| {
                let state = lock(&task.state);
                if let Some(error) = &state.cleanup_error {
                    return Err(cleanup_error(error));
                }
                Ok(super::wait::Job {
                    id,
                    r#type: "bash".into(),
                    label: task.label.clone(),
                    status: state.status.into(),
                    duration_ms: state
                        .duration_ms
                        .unwrap_or_else(|| task.started.elapsed().as_millis() as u64),
                    delivered: false,
                })
            })
            .collect()
    }

    #[cfg(test)]
    pub(super) fn start(
        &self,
        cwd: &Path,
        cache: &Path,
        command: &str,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        self.start_with_output(
            cwd,
            cache,
            command,
            "test-background",
            cancellation,
            crate::host_defaults::current(),
            &PipeStore::new(),
            OutputSink::new(Default::default(), Default::default()),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn start_with_output(
        &self,
        cwd: &Path,
        cache: &Path,
        command: &str,
        call_id: &str,
        cancellation: &CancellationToken,
        defaults: crate::host_defaults::HostDefaults,
        pipe_store: &PipeStore,
        sink: OutputSink,
    ) -> Result<Value> {
        let mut registry = lock(&self.registry);
        anyhow::ensure!(!registry.closed, "Background task manager is closed.");
        crate::cancellation::check(cancellation)?;
        let next_id = registry
            .next_id
            .checked_add(1)
            .context("background task ID exhausted")?;
        let id = format!("bg-{next_id}");
        // 与前台一致：每个任务一个 run，共享根会话的临时管道存储。
        let pipes = new_pipe_run(pipe_store, call_id)?;
        let output = Arc::new(Mutex::new(sink));
        let task = Arc::new(BackgroundTask {
            label: command.chars().take(200).collect(),
            started: Instant::now(),
            stop: CancellationToken::new(),
            pipes: Mutex::new(Some(pipes)),
            state: Mutex::new(BackgroundState {
                duration_ms: None,
                status: "running",
                exit_code: None,
                output: output.clone(),
                error: None,
                warning: None,
                cleanup_error: None,
                final_pipes: None,
            }),
            worker: Mutex::new(None),
        });
        let worker_task = task.clone();
        let run = BackgroundRun {
            cwd: cwd.to_owned(),
            cache: cache.to_owned(),
            command: command.to_owned(),
            call_id: call_id.to_owned(),
            umask: defaults.umask,
            sink: output,
            cancellation: cancellation.clone(),
        };
        let worker = match thread::Builder::new()
            .name(format!("pp-{id}"))
            .spawn(move || background_worker(&worker_task, &run))
        {
            Ok(worker) => worker,
            Err(error) => {
                // 线程没有启动时解释器不会收尾，run 不能留在 running 状态。
                if let Some(pipes) = lock(&task.pipes).take() {
                    pipes.finish(streams::RunEnd::interrupted("host_setup_failed"));
                }
                return Err(error).context("starting background executor");
            }
        };
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

/// 后台任务线程：与前台共用同一个内嵌执行核心，但没有固定截止时间。
fn background_worker(task: &BackgroundTask, run: &BackgroundRun) {
    let Some(pipes) = lock(&task.pipes).as_ref().cloned() else {
        // run 已被回收（仅可能来自启动失败路径），不再启动解释器。
        return;
    };
    let result = host::run_host(
        host::HostConfig {
            cwd: &run.cwd,
            cache: &run.cache,
            command: &run.command,
            // 后台任务只由父任务取消或显式 stop 收尾；时间上限等同于“不超时”。
            timeout_seconds: u64::MAX,
            call_id: &run.call_id,
            cancellation: &run.cancellation,
            stop: &task.stop,
            umask: run.umask,
            pipes: &pipes,
        },
        run.sink.clone(),
        None,
    );
    // 执行核心已封存本次 run；复制有界元数据并先发布到状态，再释放任务对
    // run 的强引用。快照因此在任何时刻要么读到 live run，要么读到最终元数据，
    // 不会出现“状态已完成但元数据缺失”的中间态。
    let final_pipes = pipes.pipes_json();
    let mut state = lock(&task.state);
    // 执行核心正常路径已封存；这里覆盖启动阶段就失败的路径。
    lock(state.output.as_ref()).finish();
    state.duration_ms = Some(task.started.elapsed().as_millis() as u64);
    state.final_pipes = Some(final_pipes);
    match result {
        Ok(outcome) => {
            state.exit_code = outcome.exit_code;
            match outcome.termination {
                host::Termination::Exited => {
                    let succeeded = outcome.exit_code == Some(0);
                    state.status = if succeeded { "exited" } else { "failed" };
                    state.error = (!succeeded).then(|| match outcome.exit_code {
                        Some(code) => format!("Background command exited with status {code}."),
                        None => "Background command was terminated by a signal.".to_owned(),
                    });
                }
                host::Termination::Stopped => state.status = "stopped",
                host::Termination::Cancelled => {
                    state.status = "cancelled";
                    state.error = Some(
                        "Background task cancelled; side effects were not rolled back.".into(),
                    );
                }
                host::Termination::TimedOut => {
                    state.status = "failed";
                    state.error = Some("Background command timed out.".into());
                }
            }
            if !outcome.output_drained {
                state.warning = Some(
                    "Background command ended, but an inherited output descriptor remained open; captured output may be incomplete."
                        .into(),
                );
            }
        }
        Err(error) => {
            state.status = "failed";
            if is_cleanup_error(&error) {
                state.cleanup_error = Some(format!("{error:#}"));
            } else {
                state.error = Some(format!("{error:#}"));
            }
        }
    }
    drop(state);
    // 元数据已固化为最终快照；此后 registry 淘汰才能真正释放环形缓存。
    drop(pipes);
    lock(&task.pipes).take();
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::process::Pid;
    use std::time::Duration;

    const POLL: Duration = Duration::from_millis(5);

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
            // 管理 TERM 会真实执行 trap（cleanup.done），但取消本身不可被 trap
            // 的 `exit 0` 否决：逻辑 shell 以终止状态收尾。
            assert_eq!(snapshot["exit_code"], 128 + 15, "{method}: {snapshot}");
            assert_eq!(
                snapshot["status"],
                if method == "cancel" {
                    "cancelled"
                } else {
                    "stopped"
                }
            );
            assert_eq!(snapshot["truncated"], true);
            assert_eq!(snapshot["total_bytes"], 64 * 4096 * 2);
            assert!(snapshot["output"].as_str().unwrap().len() < 1024);
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
                &format!("sh -c 'trap \"\" TERM; sleep 60 & child=$!; printf \"%s\\n%s\\n%s\\n\" \"$$\" \"$child\" \"$TMPDIR\" > {name}; wait \"$child\"'"),
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
                        "sh -c 'printf \"%s\" \"$$\" > race.pending; mv race.pending race.pid; exec sleep 60'",
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
    fn background_completed_tasks_release_evicted_pipe_bodies() {
        // 每个任务把超过单流 1 MiB 的正文送过管道；完成 run 只按 registry 上限保留。
        const RUNS: usize = super::streams::MAX_COMPLETED_RUNS + 2;
        const CHUNK: u64 = 65536;
        const CHUNKS: usize = 24;

        let dir = tempfile::tempdir().unwrap();
        let store = PipeStore::new();
        let manager = TaskManager::default();
        let mut tasks = Vec::new();
        for index in 0..RUNS {
            let snapshot = manager
                .start_with_output(
                    dir.path(),
                    dir.path(),
                    "for ((i=0; i<24; i++)); do printf '%65536s' ''; done | cat > /dev/null",
                    "test-background-pipes",
                    &CancellationToken::new(),
                    crate::host_defaults::current(),
                    &store,
                    OutputSink::new(Default::default(), Default::default()),
                )
                .unwrap();
            let id = snapshot["task_id"].as_str().unwrap().to_owned();
            let task = manager.task(&id).unwrap();
            // 运行中由 live run 提供同一份元数据。
            assert!(
                snapshot["pipes"]["run_id"]
                    .as_str()
                    .is_some_and(|run_id| run_id.len() == 32),
                "running task {index} lost its pipeline run"
            );
            let watch = lock(&task.pipes)
                .as_ref()
                .expect("running task must own its run handle")
                .watch();
            task.join().unwrap();
            tasks.push((id, watch));
        }

        // 完成 run 只保留最近 MAX_COMPLETED_RUNS 条；更老的 URI 已失效。
        assert_eq!(store.list().len(), super::streams::MAX_COMPLETED_RUNS);
        for (index, (id, watch)) in tasks.iter().enumerate() {
            let task = manager.task(id).unwrap();
            let snapshot = task.snapshot(id).unwrap();
            assert_eq!(snapshot["status"], "exited", "task {index}");
            let pipes = &snapshot["pipes"];
            assert_eq!(
                pipes["streams"].as_array().unwrap().len(),
                1,
                "task {index}"
            );
            assert_eq!(
                pipes["streams"][0]["total_bytes"].as_u64().unwrap(),
                CHUNK * CHUNKS as u64,
                "task {index}"
            );
            let retained = pipes["streams"][0]["retained_end"].as_u64().unwrap()
                - pipes["streams"][0]["retained_start"].as_u64().unwrap();
            assert_eq!(
                retained,
                super::streams::STREAM_RING_CAP as u64,
                "task {index} must hold one capped ring body"
            );
            let run_id = pipes["run_id"].as_str().unwrap();
            // 任务句柄完成后不再强引用 live run。
            assert!(
                lock(&task.pipes).is_none(),
                "task {index} kept a live run handle"
            );
            if index < RUNS - super::streams::MAX_COMPLETED_RUNS {
                assert!(
                    store.snapshot(run_id).is_err(),
                    "old run {index} must be evicted from the store"
                );
                // 淘汰后若任务仍持有 PipeRun，Ring 正文会绕过配额永久存活。
                assert!(
                    !watch.is_alive(),
                    "task {index} still owns the evicted run body"
                );
            } else {
                assert!(
                    store.snapshot(run_id).is_ok(),
                    "recent run {index} must remain readable"
                );
                assert!(watch.is_alive(), "recent run {index} must stay alive");
            }
        }
        manager.stop_all().unwrap();
    }
}
