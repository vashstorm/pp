//! 后台 Bash 与子 Agent 共用的等待屏障；进度只发布展示快照。
use super::*;
use serde::Deserialize;
use std::collections::BTreeSet;

pub const TIMEOUT: Duration = Duration::from_secs(120 * 60);
const PROGRESS: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(25);
const TARGET_GUIDANCE: &str = "For subagents created by task, copy the returned wait_args object, or put their run_id values (e.g. r1) in run_ids. task_ids accepts only task_id values from bash(mode=background) (e.g. bg-1). agent_id values (e.g. a1) and agent:// URIs are for read/write, never wait IDs. Use {} only when you intend to wait for all owned undelivered work. Correct the selector or ID before retrying.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    #[serde(default)]
    run_ids: Vec<String>,
    #[serde(default)]
    task_ids: Vec<String>,
    // 兼容旧调用；时限固定，模型不能通过此参数缩短等待。
    #[serde(default, rename = "timeout_ms")]
    _timeout_ms: u64,
}

pub type Job = pp_workspace::WaitJob;

#[derive(Default)]
pub(super) struct State {
    // 保留由 blocking task 工具直接返回的结果，避免同批 wait 再选中它们。
    blocking_runs: BTreeSet<String>,
    tasks: BTreeSet<String>,
    delivered_tasks: BTreeSet<String>,
}
impl State {
    pub(super) fn clear_background(&mut self) {
        self.tasks.clear();
        self.delivered_tasks.clear();
    }
}

const WAIT_EXECUTE_DESCRIPTION: &str = concat!(
    "Wait until selected subagent runs (`run_ids`) and/or background Bash tasks (`task_ids`) settle (omit both to wait for all undelivered owned work; 120m fixed timeout reaps unfinished targets). ",
    "Reconcile active TODOs before calling `wait`, never after it in the same batch.",
);
const WAIT_PLAN_DESCRIPTION: &str = "Wait until selected read-only subagent runs (`run_ids`) settle (omit `run_ids` to wait for all undelivered owned runs; 120m fixed timeout reaps unfinished runs).";

pub(super) fn definition(mode: WorkMode) -> FunctionObject {
    let (description, properties) = match mode {
        WorkMode::Execute => (
            WAIT_EXECUTE_DESCRIPTION,
            json!({
                "run_ids": {
                    "type": "array",
                    "description": "Subagent `run_id` values (e.g. [\"r1\"], not `agent_id`), or copy `task`'s `wait_args`.",
                    "items": {"type": "string", "minLength": 1}
                },
                "task_ids": {
                    "type": "array",
                    "description": "Background Bash `task_id` values (e.g. [\"bg-1\"]).",
                    "items": {"type": "string", "minLength": 1}
                }
            }),
        ),
        WorkMode::Plan => (
            WAIT_PLAN_DESCRIPTION,
            json!({
                "run_ids": {
                    "type": "array",
                    "description": "Subagent `run_id` values (e.g. [\"r1\"], not `agent_id`), or copy `task`'s `wait_args`.",
                    "items": {"type": "string", "minLength": 1}
                }
            }),
        ),
    };
    FunctionObject {
        name: "wait".into(),
        description: Some(description.into()),
        parameters: Some(json!({
            "type": "object",
            "properties": properties,
            "additionalProperties": false
        })),
        ..Default::default()
    }
}

impl Tools {
    pub(super) fn reserve_task_results(&self, value: &Value) {
        let mut state = self.wait_state.borrow_mut();
        for task in value["tasks"].as_array().into_iter().flatten() {
            for result in task["results"]["results"].as_array().into_iter().flatten() {
                if let Some(id) = result["run"]["id"].as_str() {
                    state.blocking_runs.insert(id.into());
                }
            }
        }
    }

    pub fn start_wait_batch(&self) {
        let mut state = self.wait_state.borrow_mut();
        state.blocking_runs.clear();
        state.tasks.clear();
    }

    /// 只在原生工具批次成功同步后确认；写盘失败时预留记录仍然可重试。
    pub fn commit_wait_results(&self) -> Result<()> {
        let mut state = self.wait_state.borrow_mut();
        if !state.blocking_runs.is_empty()
            && let Some(binding) = &self.subagent
        {
            binding.acknowledge(&state.blocking_runs.iter().cloned().collect::<Vec<_>>())?;
        }
        state.blocking_runs.clear();
        let tasks = std::mem::take(&mut state.tasks);
        state.delivered_tasks.extend(tasks);
        Ok(())
    }

    fn wait_jobs(&self, runs: Option<&[String]>, tasks: Option<&[String]>) -> Result<Vec<Job>> {
        let mut jobs = Vec::new();
        if let Some(ids) = runs {
            if let Some(binding) = &self.subagent {
                jobs.extend(binding.wait_jobs(ids)?);
            } else {
                ensure!(ids.is_empty(), "No subagent runs are available.");
            }
        }
        if let Some(ids) = tasks {
            jobs.extend(self.background_tasks.wait_jobs(ids)?);
        }
        let state = self.wait_state.borrow();
        for job in &mut jobs {
            job.delivered |= if job.r#type == "task" {
                state.blocking_runs.contains(&job.id)
            } else {
                state.tasks.contains(&job.id) || state.delivered_tasks.contains(&job.id)
            };
        }
        Ok(jobs)
    }

    pub(super) fn run_wait(
        &self,
        arguments: &str,
        call_id: &str,
        token: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Value> {
        let started = Instant::now();
        self.run_wait_with_elapsed(arguments, call_id, token, emit, || started.elapsed())
    }

    // 独立时间源让超时回归无需真实等待 120 分钟；生产入口始终使用单调时钟。
    pub fn run_wait_with_elapsed(
        &self,
        arguments: &str,
        call_id: &str,
        token: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
        mut elapsed: impl FnMut() -> Duration,
    ) -> Result<Value> {
        let args: Args = serde_json::from_str(arguments)
            .with_context(|| format!("Invalid wait arguments. {TARGET_GUIDANCE}"))?;
        ensure!(
            args.run_ids
                .iter()
                .chain(&args.task_ids)
                .all(|id| !id.trim().is_empty()),
            "wait IDs must not be blank. {TARGET_GUIDANCE}"
        );
        let explicit = !args.run_ids.is_empty() || !args.task_ids.is_empty();
        // 所有 ID 在任何结果预留之前统一校验。
        let selected = self
            .wait_jobs(
                (!explicit || !args.run_ids.is_empty()).then_some(args.run_ids.as_slice()),
                (!explicit || !args.task_ids.is_empty()).then_some(args.task_ids.as_slice()),
            )
            .with_context(|| format!("Invalid wait targets. {TARGET_GUIDANCE}"))?;
        let selected = selected
            .into_iter()
            .filter(|job| explicit || !job.delivered)
            .collect::<Vec<_>>();
        let runs = selected
            .iter()
            .filter(|j| j.r#type == "task")
            .map(|j| j.id.clone())
            .collect::<Vec<_>>();
        let tasks = selected
            .iter()
            .filter(|j| j.r#type == "bash")
            .map(|j| j.id.clone())
            .collect::<Vec<_>>();
        let mut next_progress = Duration::ZERO;
        loop {
            let mut jobs = self.wait_jobs(
                (!runs.is_empty()).then_some(runs.as_slice()),
                (!tasks.is_empty()).then_some(tasks.as_slice()),
            )?;
            let cancelled = token.is_cancelled();
            let timed_out = elapsed() >= TIMEOUT && jobs.iter().any(Job::pending);
            if cancelled || timed_out {
                self.stop_wait_jobs(&jobs)?;
                jobs = self.wait_jobs(
                    (!runs.is_empty()).then_some(runs.as_slice()),
                    (!tasks.is_empty()).then_some(tasks.as_slice()),
                )?;
            }
            let pending = jobs.iter().any(Job::pending);
            let mut result = json!({"ok":!timed_out && !cancelled,"results":[],"background_results":[],"jobs":jobs,
                "pending":pending,"timed_out":timed_out,"interrupted":false,"cancelled":cancelled});
            if !pending {
                if let Some(binding) = &self.subagent {
                    binding.check_lsp_cleanup()?;
                }
                let mut state = self.wait_state.borrow_mut();
                for job in jobs
                    .iter()
                    .filter(|j| j.r#type == "bash" && !j.pending() && !j.delivered)
                {
                    result["background_results"]
                        .as_array_mut()
                        .expect("结果数组")
                        .push(self.background_tasks.status(&job.id)?);
                }
                // 取消只发布最终展示，不消费结果；恢复后仍可交付已完成结果。
                if cancelled {
                    result["error"] = json!("Wait cancelled; owned work stopped.");
                    return Ok(result);
                }
                // 先完成所有可能失败的快照，再整体预留，避免部分成功吞掉结果。
                for entry in result["background_results"].as_array().expect("结果数组") {
                    state
                        .tasks
                        .insert(entry["task_id"].as_str().expect("后台 ID").into());
                }
                return Ok(result);
            }
            if elapsed() >= next_progress {
                emit(AgentEvent::ToolOutputSnapshot {
                    id: call_id.into(),
                    output: result.to_string(),
                });
                next_progress = elapsed() + PROGRESS;
            }
            thread::sleep(POLL.min(TIMEOUT.saturating_sub(elapsed())));
        }
    }

    /// 先通知子 Agent，再回收 Bash；即使其中一路失败，也继续尝试其余清理。
    fn stop_wait_jobs(&self, jobs: &[Job]) -> Result<()> {
        let runs = jobs
            .iter()
            .filter(|j| j.r#type == "task" && j.pending())
            .map(|j| j.id.clone())
            .collect::<Vec<_>>();
        let mut errors = Vec::new();
        if let Some(binding) = &self.subagent
            && let Err(error) = binding.cancel_runs(&runs)
        {
            errors.push(error);
        }
        for job in jobs.iter().filter(|j| j.r#type == "bash" && j.pending()) {
            if let Err(error) = self.background_tasks.stop(&job.id) {
                errors.push(error);
            }
        }
        if let Some(binding) = &self.subagent
            && let Err(error) = binding.reap_runs(&runs)
        {
            errors.push(error);
        }
        let mut errors = errors.into_iter();
        if let Some(mut error) = errors.next() {
            for additional in errors {
                error = error.context(format!("Additional wait cleanup failure: {additional:#}"));
            }
            return Err(CleanupError(error).into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod integration {
    use super::*;

    #[test]
    fn wait_fixed_timeout_is_hidden_from_schema_and_legacy_values_are_ignored() {
        assert_eq!(TIMEOUT, Duration::from_secs(7200));
        assert!(
            definition(WorkMode::Execute).parameters.unwrap()["properties"]
                .get("timeout_ms")
                .is_none()
        );
        assert!(
            definition(WorkMode::Plan).parameters.unwrap()["properties"]
                .get("task_ids")
                .is_none()
        );
        for value in [0, 1, 1_800_001, u64::MAX] {
            assert!(serde_json::from_value::<Args>(json!({"timeout_ms":value})).is_ok());
        }
        for value in [json!(-1), Value::Null, json!("0")] {
            assert!(serde_json::from_value::<Args>(json!({"timeout_ms":value})).is_err());
        }
    }

    #[test]
    fn wait_timeout_reaps_selected_bash_preserves_output_and_leaves_other_jobs_running() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let workflow = WorkflowState::default();
        let execute = |name: &str, args: Value| {
            let result = tools.execute(
                name,
                &args.to_string(),
                name,
                ToolPolicy::Execute {
                    workflow: &workflow,
                },
                &Default::default(),
                &mut |_| {},
            );
            assert!(result.stop_error.is_none());
            serde_json::from_str::<Value>(&result.output).unwrap()
        };
        let finished = execute(
            "bash",
            json!({"command":"printf completed-output", "mode":"background"}),
        );
        let selected = execute(
            "bash",
            json!({"command":"printf partial-output; while [ ! -f release ]; do sleep 0.01; done", "mode":"background"}),
        );
        let other = execute("bash", json!({"command":"sleep 30", "mode":"background"}));
        let finished_id = finished["task_id"].as_str().unwrap();
        let selected_id = selected["task_id"].as_str().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if tools.background_tasks.status(finished_id).unwrap()["status"] == "exited"
                && tools.background_tasks.status(selected_id).unwrap()["output"] == "partial-output"
            {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(POLL);
        }
        let result = tools
            .run_wait_with_elapsed(
                &json!({"task_ids":[finished_id,selected_id],"timeout_ms":u64::MAX}).to_string(),
                "timeout",
                &Default::default(),
                &mut |_| panic!("超时后不能继续发布运行快照"),
                || TIMEOUT,
            )
            .unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["pending"], false);
        assert_eq!(result["jobs"].as_array().unwrap().len(), 2);
        assert!(
            !result["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|j| j["status"] == "running")
        );
        let outputs = result["background_results"].as_array().unwrap();
        assert!(
            outputs
                .iter()
                .any(|r| r["output"] == "completed-output" && r["status"] == "exited")
        );
        let selected_result = outputs
            .iter()
            .find(|r| r["task_id"] == selected_id)
            .unwrap();
        assert_eq!(selected_result["status"], "stopped");
        // 合并的输出还可能包含停止解释器时写入 stderr 的诊断。
        assert!(
            selected_result["output"]
                .as_str()
                .unwrap()
                .starts_with("partial-output"),
            "{selected_result:?}"
        );
        assert_eq!(
            tools
                .background_tasks
                .status(other["task_id"].as_str().unwrap())
                .unwrap()["status"],
            "running"
        );
        tools.commit_wait_results().unwrap();
        assert_eq!(
            execute("wait", json!({"task_ids":[selected_id]}))["background_results"],
            json!([])
        );
        tools.stop_background_tasks().unwrap();
    }
}
