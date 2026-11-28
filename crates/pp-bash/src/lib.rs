mod display;
mod host;
mod observation;
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod sandbox_tests;
mod supervisor;

use anyhow::{Context, Result};
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::CleanupError;
use pp_core::events::AgentEvent;
use pp_sandbox::Policy;
use pp_workspace::output::OutputSink;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::Instant,
};

#[allow(clippy::too_many_arguments)]
pub fn run(
    cwd: &Path,
    cache: &Path,
    command: &str,
    timeout_seconds: u64,
    call_id: &str,
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
    sink: OutputSink,
    defaults: pp_workspace::host_defaults::HostDefaults,
    sandbox: Option<Arc<Policy>>,
) -> Result<Value> {
    let started = Instant::now();
    let observer = Arc::new(observation::Observer::new(command));
    if let Some(snapshot) = observer.shell.snapshot() {
        emit(AgentEvent::ToolShellSnapshot {
            id: call_id.to_owned(),
            snapshot,
        });
    }
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
            observer: &observer,
            sandbox: sandbox.as_ref(),
        },
        sink.clone(),
        Some(emit),
    )
    .context("running embedded bash")?;
    let mut result = lock(&sink).value();
    if let Some(failures) = observer.failures() {
        result["pipeline_failures"] = serde_json::to_value(failures)?;
    }
    if let Some(snapshot) = observer.shell.snapshot() {
        result["shell"] = serde_json::to_value(snapshot)?;
    }
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
    result["timeout_seconds"] = json!(timeout_seconds);
    result["duration_ms"] = json!(started.elapsed().as_millis() as u64);
    if timed_out {
        result["error"] = json!(format!(
            "Command timed out after {timeout_seconds} seconds. Inspect captured output for interactive input, network stalls, or excessive test/build scope before choosing a longer timeout. Side effects were not rolled back; do not automatically rerun commands with side effects."
        ));
    }
    if cancelled {
        result["error"] = json!("Command cancelled; side effects were not rolled back.");
    }
    if !outcome.output_drained {
        result["warning"] = json!(
            "Command ended, but an inherited output descriptor remained open; captured output may be incomplete."
        );
    }
    if !ok
        && let Some(policy) = sandbox.as_deref()
        && (outcome.sandbox_blocked
            || result["output"]
                .as_str()
                .is_some_and(Policy::is_denied_output))
    {
        attach_sandbox_denial(&mut result, policy);
    }
    Ok(result)
}

fn attach_sandbox_denial(result: &mut Value, policy: &Policy) {
    let meta = policy.denial_metadata();
    result["sandbox"] = json!({
        "blocked": true,
        "mode": meta.mode,
        "network": meta.network,
        "writable_roots": meta.writable_roots,
        "hint": meta.hint,
    });
}

#[derive(Default)]
pub struct TaskManager {
    registry: Arc<Mutex<TaskRegistry>>,
}

#[derive(Default)]
struct TaskRegistry {
    next_id: u64,
    closed: bool,
    tasks: BTreeMap<String, Arc<BackgroundTask>>,
}

pub struct BackgroundCleanup {
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

/// Whether a failure of the execution core itself means "process/temporary resources cannot be cleaned up".
/// Such errors must not be downgraded to an ordinary command failure; callers must see the cleanup problem.
fn is_cleanup_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<CleanupError>())
}

/// A private copy of the inputs for the background task thread; paths and command are copied so the
/// thread never borrows the caller's stack.
struct BackgroundRun {
    cwd: PathBuf,
    cache: PathBuf,
    command: String,
    call_id: String,
    umask: u32,
    sink: Arc<Mutex<OutputSink>>,
    cancellation: CancellationToken,
    sandbox: Option<Arc<Policy>>,
}

struct BackgroundTask {
    label: String,
    started: Instant,
    stop: CancellationToken,
    sandbox: Option<Arc<Policy>>,
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
    sandbox_blocked: bool,
    observer: Option<Arc<observation::Observer>>,
    final_failures: Option<observation::PipelineFailures>,
    final_shell: Option<pp_core::shell_display::ShellPresentation>,
}

impl BackgroundTask {
    fn snapshot(&self, id: &str) -> Result<Value> {
        // Copy only the bounded preview and state; the full log is read from a separate resource snapshot.
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
            .expect("output is always an object")
            .extend(
                fields
                    .as_object()
                    .expect("state is always an object")
                    .clone(),
            );
        if let Some(error) = &state.error {
            value["error"] = json!(error);
        }
        if let Some(warning) = &state.warning {
            value["warning"] = json!(warning);
        }
        let failed = state.status == "failed";
        let sandbox_blocked = state.sandbox_blocked;
        let failures = state
            .final_failures
            .clone()
            .or_else(|| state.observer.as_ref()?.failures());
        let shell = state
            .final_shell
            .clone()
            .or_else(|| state.observer.as_ref()?.shell.snapshot());
        drop(state);
        if failed
            && let Some(policy) = self.sandbox.as_deref()
            && (sandbox_blocked
                || value["output"]
                    .as_str()
                    .is_some_and(Policy::is_denied_output))
        {
            attach_sandbox_denial(&mut value, policy);
        }
        if let Some(failures) = failures {
            value["pipeline_failures"] = serde_json::to_value(failures)?;
        }
        if let Some(shell) = shell {
            value["shell"] = serde_json::to_value(shell)?;
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
    pub fn wait_jobs(&self, ids: &[String]) -> Result<Vec<pp_workspace::WaitJob>> {
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
                Ok(pp_workspace::WaitJob {
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
    fn start(
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
            pp_workspace::host_defaults::HostDefaults::test_default(),
            OutputSink::new(Default::default(), Default::default()),
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_with_output(
        &self,
        cwd: &Path,
        cache: &Path,
        command: &str,
        call_id: &str,
        cancellation: &CancellationToken,
        defaults: pp_workspace::host_defaults::HostDefaults,
        sink: OutputSink,
        sandbox: Option<Arc<Policy>>,
    ) -> Result<Value> {
        let mut registry = lock(&self.registry);
        anyhow::ensure!(!registry.closed, "Background task manager is closed.");
        pp_core::cancellation::check(cancellation)?;
        let next_id = registry
            .next_id
            .checked_add(1)
            .context("background task ID exhausted")?;
        let id = format!("bg-{next_id}");
        let observer = Arc::new(observation::Observer::new(command));
        let output = Arc::new(Mutex::new(sink));
        let task = Arc::new(BackgroundTask {
            label: command.chars().take(200).collect(),
            started: Instant::now(),
            stop: CancellationToken::new(),
            sandbox: sandbox.clone(),
            state: Mutex::new(BackgroundState {
                duration_ms: None,
                status: "running",
                exit_code: None,
                output: output.clone(),
                error: None,
                warning: None,
                cleanup_error: None,
                sandbox_blocked: false,
                observer: Some(observer.clone()),
                final_failures: None,
                final_shell: None,
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
            sandbox,
        };
        let worker = thread::Builder::new()
            .name(format!("pp-{id}"))
            .spawn(move || background_worker(&worker_task, &run, observer))
            .context("starting background executor")?;
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

    pub fn status(&self, id: &str) -> Result<Value> {
        self.task(id)?.snapshot(id)
    }

    pub fn stop(&self, id: &str) -> Result<Value> {
        let task = self.task(id)?;
        task.stop.cancel();
        task.join()?;
        task.snapshot(id)
    }

    pub fn stop_all(&self) -> Result<()> {
        stop_registry(&self.registry, false)
    }

    pub fn cleanup(&self) -> BackgroundCleanup {
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
    pub fn shutdown(&self) -> Result<()> {
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

/// Background task thread: shares the same embedded execution core as the foreground path but has no
/// fixed deadline.
fn background_worker(
    task: &BackgroundTask,
    run: &BackgroundRun,
    observer: Arc<observation::Observer>,
) {
    let result = host::run_host(
        host::HostConfig {
            cwd: &run.cwd,
            cache: &run.cache,
            command: &run.command,
            // Background tasks are only shut down by the parent task's cancellation or an explicit stop.
            timeout_seconds: 0,
            call_id: &run.call_id,
            cancellation: &run.cancellation,
            stop: &task.stop,
            umask: run.umask,
            observer: &observer,
            sandbox: run.sandbox.as_ref(),
        },
        run.sink.clone(),
        None,
    );
    let mut state = lock(&task.state);
    // The normal path already sealed in the execution core; this covers the startup-failure path.
    lock(state.output.as_ref()).finish();
    state.duration_ms = Some(task.started.elapsed().as_millis() as u64);
    state.final_failures = observer.failures();
    state.observer = None;
    state.final_shell = observer.shell.snapshot();
    match result {
        Ok(outcome) => {
            state.exit_code = outcome.exit_code;
            state.sandbox_blocked = outcome.sandbox_blocked;
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
}

#[cfg(test)]
mod integration {
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
            // Both pipes write more than the capacity and save limit, so the completion marker must be created
            // after those writes.
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
                assert!(
                    Instant::now() < deadline,
                    "background task not ready: {method}"
                );
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
                "TERM cleanup did not finish: {method}"
            );
            let snapshot = task.snapshot(id).unwrap();
            // Managing TERM really runs the trap (cleanup.done), but the cancellation itself cannot be vetoed
            // by the trap's `exit 0`: the logical shell still finishes in a terminated state.
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
            assert!(
                !Path::new(&scratch).exists(),
                "temporary directory not cleaned up: {method}"
            );
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
}
