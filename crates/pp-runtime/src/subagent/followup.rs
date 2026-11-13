//! 追加消息（follow-up）与 `agent://` 控制动作。

use anyhow::{Context, Result, ensure};
use pp_core::cancellation::CancellationToken;
use serde_json::{Value, json};

use super::observe::HubEvent;
use super::{Binding, Record, Run, lock, now};

impl Binding {
    pub(super) fn followup(
        &self,
        record: &Record,
        text: &str,
        token: &CancellationToken,
    ) -> Result<Value> {
        ensure!(!text.trim().is_empty(), "message must not be empty");
        let mut state = lock(&self.inner.state);
        ensure!(
            !state.stopping,
            "Agent is shutting down; follow-up is temporarily unavailable"
        );
        if state
            .retiring
            .get(&record.id)
            .is_some_and(|generation| *generation == record.generation)
        {
            state.live.remove(&record.id);
            state.retiring.remove(&record.id);
        }
        if let Some(live) = state.live.get(&record.id).filter(|_| {
            state.disk.runs.values().any(|r| {
                r.agent_id == record.id && matches!(r.status.as_str(), "queued" | "running")
            })
        }) && live.input.steer_if_open(text.to_owned())?.is_some()
        {
            self.inner.changed.notify_all();
            return Ok(json!({"ok":true,"steering":true}));
        }
        ensure!(
            !state
                .disk
                .runs
                .values()
                .any(|r| r.agent_id == record.id
                    && matches!(r.status.as_str(), "queued" | "running")),
            "Agent is still committing the previous result; retry the follow-up later."
        );
        state.disk.next_id += 1;
        let run = format!("r{}", state.disk.next_id);
        state.disk.runs.insert(
            run.clone(),
            Run {
                id: run.clone(),
                agent_id: record.id.clone(),
                owner: self.owner.clone(),
                status: "queued".into(),
                task: text.into(),
                result: None,
                consumed: false,
                duration_ms: 0,
                started_ms: now(),
                requests: 0,
                tokens: 0,
                cost: None,
            },
        );
        let generation = state
            .disk
            .agents
            .get(&record.id)
            .map_or(record.generation, |current| current.generation);
        self.inner.save(&state)?;
        drop(state);
        self.inner.observe(
            &record.id,
            Some(&run),
            generation,
            None,
            None,
            Some(HubEvent::RunState {
                status: "queued".into(),
            }),
        );
        if let Err(error) = self.launch(record.clone(), &run, text.into(), token) {
            self.settle(&run, json!({"ok":false,"error":format!("{error:#}")}), 0)?;
            return Err(error);
        }
        Ok(json!({"ok":true,"run_id":run,"wait_args":{"run_ids":[run]}}))
    }
    pub fn control(&self, id: &str, action: &str, content: Option<&str>) -> Result<Value> {
        if action == "revive" {
            let record = lock(&self.inner.state)
                .disk
                .agents
                .get(id)
                .cloned()
                .context("unknown agent")?;
            self.launch(record, "", String::new(), &CancellationToken::new())?;
            return Ok(json!({"ok":true,"revived":true}));
        }
        if action == "send" {
            let record = lock(&self.inner.state)
                .disk
                .agents
                .get(id)
                .cloned()
                .context("unknown agent")?;
            return self.followup(
                &record,
                content.unwrap_or(
                    "Continue from your saved context. Do not repeat completed side effects.",
                ),
                &CancellationToken::new(),
            );
        }
        let mut state = lock(&self.inner.state);
        let agent = state.disk.agents.get_mut(id).context("unknown agent")?;
        ensure!(
            matches!(action, "cancel" | "kill"),
            "unknown control action"
        );
        if action == "kill" {
            agent.status = "aborted".into();
            agent.generation += 1;
        }
        let worker_generation = agent.generation.saturating_sub(u64::from(action == "kill"));
        let killed = if action == "kill" {
            if let Some(live) = state.live.remove(id) {
                state.retiring.insert(id.to_owned(), worker_generation);
                live.cancellation.cancel();
            }
            true
        } else {
            if let Some(live) = state.live.get(id) {
                live.cancellation.cancel();
            }
            false
        };
        let generation = state
            .disk
            .agents
            .get(id)
            .map_or(0, |record| record.generation);
        self.inner.save(&state)?;
        self.inner.changed.notify_all();
        drop(state);
        if killed {
            self.inner.observe(
                id,
                None,
                generation,
                None,
                None,
                Some(HubEvent::AgentState {
                    status: "aborted".into(),
                }),
            );
        }
        Ok(json!({"ok":true}))
    }
}
