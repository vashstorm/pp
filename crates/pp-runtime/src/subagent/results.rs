//! 结果投递、等待与确认。

use anyhow::{Result, ensure};
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

use super::{Binding, Run, lock, now};

impl Binding {
    pub(crate) fn pending(&self) -> bool {
        lock(&self.inner.state).disk.runs.values().any(|r| {
            r.owner == self.owner
                && (!r.consumed || matches!(r.status.as_str(), "queued" | "running"))
        })
    }
    pub fn deliveries(&self, ids: &[String]) -> Vec<Run> {
        lock(&self.inner.state)
            .disk
            .runs
            .values()
            .filter(|r| {
                r.owner == self.owner
                    && !r.consumed
                    && r.result.is_some()
                    && (ids.is_empty() || ids.contains(&r.id))
            })
            .cloned()
            .collect()
    }
    pub(crate) fn wait_jobs(&self, ids: &[String]) -> Result<Vec<crate::tools::wait::Job>> {
        let state = lock(&self.inner.state);
        for id in ids {
            ensure!(
                state
                    .disk
                    .runs
                    .get(id)
                    .is_some_and(|r| r.owner == self.owner),
                "run does not belong to this agent"
            );
        }
        Ok(state
            .disk
            .runs
            .values()
            .filter(|r| r.owner == self.owner && (ids.is_empty() || ids.contains(&r.id)))
            .map(|r| crate::tools::wait::Job {
                id: r.id.clone(),
                r#type: "task".into(),
                label: r.task.chars().take(200).collect(),
                status: r.status.clone(),
                delivered: r.consumed,
                duration_ms: if matches!(r.status.as_str(), "queued" | "running")
                    && r.started_ms > 0
                {
                    now().saturating_sub(r.started_ms)
                } else {
                    r.duration_ms
                },
            })
            .collect())
    }
    pub(crate) fn acknowledge(&self, ids: &[String]) -> Result<()> {
        let mut state = lock(&self.inner.state);
        let mut previous = Vec::new();
        for id in ids {
            if let Some(run) = state
                .disk
                .runs
                .get_mut(id)
                .filter(|r| r.owner == self.owner)
            {
                previous.push((id.clone(), run.consumed));
                run.consumed = true;
            }
        }
        if let Err(error) = self.inner.save(&state) {
            for (id, consumed) in previous {
                state.disk.runs.get_mut(&id).expect("已校验的 Run").consumed = consumed;
            }
            return Err(error);
        }
        Ok(())
    }
    pub(crate) fn wait(
        &self,
        ids: &[String],
        timeout_ms: u64,
        token: &CancellationToken,
    ) -> Result<Value> {
        {
            let state = lock(&self.inner.state);
            for id in ids {
                ensure!(
                    state
                        .disk
                        .runs
                        .get(id)
                        .is_some_and(|r| r.owner == self.owner),
                    "run does not belong to this agent"
                );
            }
        }
        let start = Instant::now();
        loop {
            cancellation::check(token)?;
            let pending = {
                let state = lock(&self.inner.state);
                state.disk.runs.values().any(|r| {
                    r.owner == self.owner
                        && (ids.is_empty() || ids.contains(&r.id))
                        && matches!(r.status.as_str(), "queued" | "running")
                })
            };
            let results = self.deliveries(ids);
            if !results.is_empty()
                || !pending
                || start.elapsed().as_millis() >= u128::from(timeout_ms)
            {
                let results=results.iter().map(|r|json!({"delivery_marker":format!("pp-subagent-result:{}",r.id),"run":r})).collect::<Vec<_>>();
                return Ok(json!({"ok":true,"results":results,"pending":pending}));
            }
            self.sleep();
        }
    }
    pub(crate) fn sleep(&self) {
        let state = lock(&self.inner.state);
        drop(
            self.inner
                .changed
                .wait_timeout(state, Duration::from_millis(25))
                .unwrap_or_else(|e| e.into_inner()),
        );
    }
}
