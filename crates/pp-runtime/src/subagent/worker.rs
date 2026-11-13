//! 名额获取、空闲 worker 摘除与 worker 主循环。

use anyhow::{Context, Result, ensure};
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::events::AgentEvent;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Weak, mpsc},
    thread,
    time::{Duration, Instant},
};
use tokio::sync::OwnedSemaphorePermit;

use crate::agent::Agent;
use crate::agent::input::InputQueue;

use super::observe::HubEvent;
use super::workspace;
use super::{
    Binding, IDLE_POLL_INTERVAL, Inner, Record, Work, accumulate_cost, atomic_json, lock, now,
};

impl Binding {
    pub(super) fn permit(&self, token: &CancellationToken) -> Result<OwnedSemaphorePermit> {
        use std::{
            future::Future,
            task::{Context as TaskContext, Poll, Waker},
        };

        // 保留同一个 acquire future，让 Semaphore 管理 FIFO 和取消后的名额归还。
        // 同步 worker 通过 unpark 唤醒，不需要额外的 Tokio runtime。
        let mut acquire = std::pin::pin!(self.inner.semaphore.clone().acquire_owned());
        let waker = Waker::from(Arc::new(PermitWake(thread::current())));
        let mut context = TaskContext::from_waker(&waker);
        loop {
            if token.is_cancelled() || lock(&self.inner.state).stopping {
                return Err(pp_core::cancellation::Cancelled.into());
            }
            match acquire.as_mut().poll(&mut context) {
                Poll::Ready(permit) => return Ok(permit?),
                // 取消 token 不唤醒当前线程，保留有界 tick 检查取消与关闭。
                Poll::Pending => thread::park_timeout(IDLE_POLL_INTERVAL),
            }
        }
    }
    pub(crate) fn interaction<T>(
        &self,
        token: &CancellationToken,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        cancellation::check(token)?;
        f()
    }
    pub(crate) fn request_budget(&self) -> usize {
        self.inner.settings.request_budget
    }
}
struct PermitWake(thread::Thread);
impl std::task::Wake for PermitWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// 空闲 worker 退出前在状态锁内摘除自己的 Live 槽：只有确认没有排队工作时才
/// 允许退出，`launch`/`followup` 因此不会把新任务排进无人消费的通道。
fn detach_idle_worker(inner: &Inner, id: &str, generation: u64) -> bool {
    let mut state = lock(&inner.state);
    let queued = state
        .disk
        .runs
        .values()
        .any(|run| run.agent_id == id && matches!(run.status.as_str(), "queued" | "running"));
    if queued
        || !state
            .live
            .get(id)
            .is_some_and(|live| live.generation == generation)
    {
        return false;
    }
    state.live.remove(id);
    drop(state);
    inner.changed.notify_all();
    true
}

pub(super) fn worker(
    weak: Weak<Inner>,
    record: Record,
    generation: u64,
    input: InputQueue,
    receiver: mpsc::Receiver<Work>,
) {
    let Some(inner) = weak.upgrade() else { return };
    struct WorkerExit {
        inner: Arc<Inner>,
        id: String,
        generation: u64,
    }
    impl Drop for WorkerExit {
        fn drop(&mut self) {
            let parked;
            {
                let mut state = lock(&self.inner.state);
                state.workers = state.workers.saturating_sub(1);
                if state.retiring.get(&self.id) == Some(&self.generation) {
                    state.retiring.remove(&self.id);
                }
                parked = state
                    .disk
                    .agents
                    .get(&self.id)
                    .is_some_and(|r| r.generation == self.generation && r.status != "parked");
                if state
                    .disk
                    .agents
                    .get(&self.id)
                    .is_some_and(|r| r.generation == self.generation)
                {
                    state.live.remove(&self.id);
                    if let Some(record) = state.disk.agents.get_mut(&self.id) {
                        record.status = "parked".into();
                    }
                    let _ = self.inner.save(&state);
                }
                self.inner.changed.notify_all();
            }
            // 只在实际状态转换时发布一次 parked。
            if parked {
                self.inner.observe(
                    &self.id,
                    None,
                    self.generation,
                    None,
                    None,
                    Some(HubEvent::AgentState {
                        status: "parked".into(),
                    }),
                );
            }
        }
    }
    let _exit = WorkerExit {
        inner: inner.clone(),
        id: record.id.clone(),
        generation,
    };
    let mut record = record;
    let mut binding = Binding {
        inner,
        owner: record.id.clone(),
        depth: record.spec.depth,
        spawns: record.spec.spawns.clone(),
        model: record.spec.model.clone(),
        thinking: record.spec.thinking,
        cwd: record.spec.cwd.clone(),
        readonly: record.spec.readonly,
    };
    let mut agent: Option<Agent> = None;
    let mut workspace_ready = !record.spec.isolated;
    // 空闲 TTL 从最后一次真实 work 结束开始计算；50ms 诊断 tick 不能续命。
    let idle_ttl = (binding.inner.settings.idle_ttl_ms != 0)
        .then(|| Duration::from_millis(binding.inner.settings.idle_ttl_ms));
    let mut idle_since = Instant::now();
    // 迟到诊断沿用其所属 run 的 id，并延续该 run 的 event sequence。
    let mut late_sequences: BTreeMap<String, u64> = BTreeMap::new();
    loop {
        let work = match receiver.recv_timeout(IDLE_POLL_INTERVAL) {
            Ok(work) => work,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if idle_ttl.is_some_and(|ttl| idle_since.elapsed() >= ttl)
                    && detach_idle_worker(&binding.inner, &record.id, generation)
                {
                    break;
                }
                if let Some(native) = agent.as_mut() {
                    // 只追加报告事件：不改 run 状态/结果/请求数，也不向父模型投递。
                    let polled = native.poll_lsp_diagnostics_with_run(&mut |origin, event| {
                        let sequence = origin.map(|run| {
                            let sequence = late_sequences.entry(run.to_owned()).or_insert(0);
                            *sequence += 1;
                            *sequence
                        });
                        binding.inner.observe(
                            &record.id,
                            origin,
                            generation,
                            sequence,
                            Some(&event),
                            None,
                        );
                    });
                    if let Err(error) = polled {
                        lock(&binding.inner.cleanup_errors).push(error);
                        let _ = detach_idle_worker(&binding.inner, &record.id, generation);
                        break;
                    }
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let started = Instant::now();
        let mut run_sequence = 0u64;
        let _ = input.begin_run();
        let mut last = String::new();
        // 名额覆盖初始化、全部模型轮次、工具执行与结果收尾；出错也由 RAII 释放。
        let mut permit = None;
        let outcome = (|| -> Result<Value> {
            permit = Some(binding.permit(&work.cancellation)?);
            {
                let mut state = lock(&binding.inner.state);
                state
                    .disk
                    .runs
                    .get_mut(&work.run)
                    .context("unknown run")?
                    .status = "running".into();
                let current = state
                    .disk
                    .agents
                    .get_mut(&record.id)
                    .context("unknown agent")?;
                current.status = "running".into();
                current.updated_ms = now();
                binding.inner.save(&state)?;
            }
            binding.inner.observe(
                &record.id,
                None,
                generation,
                None,
                None,
                Some(HubEvent::AgentState {
                    status: "running".into(),
                }),
            );
            binding.inner.observe(
                &record.id,
                Some(&work.run),
                generation,
                None,
                None,
                Some(HubEvent::RunState {
                    status: "running".into(),
                }),
            );
            cancellation::check(&work.cancellation)?;
            if agent.is_none() {
                if !workspace_ready {
                    record.spec.cwd = workspace::prepare(
                        &record.spec.cwd,
                        &binding.inner.directory(),
                        &record.id,
                        &work.cancellation,
                    )?;
                    binding.cwd = record.spec.cwd.clone();
                    workspace_ready = true;
                }
                let factory = lock(&binding.inner.factory)
                    .clone()
                    .context("supervisor stopped")?;
                agent = Some(factory(&record, binding.clone())?);
            }
            let native = agent.as_mut().context("agent initialization failed")?;
            native.set_task_accounting(work.accounting.clone());
            native.set_lsp_run_id(Some(work.run.clone()));
            let mut requests = 0usize;
            let mut tokens = 0u64;
            let mut cost = record.spec.cost.map(|_| 0.0);
            let outcome = native.run(&work.task, &work.cancellation, Some(&input), &mut |event| {
                run_sequence += 1;
                match &event {
                    AgentEvent::AssistantFinished { text } if !text.trim().is_empty() => {
                        last = text.clone()
                    }
                    AgentEvent::ModelRequestStarted => requests += 1,
                    AgentEvent::ModelRequestFinished {
                        usage: Some(usage), ..
                    } => {
                        tokens += usage.input_tokens + usage.output_tokens;
                        let current_price = lock(&binding.inner.state)
                            .disk
                            .agents
                            .get(&record.id)
                            .and_then(|r| r.spec.cost);
                        cost = accumulate_cost(cost, current_price, *usage);
                    }
                    AgentEvent::ModelRequestFinished { usage: None, .. } => cost = None,
                    _ => {}
                }
                if matches!(
                    event,
                    AgentEvent::ModelRequestStarted | AgentEvent::ModelRequestFinished { .. }
                ) {
                    {
                        let mut state = lock(&binding.inner.state);
                        if let Some(run) = state.disk.runs.get_mut(&work.run) {
                            run.requests = requests;
                            run.tokens = tokens;
                            if matches!(event, AgentEvent::ModelRequestFinished { .. }) {
                                run.cost = cost;
                            }
                        }
                    }
                    binding.inner.bump_revision();
                }
                // 整条 JSON + 换行在同一把 agent 追加锁内写出，控制线程不会与之交错。
                binding.inner.observe(
                    &record.id,
                    Some(&work.run),
                    generation,
                    Some(run_sequence),
                    Some(&event),
                    None,
                );
            });
            native.save_child(
                &binding
                    .inner
                    .directory()
                    .join(format!("{}.session.json", record.id)),
            )?;
            {
                let mut state = lock(&binding.inner.state);
                if let Some(current) = state
                    .disk
                    .agents
                    .get_mut(&record.id)
                    .filter(|r| r.generation == generation)
                {
                    current.session_file = Some(
                        binding
                            .directory()
                            .join(format!("{}.session.json", record.id)),
                    );
                }
            }
            let outcome = outcome?;
            let result = if outcome == pp_core::cancellation::RunOutcome::Cancelled {
                json!({"ok":false,"cancelled":true,"output":last})
            } else {
                ensure!(
                    !last.trim().is_empty() || native.yielded().is_some(),
                    "subagent returned no result"
                );
                json!({"ok":true,"output":native.yielded().unwrap_or(Value::String(last.clone()))})
            };
            let mut state = lock(&binding.inner.state);
            if let Some(run) = state.disk.runs.get_mut(&work.run) {
                run.requests = requests;
                run.tokens = tokens;
                run.cost = cost;
            }
            drop(state);
            Ok(result)
        })();
        let outcome = outcome.and_then(|mut value| {
            if record.spec.isolated {
                let sink = lock(&binding.inner.changes).clone();
                let _guard = sink.as_ref().map(|s| lock(&s.serial));
                let captured = workspace::capture(
                    &binding.inner.directory(),
                    &record.id,
                    value["ok"] == true,
                    &CancellationToken::new(),
                )?;
                value["patches"] = json!(captured.patches);
                if let Some(sink) = &sink {
                    for change in captured.changes {
                        if let Ok(path) = change.path.strip_prefix(&sink.root) {
                            let mut tracker = lock(&sink.tracker);
                            let before = change.before.map_or(
                                crate::tools::changes::Original::Missing,
                                crate::tools::changes::Original::Bytes,
                            );
                            if let Some(after) = change.after {
                                tracker.record(path.to_owned(), before, after);
                            } else {
                                tracker.record_resource(path.to_owned(), before, None);
                            }
                            tracker.source(path, &record.id);
                        }
                    }
                }
            }
            Ok(value)
        });
        let mut result = match outcome {
            Ok(value) => value,
            Err(error) => {
                let result = json!({"ok":false,"cancelled":work.cancellation.is_cancelled(),"error":format!("{error:#}"),"partial_output":last});
                if error
                    .chain()
                    .any(|cause| cause.is::<pp_core::cancellation::CleanupError>())
                {
                    lock(&binding.inner.cleanup_errors).push(error);
                }
                result
            }
        };
        let output_path = binding
            .inner
            .directory()
            .join(format!("{}.result.json", work.run));
        if let Err(error) = atomic_json(&output_path, &result) {
            result = json!({"ok":false,"error":format!("failed to save result file: {error:#}")});
        } else if let Some(text) = result["output"]
            .as_str()
            .filter(|s| s.chars().count() > 11500)
        {
            let preview = text.chars().take(5000).collect::<String>();
            result["output"] = json!(preview);
            result["truncated"] = json!(true);
            result["output_path"] = json!(output_path);
        }

        if let Err(e) = binding.settle(&work.run, result, started.elapsed().as_millis() as u64) {
            tracing::error!(error=%e,"subagent result persist failed");
        }
        // 只有真实 work 结束才重置空闲基线；诊断 tick 不能延长 TTL。
        idle_since = Instant::now();
        late_sequences.insert(work.run.clone(), run_sequence);
        let state = lock(&binding.inner.state);
        if state.stopping
            || state
                .disk
                .agents
                .get(&record.id)
                .is_none_or(|r| r.generation != generation)
        {
            break;
        }
        drop(state);
        drop(permit);
    }
    if let Some(native) = agent.as_mut() {
        // park/退出时取消该 owner 的等待；共享父 Registry 的 worker 不停父进程。
        native.cancel_lsp_diagnostics();
        if record.spec.isolated
            && let Err(error) = native.stop_lsp()
        {
            if let Some(cleanup) = native.lsp_cleanup() {
                lock(&binding.inner.lsp_cleanup).push(cleanup);
            }
            lock(&binding.inner.cleanup_errors).push(error);
        }
    }
    if let Some(agent) = agent {
        let _ = agent.save_child(
            &binding
                .inner
                .directory()
                .join(format!("{}.session.json", record.id)),
        );
    }
    let parked;
    {
        let mut state = lock(&binding.inner.state);
        parked = state
            .disk
            .agents
            .get(&record.id)
            .is_some_and(|r| r.generation == generation && r.status != "parked");
        if state
            .disk
            .agents
            .get(&record.id)
            .is_some_and(|r| r.generation == generation)
        {
            state.live.remove(&record.id);
            if let Some(r) = state.disk.agents.get_mut(&record.id) {
                r.status = "parked".into();
            }
            let _ = binding.inner.save(&state);
        }
        binding.inner.changed.notify_all();
    }
    if parked {
        binding.inner.observe(
            &record.id,
            None,
            generation,
            None,
            None,
            Some(HubEvent::AgentState {
                status: "parked".into(),
            }),
        );
    }
}
