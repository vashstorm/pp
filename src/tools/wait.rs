//! 后台 Bash 与子 Agent 共用的等待屏障；进度只发布展示快照。
use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const MAX_MS: u64 = 1_800_000;
const PROGRESS: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(25);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    #[serde(default)]
    run_ids: Vec<String>,
    #[serde(default)]
    task_ids: Vec<String>,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}
fn default_timeout() -> u64 {
    MAX_MS
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wait_default_matches_the_schema_and_accepts_zero() {
        assert_eq!(
            serde_json::from_str::<Args>("{}").unwrap().timeout_ms,
            MAX_MS
        );
        assert_eq!(
            serde_json::from_str::<Args>(r#"{"timeout_ms":0}"#)
                .unwrap()
                .timeout_ms,
            0
        );
        assert_eq!(
            definition().parameters.unwrap()["properties"]["timeout_ms"]["maximum"],
            MAX_MS
        );
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Job {
    pub id: String,
    pub r#type: String,
    pub label: String,
    pub status: String,
    pub duration_ms: u64,
    pub delivered: bool,
}
impl Job {
    pub(crate) fn pending(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "running")
    }
}

#[derive(Default)]
pub(super) struct State {
    // 当前原生批次的预留记录，防止同一批多个 wait 返回同一份结果。
    runs: BTreeSet<String>,
    tasks: BTreeSet<String>,
    delivered_tasks: BTreeSet<String>,
}
impl State {
    pub(super) fn clear_background(&mut self) {
        self.tasks.clear();
        self.delivered_tasks.clear();
    }
}

pub(super) fn definition() -> FunctionObject {
    FunctionObject {
        name: "wait".into(),
        description: Some("Wait for the first settled owned background Bash task or subagent run, and return all newly settled results plus the remaining jobs. Optional run_ids/task_ids select targets; omit both to watch all undelivered owned work. timeout_ms defaults to 1800000 (30 minutes), accepts 0..=1800000; 0 reads a snapshot. Timeout or steering interrupts only the wait, not the jobs. Results are delivered once after context commit; use bash_task status or read for non-consuming inspection. Continue useful work before waiting. Running services may never exit: verify readiness with actual requests, then stop them explicitly.".into()),
        parameters: Some(json!({"type":"object","properties":{
            "run_ids":{"type":"array","items":{"type":"string","minLength":1}},
            "task_ids":{"type":"array","items":{"type":"string","minLength":1}},
            "timeout_ms":{"type":"integer","minimum":0,"maximum":MAX_MS}
        },"additionalProperties":false})),
        ..Default::default()
    }
}

impl Tools {
    pub(super) fn reserve_task_results(&self, value: &Value) {
        let mut state = self.wait_state.borrow_mut();
        for task in value["tasks"].as_array().into_iter().flatten() {
            for result in task["results"]["results"].as_array().into_iter().flatten() {
                if let Some(id) = result["run"]["id"].as_str() {
                    state.runs.insert(id.into());
                }
            }
        }
    }

    pub(crate) fn start_wait_batch(&self, input: Option<&InputQueue>) {
        *self.wait_input.borrow_mut() = input.cloned();
        let mut state = self.wait_state.borrow_mut();
        state.runs.clear();
        state.tasks.clear();
    }

    /// 只在原生工具批次成功同步后确认；写盘失败时预留记录仍然可重试。
    pub(crate) fn commit_wait_results(&self) -> Result<()> {
        let mut state = self.wait_state.borrow_mut();
        if !state.runs.is_empty()
            && let Some(binding) = &self.subagent
        {
            binding.acknowledge(&state.runs.iter().cloned().collect::<Vec<_>>())?;
        }
        state.runs.clear();
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
                state.runs.contains(&job.id)
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
        let args: Args = serde_json::from_str(arguments).context("invalid wait arguments")?;
        ensure!(
            args.timeout_ms <= MAX_MS,
            "timeout_ms must be in 0..=1800000"
        );
        ensure!(
            args.run_ids
                .iter()
                .chain(&args.task_ids)
                .all(|id| !id.trim().is_empty()),
            "wait IDs must not be blank"
        );
        let explicit = !args.run_ids.is_empty() || !args.task_ids.is_empty();
        // 所有 ID 在任何结果预留之前统一校验。
        let selected = self.wait_jobs(
            (!explicit || !args.run_ids.is_empty()).then_some(args.run_ids.as_slice()),
            (!explicit || !args.task_ids.is_empty()).then_some(args.task_ids.as_slice()),
        )?;
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
        let started = Instant::now();
        let mut next_progress = Duration::ZERO;
        loop {
            crate::cancellation::check(token)?;
            let jobs = self.wait_jobs(
                (!runs.is_empty()).then_some(runs.as_slice()),
                (!tasks.is_empty()).then_some(tasks.as_slice()),
            )?;
            let pending = jobs.iter().any(Job::pending);
            let ready = jobs.iter().any(|job| !job.pending() && !job.delivered);
            let interrupted = self
                .wait_input
                .borrow()
                .as_ref()
                .map(|q| q.has_steering())
                .transpose()?
                .unwrap_or(false);
            let timed_out = started.elapsed() >= Duration::from_millis(args.timeout_ms)
                && pending
                && !ready
                && !interrupted;
            let mut result = json!({"ok":true,"results":[],"background_results":[],"jobs":jobs,
                "pending":pending,"timed_out":timed_out,"interrupted":interrupted});
            if ready || !pending || timed_out || interrupted {
                let mut state = self.wait_state.borrow_mut();
                if let Some(binding) = &self.subagent {
                    let ready_runs = jobs
                        .iter()
                        .filter(|job| job.r#type == "task" && !job.pending() && !job.delivered)
                        .map(|job| job.id.clone())
                        .collect::<Vec<_>>();
                    let deliveries = if ready_runs.is_empty() {
                        Vec::new()
                    } else {
                        binding.deliveries(&ready_runs)
                    };
                    for run in deliveries
                        .into_iter()
                        .filter(|r| !state.runs.contains(&r.id))
                    {
                        result["results"]
                            .as_array_mut()
                            .expect("结果数组")
                            .push(json!({
                                "delivery_marker":format!("pp-subagent-result:{}",run.id),"run":run
                            }));
                    }
                }
                for job in jobs
                    .iter()
                    .filter(|j| j.r#type == "bash" && !j.pending() && !j.delivered)
                {
                    result["background_results"]
                        .as_array_mut()
                        .expect("结果数组")
                        .push(self.background_tasks.status(&job.id)?);
                }
                // 先完成所有可能失败的快照，再整体预留，避免部分成功吞掉结果。
                for entry in result["results"].as_array().expect("结果数组") {
                    state
                        .runs
                        .insert(entry["run"]["id"].as_str().expect("Run ID").into());
                }
                for entry in result["background_results"].as_array().expect("结果数组") {
                    state
                        .tasks
                        .insert(entry["task_id"].as_str().expect("后台 ID").into());
                }
                return Ok(result);
            }
            if started.elapsed() >= next_progress {
                emit(AgentEvent::ToolOutputSnapshot {
                    id: call_id.into(),
                    output: result.to_string(),
                });
                next_progress = started.elapsed() + PROGRESS;
            }
            thread::sleep(
                POLL.min(Duration::from_millis(args.timeout_ms).saturating_sub(started.elapsed())),
            );
        }
    }
}
