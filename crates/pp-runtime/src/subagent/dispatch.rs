//! 批次派发、worker 启动与 run 结算。

use anyhow::{Context, Result, ensure};
use pp_config::config::ModelRole;
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::model::ThinkLevel;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, mpsc},
    thread,
};

use crate::agent::input::InputQueue;

use super::catalog;
use super::observe::HubEvent;
use super::task_tool::Batch;
use super::worker::worker;
use super::{
    Binding, Live, Record, Run, Spec, Work, is_read_only_child_tool, lock, now, validate_schema,
};

impl Binding {
    pub(super) fn spawn(
        &self,
        batch: Batch,
        plan: bool,
        token: &CancellationToken,
    ) -> Result<Value> {
        ensure!(
            self.depth == 0
                && self.depth < self.inner.settings.max_recursion_depth
                && !self.spawns.is_empty()
                && !self.readonly,
            "this agent is not allowed to spawn further subagents"
        );
        ensure!(
            !batch.tasks.is_empty() && batch.tasks.len() <= 32,
            "tasks count must be in 1..=32"
        );
        // 整批使用同一份可用角色快照；旧 prompt 或手写调用也必须通过校验。
        let roles = self.model_roles();
        let definitions = self.custom_roles();
        let available = self
            .available_roles(&roles, &definitions)
            .map(|(def, role)| (def.name.as_str(), (def, role)))
            .collect::<BTreeMap<_, _>>();
        let custom_default = roles.get(&ModelRole::Default).filter(|role| role.enabled);
        let mut names = BTreeSet::new();
        let mut prepared = Vec::new();
        for item in batch.tasks {
            cancellation::check(token)?;
            let role = item.role.as_str();
            ensure!(
                !matches!(role, "default" | "plan"),
                "DEFAULT and PLAN are reserved for the main agent"
            );
            let &(def, model_role) = available.get(role).with_context(|| {
                if let Some((role, _)) = catalog::task_roles().find(|(_, def)| def.name == role)
                    && !roles
                        .get(&role)
                        .is_some_and(|assignment| assignment.enabled)
                {
                    return format!(
                        "model role {} is not selected; enable it in /Roles first",
                        role.name()
                    );
                }
                "unknown or disabled task role; select a listed role".to_owned()
            })?;
            let assignment = model_role.and_then(|role| roles.get(&role));
            let (model, thinking) = if let Some(assignment) = assignment {
                (assignment.model.clone(), assignment.effort)
            } else {
                let model = self
                    .inner
                    .settings
                    .agent_models
                    .get(&def.name)
                    .or(def.model.as_ref())
                    .or_else(|| custom_default.map(|default| &default.model))
                    .context("subagent model is required")?;
                let thinking = def
                    .thinking
                    .or_else(|| custom_default.map(|default| default.effort))
                    .unwrap_or(ThinkLevel::None);
                (model.clone(), thinking)
            };
            ensure!(
                !item.task.trim().is_empty(),
                "subagent task must not be empty"
            );
            if let Some(name) = &item.name {
                ensure!(
                    !name.trim().is_empty() && names.insert(name.to_lowercase()),
                    "duplicate or empty agent name"
                );
            }
            ensure!(
                !plan || !item.isolated,
                "plan mode does not allow workspace isolation"
            );
            let output = item.output_schema.clone().or_else(|| def.output.clone());
            if let Some(schema) = &output {
                validate_schema(schema)?;
            }
            let mcp_tools = lock(&self.inner.mcp_tools);
            let readonly = plan
                || item.readonly
                || def.readonly
                || def.tools.as_ref().is_some_and(|tools| {
                    tools.iter().all(|name| {
                        is_read_only_child_tool(name, |name| {
                            mcp_tools.read_only.iter().any(|tool| tool == name)
                        })
                    })
                });
            let configured_tools = def.tools.as_ref();
            let mut tools = configured_tools.cloned().unwrap_or_else(|| {
                [
                    "read",
                    "rg",
                    "glob",
                    "lsp",
                    "edit",
                    "write",
                    "bash",
                    "bash_task",
                    "ask",
                    "wait",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
            });
            if configured_tools.is_none() {
                let mcp_names = if readonly {
                    &mcp_tools.read_only
                } else {
                    &mcp_tools.all
                };
                tools.extend(mcp_names.iter().cloned());
            }
            if readonly {
                tools.retain(|name| {
                    is_read_only_child_tool(name, |name| {
                        mcp_tools.read_only.iter().any(|tool| tool == name)
                    })
                });
            }
            drop(mcp_tools);
            tools.retain(|t| t != "task" && t != "todo" && !t.starts_with("plan_"));
            let prompt = if model_role.is_some() && readonly && !def.readonly {
                include_str!("../prompts/roles/read-only.md").trim_end()
            } else {
                &def.prompt
            };
            let mut spec = Spec {
                model_role,
                role_route: model_role.zip(assignment).map(|(role, assignment)| {
                    pp_config::config::RoleRoute {
                        role,
                        primary: pp_config::config::RoleModel {
                            model: model.clone(),
                            effort: thinking,
                        },
                        fallback: assignment.fallback.clone(),
                    }
                }),
                definition: role.to_owned(),
                prompt: prompt.to_owned(),
                context: batch.context.clone(),
                model,
                thinking,
                tools,
                spawns: vec![],
                output_schema: output,
                cwd: self.cwd.clone(),
                depth: self.depth + 1,
                readonly,
                isolated: item.isolated,
                cost: None,
            };
            (self.inner.validate)(&mut spec)?;
            prepared.push((item, spec, def.blocking));
        }
        // 预检模型与能力在任何 worker 启动前完成。
        let mut records = Vec::new();
        {
            let mut state = lock(&self.inner.state);
            ensure!(!state.stopping, "subagent supervisor is stopping");
            for (item, spec, blocking) in prepared {
                state.disk.next_id += 1;
                let id = format!("a{}", state.disk.next_id);
                let run_id = format!("r{}", state.disk.next_id);
                let registered_ms = now();
                let record = Record {
                    id: id.clone(),
                    name: item.name.unwrap_or_else(|| spec.definition.clone()),
                    parent: self.owner.clone(),
                    generation: 1,
                    status: "idle".into(),
                    spec,
                    session_file: None,
                    registered_ms: Some(registered_ms),
                    updated_ms: registered_ms,
                };
                state.disk.agents.insert(id.clone(), record.clone());
                state.disk.runs.insert(
                    run_id.clone(),
                    Run {
                        id: run_id.clone(),
                        agent_id: id,
                        owner: self.owner.clone(),
                        status: "queued".into(),
                        task: item.task.clone(),
                        result: None,
                        consumed: false,
                        duration_ms: 0,
                        started_ms: now(),
                        requests: 0,
                        tokens: 0,
                        cost: None,
                    },
                );
                records.push((record, run_id, item.task, blocking));
            }
            self.inner.save(&state)?;
        }
        for (record, run_id, _, _) in &records {
            self.inner.observe(
                &record.id,
                Some(run_id),
                record.generation,
                None,
                None,
                Some(HubEvent::RunState {
                    status: "queued".into(),
                }),
            );
        }
        let mut results = Vec::new();
        for (record, run_id, task, blocking) in records {
            let result = self.launch(record, &run_id, task, token);
            if let Err(error) = result {
                self.settle(&run_id, json!({"ok":false,"error":format!("{error:#}")}), 0)?;
            }
            let agent_id = lock(&self.inner.state).disk.runs[&run_id].agent_id.clone();
            let value = json!({"agent_id":agent_id,"run_id":run_id,"status":"queued","output_uri":format!("agent://{agent_id}"),"blocking":blocking});
            results.push(value);
        }
        for value in &mut results {
            if value["blocking"] == true {
                let id = value["run_id"]
                    .as_str()
                    .context("missing run ID")?
                    .to_owned();
                value["results"] = self.wait(&[id], u64::MAX, token)?;
            }
        }
        let run_ids = results
            .iter()
            .map(|task| task["run_id"].clone())
            .collect::<Vec<_>>();
        Ok(json!({"ok":true,"tasks":results,"wait_args":{"run_ids":run_ids}}))
    }
    pub(super) fn launch(
        &self,
        mut record: Record,
        run: &str,
        task: String,
        parent_token: &CancellationToken,
    ) -> Result<()> {
        ensure!(
            !self.inner.plan_mode || !record.spec.isolated,
            "Plan mode can only view existing isolated sessions; isolated runs cannot be resumed."
        );
        let token = parent_token.child_token();
        let mut state = lock(&self.inner.state);
        ensure!(
            !state.stopping,
            "Agent is shutting down; resume is temporarily unavailable"
        );
        // 已决定退出的 worker 仍可能持有 Live，但它不再读取队列；视同已停泊，
        // 由新 worker 接管，避免新任务被排进无人消费的通道。
        if state
            .retiring
            .get(&record.id)
            .is_some_and(|generation| *generation == record.generation)
        {
            state.live.remove(&record.id);
            state.retiring.remove(&record.id);
        }
        if state.live.contains_key(&record.id) {
            if run.is_empty() {
                return Ok(());
            }
            state
                .disk
                .agents
                .get_mut(&record.id)
                .context("unknown agent")?
                .status = "queued".into();
            self.inner.save(&state)?;
            self.inner.observe(
                &record.id,
                None,
                record.generation,
                None,
                None,
                Some(HubEvent::AgentState {
                    status: "queued".into(),
                }),
            );
            let live = state
                .live
                .get_mut(&record.id)
                .context("missing live agent")?;
            live.cancellation = token.clone();
            live.input.begin_run()?;
            live.sender.send(Work {
                accounting: lock(&self.inner.accounting).clone(),
                run: run.into(),
                task,
                cancellation: token,
            })?;
            return Ok(());
        }
        record.generation += 1;
        record.status = if run.is_empty() { "idle" } else { "queued" }.into();
        let generation = record.generation;
        let status = record.status.clone();
        state.disk.agents.insert(record.id.clone(), record.clone());
        let input = InputQueue::default();
        input.begin_run()?;
        let (sender, receiver) = mpsc::channel();
        state.live.insert(
            record.id.clone(),
            Live {
                sender: sender.clone(),
                input: input.clone(),
                cancellation: token.clone(),
                generation,
            },
        );
        self.inner.save(&state)?;
        state.workers += 1;
        let weak = Arc::downgrade(&self.inner);
        let id = record.id.clone();
        if let Err(error) = thread::Builder::new()
            .name(format!("pp-{id}"))
            .spawn(move || worker(weak, record, generation, input, receiver))
        {
            state.workers = state.workers.saturating_sub(1);
            state.live.remove(&id);
            return Err(error.into());
        }
        // 在唤醒 worker 前发布排队状态，避免迟到的 queued 覆盖 running。
        self.inner.observe(
            &id,
            None,
            generation,
            None,
            None,
            Some(HubEvent::AgentState { status }),
        );
        if !run.is_empty() {
            sender.send(Work {
                accounting: lock(&self.inner.accounting).clone(),
                run: run.into(),
                task,
                cancellation: token,
            })?;
        }
        Ok(())
    }
    pub(super) fn settle(&self, run: &str, result: Value, duration_ms: u64) -> Result<()> {
        let mut state = lock(&self.inner.state);
        let r = state.disk.runs.get_mut(run).context("unknown run")?;
        r.status = if result["cancelled"] == true {
            "cancelled"
        } else if result["ok"] == true {
            "completed"
        } else {
            "failed"
        }
        .into();
        r.result = Some(result);
        r.duration_ms = duration_ms;
        let agent_id = r.agent_id.clone();
        let status = r.status.clone();
        let generation = state
            .disk
            .agents
            .get(&agent_id)
            .map_or(0, |record| record.generation);
        if let Some(record) = state.disk.agents.get_mut(&agent_id) {
            record.status = "idle".into();
            record.updated_ms = now();
        }
        self.inner.save(&state)?;
        self.inner.observe(
            &agent_id,
            Some(run),
            generation,
            None,
            None,
            Some(HubEvent::RunState { status }),
        );
        self.inner.observe(
            &agent_id,
            None,
            generation,
            None,
            None,
            Some(HubEvent::AgentState {
                status: "idle".into(),
            }),
        );
        self.inner.changed.notify_all();
        Ok(())
    }
}
