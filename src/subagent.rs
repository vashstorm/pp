//! 子 Agent 的身份、调度、结果投递与持久生命周期。
mod catalog;
#[cfg(test)]
mod tests;
mod workspace;

use crate::{
    agent::{Agent, input::InputQueue},
    cancellation::{self, CancellationToken},
    config::ThinkLevel,
    events::AgentEvent,
    plan::WorkMode,
};
use anyhow::{Context, Result, bail, ensure};
use async_openai::types::chat::FunctionObject;
pub(crate) use catalog::{Definition, discover as discover_roles};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, Weak, mpsc},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Settings {
    pub max_concurrency: usize,
    pub max_recursion_depth: usize,
    pub idle_ttl_ms: u64,
    pub request_budget: usize,
    pub disabled_agents: Vec<String>,
    pub agent_models: BTreeMap<String, String>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            max_concurrency: 4,
            max_recursion_depth: 2,
            idle_ttl_ms: 420_000,
            request_budget: 200,
            disabled_agents: vec![],
            agent_models: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Spec {
    pub definition: String,
    pub prompt: String,
    pub context: String,
    pub model: String,
    pub thinking: ThinkLevel,
    pub tools: Vec<String>,
    pub spawns: Vec<String>,
    pub output_schema: Option<Value>,
    pub cwd: PathBuf,
    pub depth: usize,
    pub readonly: bool,
    pub isolated: bool,
    #[serde(default)]
    pub cost: Option<crate::models::ModelCost>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub id: String,
    pub name: String,
    pub parent: String,
    pub generation: u64,
    pub status: String,
    pub spec: Spec,
    pub session_file: Option<PathBuf>,
    pub updated_ms: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Run {
    pub id: String,
    pub agent_id: String,
    pub owner: String,
    pub status: String,
    pub task: String,
    pub result: Option<Value>,
    pub consumed: bool,
    pub duration_ms: u64,
    pub requests: usize,
    pub tokens: u64,
    #[serde(default)]
    pub cost: Option<f64>,
}
#[derive(Default, Serialize, Deserialize)]
struct Disk {
    version: u32,
    agents: BTreeMap<String, Record>,
    runs: BTreeMap<String, Run>,
    next_id: u64,
}
struct Live {
    sender: mpsc::Sender<Work>,
    input: InputQueue,
    cancellation: CancellationToken,
}
struct State {
    disk: Disk,
    live: BTreeMap<String, Live>,
    active: usize,
    workers: usize,
    retiring: BTreeSet<String>,
    permits: std::collections::VecDeque<u64>,
    next_permit: u64,
    stopping: bool,
}
type Validator = dyn Fn(&mut Spec) -> Result<()> + Send + Sync;
type Factory = dyn Fn(&Record, Binding) -> Result<Agent> + Send + Sync;
#[derive(Clone)]
struct ChangeSink {
    root: PathBuf,
    tracker: Arc<Mutex<crate::tools::changes::TaskChanges>>,
    serial: Arc<Mutex<()>>,
}
struct Inner {
    state: Mutex<State>,
    changed: Condvar,
    settings: Settings,
    dir: Mutex<PathBuf>,
    user: PathBuf,
    factory: Mutex<Option<Arc<Factory>>>,
    validate: Arc<Validator>,
    changes: Mutex<Option<ChangeSink>>,
    mcp_names: Mutex<Vec<String>>,
    plan_mode: bool,
}
#[derive(Clone)]
pub(crate) struct Binding {
    inner: Arc<Inner>,
    pub owner: String,
    pub depth: usize,
    pub spawns: Vec<String>,
    pub model: String,
    pub thinking: ThinkLevel,
    pub cwd: PathBuf,
    pub readonly: bool,
}
struct Work {
    run: String,
    task: String,
    cancellation: CancellationToken,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let parent = path.parent().context("缺少持久目录")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut tmp, value)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
impl Inner {
    fn directory(&self) -> PathBuf {
        lock(&self.dir).clone()
    }
    fn save(&self, state: &State) -> Result<()> {
        atomic_json(&self.directory().join("registry.json"), &state.disk)
    }
}
pub(crate) struct RootOptions {
    pub dir: PathBuf,
    pub user: PathBuf,
    pub cwd: PathBuf,
    pub model: String,
    pub thinking: ThinkLevel,
    pub settings: Settings,
    pub plan_mode: bool,
}
impl Binding {
    pub(crate) fn root(
        options: RootOptions,
        factory: Arc<Factory>,
        validate: Arc<Validator>,
    ) -> Result<Self> {
        let RootOptions {
            dir,
            user,
            cwd,
            model,
            thinking,
            settings,
            plan_mode,
        } = options;
        ensure!(
            (1..=32).contains(&settings.max_concurrency),
            "task.max_concurrency 必须在 1..=32 内"
        );
        ensure!(
            settings.max_recursion_depth <= 8,
            "task.max_recursion_depth 不得超过 8"
        );
        let mut disk: Disk = if dir
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("pending-"))
        {
            Disk::default()
        } else {
            match fs::read(dir.join("registry.json")) {
                Ok(bytes) => serde_json::from_slice(&bytes)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Disk::default(),
                Err(error) => return Err(error.into()),
            }
        };
        ensure!(disk.version <= 1, "子 Agent 存储版本不可识别");
        disk.version = 1;
        for agent in disk.agents.values_mut() {
            if agent.status != "aborted" {
                agent.status = "parked".into();
            }
        }
        for run in disk.runs.values_mut() {
            ensure!(
                matches!(
                    run.status.as_str(),
                    "queued" | "running" | "completed" | "failed" | "cancelled" | "interrupted"
                ),
                "未知的子任务状态"
            );
            if matches!(run.status.as_str(), "queued" | "running") {
                match fs::read(dir.join(format!("{}.result.json", run.id))) {
                    Ok(bytes) => {
                        let result: Value = serde_json::from_slice(&bytes)?;
                        ensure!(result["ok"].is_boolean(), "无效子任务结果文件");
                        run.status = if result["cancelled"] == true {
                            "cancelled"
                        } else if result["ok"] == true {
                            "completed"
                        } else {
                            "failed"
                        }
                        .into();
                        run.result = Some(result);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        run.status = "interrupted".into();
                        run.result = Some(
                            json!({"ok":false,"error":"进程中断；需要显式追问以继续，不会重放工具。"}),
                        );
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            ensure!(
                matches!(run.status.as_str(), "queued" | "running") || run.result.is_some(),
                "终态子任务缺少结果"
            );
        }
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                disk,
                live: BTreeMap::new(),
                active: 0,
                workers: 0,
                retiring: Default::default(),
                permits: Default::default(),
                next_permit: 0,
                stopping: false,
            }),
            changed: Condvar::new(),
            settings,
            dir: Mutex::new(dir),
            user,
            factory: Mutex::new(Some(factory)),
            validate,
            changes: Mutex::new(None),
            mcp_names: Mutex::new(Vec::new()),
            plan_mode,
        });
        Ok(Self {
            inner,
            owner: "Main".into(),
            depth: 0,
            spawns: vec!["*".into()],
            model,
            thinking,
            cwd,
            readonly: false,
        })
    }
    pub(crate) fn plan_mode(&self) -> bool {
        self.inner.plan_mode
    }
    pub(crate) fn allow_mcp(&self, names: Vec<String>) {
        *lock(&self.inner.mcp_names) = names;
    }
    pub(crate) fn track_changes(
        &self,
        root: PathBuf,
        tracker: Arc<Mutex<crate::tools::changes::TaskChanges>>,
        serial: Arc<Mutex<()>>,
    ) {
        *lock(&self.inner.changes) = Some(ChangeSink {
            root,
            tracker,
            serial,
        });
    }
    pub(crate) fn directory(&self) -> PathBuf {
        self.inner.directory()
    }
    pub(crate) fn bind_session(&self, dir: &Path, id: &str) {
        if self.depth == 0 {
            let state = lock(&self.inner.state);
            if state.disk.agents.is_empty() {
                *lock(&self.inner.dir) = dir.join(format!("{id}.subagents"));
            }
        }
    }
    pub(crate) fn roles(&self) -> Result<Vec<Definition>> {
        catalog::discover(&self.cwd, &self.inner.user, &self.inner.settings)
    }
    pub(crate) fn definitions(&self, _mode: WorkMode) -> Vec<FunctionObject> {
        let mut defs = vec![function(
            "wait",
            "Wait for owned subagent runs. Results are consumed once; inspection does not consume results.",
            json!({"run_ids":{"type":"array","items":{"type":"string"}},"timeout_ms":{"type":"integer","minimum":0,"maximum":60000}}),
            &[],
        )];
        if self.depth < self.inner.settings.max_recursion_depth
            && !self.spawns.is_empty()
            && !self.readonly
        {
            let roles = self
                .roles()
                .unwrap_or_default()
                .iter()
                .map(|d| format!("{}: {}", d.name, d.description))
                .collect::<Vec<_>>()
                .join("; ");
            let mut def = function(
                "task",
                &format!(
                    "Delegate independent tasks with explicit shared context. Tasks run asynchronously unless their role is blocking. Final results automatically return to you; use wait when blocked. Available roles: {roles}"
                ),
                json!({"context":{"type":"string"},"tasks":{"type":"array","minItems":1,"maxItems":32,"items":{"type":"object","properties":{"name":{"type":"string"},"agent":{"type":"string"},"task":{"type":"string"},"isolated":{"type":"boolean"},"output_schema":{},"model":{"type":"string"}},"required":["task"],"additionalProperties":false}}}),
                &["context", "tasks"],
            );
            // 模型选择由角色定义控制，不开放单次任意模型覆盖。
            if let Some(properties) = def
                .parameters
                .as_mut()
                .and_then(|v| v.pointer_mut("/properties/tasks/items/properties"))
                .and_then(Value::as_object_mut)
            {
                properties.remove("model");
            }
            defs.push(def);
        }
        defs
    }
    pub(crate) fn execute(
        &self,
        name: &str,
        arguments: &str,
        plan: bool,
        token: &CancellationToken,
    ) -> Result<Value> {
        match name {
            "task" => {
                let args: Batch = serde_json::from_str(arguments)?;
                self.spawn(args, plan, token)
            }
            "wait" => {
                let args: Wait = serde_json::from_str(arguments)?;
                self.wait(&args.run_ids, args.timeout_ms.min(60_000), token)
            }
            _ => bail!("未知子 Agent 工具"),
        }
    }
    fn spawn(&self, batch: Batch, plan: bool, token: &CancellationToken) -> Result<Value> {
        ensure!(
            self.depth < self.inner.settings.max_recursion_depth
                && !self.spawns.is_empty()
                && !self.readonly,
            "此 Agent 不允许继续派发"
        );
        ensure!(
            !batch.tasks.is_empty() && batch.tasks.len() <= 32,
            "tasks 数量必须为 1..=32"
        );
        let roles = self.roles()?;
        let mut names = BTreeSet::new();
        let mut prepared = Vec::new();
        for item in batch.tasks {
            cancellation::check(token)?;
            let role = item.agent.as_deref().unwrap_or("task");
            ensure!(
                self.spawns.iter().any(|n| n == "*" || n == role),
                "父角色不允许派发 {role}"
            );
            let def = roles
                .iter()
                .find(|d| d.name == role)
                .context("未知或已禁用的 Agent 角色")?;
            ensure!(!item.task.trim().is_empty(), "子任务不能为空");
            if let Some(name) = &item.name {
                ensure!(
                    !name.trim().is_empty() && names.insert(name.to_lowercase()),
                    "重复或空 Agent 名称"
                );
            }
            ensure!(!plan || !item.isolated, "Plan 不允许工作区隔离");
            let output = item.output_schema.clone().or_else(|| def.output.clone());
            if let Some(schema) = &output {
                validate_schema(schema)?;
            }
            let mut tools = def.tools.clone().unwrap_or_else(|| {
                vec![
                    "read",
                    "grep",
                    "glob",
                    "edit",
                    "write",
                    "bash",
                    "bash_task",
                    "ask",
                    "task",
                    "wait",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect()
            });
            if def.tools.is_none() && !plan {
                tools.extend(lock(&self.inner.mcp_names).iter().cloned());
            }
            if plan {
                tools.retain(|t| matches!(t.as_str(), "read" | "grep" | "glob"));
            }
            if self.depth > 0 {
                let state = lock(&self.inner.state);
                if let Some(parent) = state.disk.agents.get(&self.owner) {
                    tools.retain(|t| parent.spec.tools.contains(t));
                }
            }
            tools.retain(|t| t != "todo" && !t.starts_with("plan_"));
            let mut spec = Spec {
                definition: role.into(),
                prompt: def.prompt.clone(),
                context: batch.context.clone(),
                model: self
                    .inner
                    .settings
                    .agent_models
                    .get(role)
                    .cloned()
                    .or_else(|| def.model.clone())
                    .unwrap_or_else(|| self.model.clone()),
                thinking: def.thinking.unwrap_or(self.thinking),
                tools,
                spawns: if plan {
                    vec![]
                } else {
                    def.spawns.clone().unwrap_or_default()
                },
                output_schema: output,
                cwd: self.cwd.clone(),
                depth: self.depth + 1,
                readonly: plan,
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
            ensure!(!state.stopping, "子 Agent supervisor 正在停止");
            for (item, spec, blocking) in prepared {
                state.disk.next_id += 1;
                let id = format!("a{}", state.disk.next_id);
                let run_id = format!("r{}", state.disk.next_id);
                let record = Record {
                    id: id.clone(),
                    name: item.name.unwrap_or_else(|| spec.definition.clone()),
                    parent: self.owner.clone(),
                    generation: 1,
                    status: "idle".into(),
                    spec,
                    session_file: None,
                    updated_ms: now(),
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
                        requests: 0,
                        tokens: 0,
                        cost: None,
                    },
                );
                records.push((record, run_id, item.task, blocking));
            }
            self.inner.save(&state)?;
        }
        let mut results = Vec::new();
        for (record, run_id, task, blocking) in records {
            let result = self.launch(record, &run_id, task, token);
            if let Err(error) = result {
                self.settle(&run_id, json!({"ok":false,"error":format!("{error:#}")}), 0)?;
            }
            let agent_id = lock(&self.inner.state).disk.runs[&run_id].agent_id.clone();
            let mut value = json!({"agent_id":agent_id,"run_id":run_id,"status":"queued","output_uri":format!("agent://{agent_id}")});
            value["blocking"] = json!(blocking);
            results.push(value);
        }
        for value in &mut results {
            if value["blocking"] == true {
                let id = value["run_id"].as_str().context("缺少 Run ID")?.to_owned();
                value["results"] = self.wait(&[id], u64::MAX, token)?;
            }
        }
        Ok(json!({"ok":true,"tasks":results}))
    }
    fn launch(
        &self,
        mut record: Record,
        run: &str,
        task: String,
        parent_token: &CancellationToken,
    ) -> Result<()> {
        ensure!(
            !self.inner.plan_mode || !record.spec.isolated,
            "Plan 模式只能查看已有隔离会话，不能恢复隔离运行。"
        );
        let token = parent_token.child_token();
        let mut state = lock(&self.inner.state);
        ensure!(
            !state.stopping && !state.retiring.contains(&record.id),
            "Agent 正在清理，暂时不能恢复"
        );
        if let Some(live) = state.live.get_mut(&record.id) {
            if run.is_empty() {
                return Ok(());
            }
            live.cancellation = token.clone();
            live.input.begin_run()?;
            live.sender.send(Work {
                run: run.into(),
                task,
                cancellation: token,
            })?;
            return Ok(());
        }
        record.generation += 1;
        record.status = if run.is_empty() { "idle" } else { "running" }.into();
        let generation = record.generation;
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
            },
        );
        self.inner.save(&state)?;
        state.workers += 1;
        let weak = Arc::downgrade(&self.inner);
        let id = record.id.clone();
        let startup_token = token.clone();
        if let Err(error) = thread::Builder::new()
            .name(format!("pp-{id}"))
            .spawn(move || worker(weak, record, generation, input, receiver, startup_token))
        {
            state.workers = state.workers.saturating_sub(1);
            state.live.remove(&id);
            return Err(error.into());
        }
        if !run.is_empty() {
            sender.send(Work {
                run: run.into(),
                task,
                cancellation: token,
            })?;
        }
        Ok(())
    }
    fn settle(&self, run: &str, result: Value, duration_ms: u64) -> Result<()> {
        let mut state = lock(&self.inner.state);
        let r = state.disk.runs.get_mut(run).context("未知 Run")?;
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
        self.inner.save(&state)?;
        self.inner.changed.notify_all();
        Ok(())
    }
    pub(crate) fn pending(&self) -> bool {
        lock(&self.inner.state).disk.runs.values().any(|r| {
            r.owner == self.owner
                && (!r.consumed || matches!(r.status.as_str(), "queued" | "running"))
        })
    }
    pub(crate) fn deliveries(&self, ids: &[String]) -> Vec<Run> {
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
    pub(crate) fn acknowledge(&self, ids: &[String]) -> Result<()> {
        let mut state = lock(&self.inner.state);
        for id in ids {
            if let Some(run) = state
                .disk
                .runs
                .get_mut(id)
                .filter(|r| r.owner == self.owner)
            {
                run.consumed = true;
            }
        }
        self.inner.save(&state)
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
                    "Run 不属于当前 Agent"
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
    pub(crate) fn summary(&self) -> String {
        let state = lock(&self.inner.state);
        if state.disk.agents.is_empty() {
            return String::new();
        }
        let active = state
            .disk
            .runs
            .values()
            .filter(|r| matches!(r.status.as_str(), "queued" | "running"))
            .count();
        format!(
            "Subagents: {active} 运行/排队 · {} 个会话 · /subagents 查看",
            state.disk.agents.len()
        )
    }
    pub(crate) fn snapshots(&self) -> Value {
        let state = lock(&self.inner.state);
        json!({"agents":state.disk.agents.values().collect::<Vec<_>>(),"runs":state.disk.runs.values().collect::<Vec<_>>()})
    }
    pub(crate) fn uri(
        &self,
        op: &str,
        path: &str,
        content: Option<&str>,
        token: &CancellationToken,
    ) -> Result<Value> {
        let (scheme, rest) = path.split_once("://").context("无效 URI")?;
        let (id, suffix) = rest.split_once('/').unwrap_or((rest, ""));
        if scheme == "history" && id.is_empty() {
            let state = lock(&self.inner.state);
            let agents=state.disk.agents.values().map(|r|json!({"agent_id":r.id,"name":r.name,"parent":r.parent,"status":r.status,"output_uri":format!("agent://{}",r.id)})).collect::<Vec<_>>();
            return Ok(json!({"ok":true,"agents":agents}));
        }
        if scheme == "proc" {
            let state = lock(&self.inner.state);
            let run = state.disk.runs.get(id).context("未知 Run")?;
            if op == "write" {
                ensure!(suffix == "kill", "仅支持 proc://run/kill");
                if matches!(run.status.as_str(), "queued" | "running")
                    && let Some(live) = state.live.get(&run.agent_id)
                {
                    live.cancellation.cancel();
                }
                return Ok(json!({"ok":true}));
            }
            return Ok(json!({"ok":true,"run":run}));
        }
        ensure!(suffix.is_empty(), "Agent URI 不支持路径后缀");
        let record = lock(&self.inner.state)
            .disk
            .agents
            .get(id)
            .cloned()
            .context("未知 Agent")?;
        if op == "write" {
            ensure!(scheme == "agent", "历史 URI 只支持读取");
            return self.followup(&record, content.context("消息不能为空")?, token);
        }
        if scheme == "history" {
            use std::io::{Read, Seek, SeekFrom};
            let path = self
                .inner
                .directory()
                .join(format!("{}.events.jsonl", record.id));
            let mut bytes = Vec::new();
            let mut truncated = false;
            match fs::File::open(&path) {
                Ok(mut file) => {
                    let size = file.metadata()?.len();
                    truncated = size > 65536;
                    if truncated {
                        file.seek(SeekFrom::Start(size - 65536))?;
                    }
                    file.take(65536).read_to_end(&mut bytes)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            if truncated {
                if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
                    bytes.drain(..=end);
                } else {
                    bytes.clear();
                }
            }
            if let Some(end) = bytes.iter().rposition(|b| *b == b'\n') {
                bytes.truncate(end + 1);
            } else {
                bytes.clear();
            }
            return Ok(
                json!({"ok":true,"history":String::from_utf8_lossy(&bytes),"truncated":truncated,"source_path":path}),
            );
        }
        let state = lock(&self.inner.state);
        let sequence = |r: &&Run| {
            r.id.strip_prefix('r')
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0)
        };
        let run = state
            .disk
            .runs
            .values()
            .filter(|r| r.agent_id == id)
            .max_by_key(sequence);
        let published = state
            .disk
            .runs
            .values()
            .filter(|r| r.agent_id == id && r.result.is_some())
            .max_by_key(sequence);
        Ok(
            json!({"ok":true,"agent":record,"run":run,"published":published,"superseded":run.map(|r|&r.id)!=published.map(|r|&r.id),"history_uri":format!("history://{id}")}),
        )
    }

    fn followup(&self, record: &Record, text: &str, token: &CancellationToken) -> Result<Value> {
        ensure!(!text.trim().is_empty(), "消息不能为空");
        let mut state = lock(&self.inner.state);
        ensure!(
            !state.stopping && !state.retiring.contains(&record.id),
            "Agent 正在清理，暂时不能追问"
        );
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
            "Agent 正在提交上一轮结果，请稍后重试追问。"
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
                requests: 0,
                tokens: 0,
                cost: None,
            },
        );
        self.inner.save(&state)?;
        drop(state);
        if let Err(error) = self.launch(record.clone(), &run, text.into(), token) {
            self.settle(&run, json!({"ok":false,"error":format!("{error:#}")}), 0)?;
            return Err(error);
        }
        Ok(json!({"ok":true,"run_id":run}))
    }
    pub(crate) fn control(&self, id: &str, action: &str, content: Option<&str>) -> Result<Value> {
        if action == "revive" {
            let record = lock(&self.inner.state)
                .disk
                .agents
                .get(id)
                .cloned()
                .context("未知 Agent")?;
            self.launch(record, "", String::new(), &CancellationToken::new())?;
            return Ok(json!({"ok":true,"revived":true}));
        }
        if action == "send" {
            let record = lock(&self.inner.state)
                .disk
                .agents
                .get(id)
                .cloned()
                .context("未知 Agent")?;
            return self.followup(
                &record,
                content.unwrap_or(
                    "Continue from your saved context. Do not repeat completed side effects.",
                ),
                &CancellationToken::new(),
            );
        }
        let mut state = lock(&self.inner.state);
        let agent = state.disk.agents.get_mut(id).context("未知 Agent")?;
        ensure!(matches!(action, "cancel" | "kill"), "未知控制动作");
        if action == "kill" {
            agent.status = "aborted".into();
            agent.generation += 1;
        }
        if action == "kill" {
            if let Some(live) = state.live.remove(id) {
                state.retiring.insert(id.to_owned());
                live.cancellation.cancel();
            }
        } else if let Some(live) = state.live.get(id) {
            live.cancellation.cancel();
        }
        self.inner.save(&state)?;
        self.inner.changed.notify_all();
        Ok(json!({"ok":true}))
    }
    pub(crate) fn cancel_owned(&self) -> Result<()> {
        let state = lock(&self.inner.state);
        for run in
            state.disk.runs.values().filter(|r| {
                r.owner == self.owner && matches!(r.status.as_str(), "queued" | "running")
            })
        {
            if let Some(live) = state.live.get(&run.agent_id) {
                live.cancellation.cancel();
            }
        }
        self.inner.changed.notify_all();
        Ok(())
    }
    pub(crate) fn reap_owned(&self) -> Result<()> {
        self.cancel_owned()?;
        let start = Instant::now();
        loop {
            let running = lock(&self.inner.state).disk.runs.values().any(|r| {
                r.owner == self.owner && matches!(r.status.as_str(), "queued" | "running")
            });
            if !running {
                return Ok(());
            }
            if start.elapsed() > Duration::from_secs(5) {
                return Err(crate::cancellation::CleanupError(anyhow::anyhow!(
                    "子 Agent 停止清理超时"
                ))
                .into());
            }
            self.sleep();
        }
    }
    pub(crate) fn shutdown(&self) -> Result<()> {
        {
            let mut state = lock(&self.inner.state);
            state.stopping = true;
            for live in state.live.values() {
                live.cancellation.cancel();
            }
            state.live.clear();
        }
        let start = Instant::now();
        while lock(&self.inner.state).workers > 0 {
            ensure!(
                start.elapsed() < Duration::from_secs(5),
                "子 Agent 清理超时"
            );
            self.sleep();
        }
        lock(&self.inner.factory).take();
        Ok(())
    }
    pub(crate) fn permit(&self, token: &CancellationToken) -> Result<Permit> {
        if self.owner == "Main" {
            return Ok(Permit(None));
        }
        let ticket = {
            let mut state = lock(&self.inner.state);
            state.next_permit += 1;
            let ticket = state.next_permit;
            state.permits.push_back(ticket);
            ticket
        };
        loop {
            let mut state = lock(&self.inner.state);
            if token.is_cancelled() || state.stopping {
                state.permits.retain(|id| *id != ticket);
                self.inner.changed.notify_all();
                return Err(crate::cancellation::Cancelled.into());
            }
            if state.permits.front() == Some(&ticket)
                && state.active < self.inner.settings.max_concurrency
            {
                state.permits.pop_front();
                state.active += 1;
                return Ok(Permit(Some(self.inner.clone())));
            }
            drop(state);
            self.sleep();
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
pub(crate) struct Permit(Option<Arc<Inner>>);
impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(inner) = &self.0 {
            let mut state = lock(&inner.state);
            state.active = state.active.saturating_sub(1);
            inner.changed.notify_all();
        }
    }
}

fn worker(
    weak: Weak<Inner>,
    record: Record,
    generation: u64,
    input: InputQueue,
    receiver: mpsc::Receiver<Work>,
    startup_token: CancellationToken,
) {
    let Some(inner) = weak.upgrade() else { return };
    struct WorkerExit {
        inner: Arc<Inner>,
        id: String,
        generation: u64,
    }
    impl Drop for WorkerExit {
        fn drop(&mut self) {
            let mut state = lock(&self.inner.state);
            state.workers = state.workers.saturating_sub(1);
            state.retiring.remove(&self.id);
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
    }
    let _exit = WorkerExit {
        inner: inner.clone(),
        id: record.id.clone(),
        generation,
    };
    let mut record = record;
    if record.spec.isolated {
        match workspace::prepare(
            &record.spec.cwd,
            &inner.directory(),
            &record.id,
            &startup_token,
        ) {
            Ok(cwd) => record.spec.cwd = cwd,
            Err(error) => {
                let mut state = lock(&inner.state);
                for run in state
                    .disk
                    .runs
                    .values_mut()
                    .filter(|r| r.agent_id == record.id && r.status == "queued")
                {
                    run.status = "failed".into();
                    run.result = Some(json!({"ok":false,"error":format!("{error:#}")}));
                }
                let _ = inner.save(&state);
                inner.changed.notify_all();
                return;
            }
        }
    }
    let binding = Binding {
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
    let ttl = if binding.inner.settings.idle_ttl_ms == 0 {
        Duration::from_secs(86400 * 365)
    } else {
        Duration::from_millis(binding.inner.settings.idle_ttl_ms)
    };
    let receive = || {
        if binding.inner.settings.idle_ttl_ms == 0 {
            receiver
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            receiver.recv_timeout(ttl)
        }
    };
    while let Ok(work) = receive() {
        let started = Instant::now();
        let _ = input.begin_run();
        let mut last = String::new();
        let outcome = (|| -> Result<Value> {
            cancellation::check(&work.cancellation)?;
            if agent.is_none() {
                let factory = lock(&binding.inner.factory)
                    .clone()
                    .context("Supervisor 已停止")?;
                agent = Some(factory(&record, binding.clone())?);
            }
            {
                let mut state = lock(&binding.inner.state);
                state
                    .disk
                    .runs
                    .get_mut(&work.run)
                    .context("未知 Run")?
                    .status = "running".into();
                binding.inner.save(&state)?;
            }
            let native = agent.as_mut().context("Agent 初始化失败")?;
            let mut sequence = 0u64;
            let mut requests = 0usize;
            let mut tokens = 0u64;
            let mut cost = record.spec.cost.map(|_| 0.0);
            let outcome=native.run(&work.task,&work.cancellation,Some(&input),&mut |event| {
                sequence+=1;
                match &event { AgentEvent::AssistantFinished{text}if !text.trim().is_empty()=>last=text.clone(), AgentEvent::ModelRequestStarted=>requests+=1, AgentEvent::ModelRequestFinished{usage:Some(usage),..}=>{
                    tokens+=usage.input_tokens+usage.output_tokens;
                    cost=cost.zip(record.spec.cost).and_then(|(total,price)|{
                        let cached=usage.cached_tokens?;let written=usage.cache_write_tokens.unwrap_or(0);
                        let uncached=usage.input_tokens.checked_sub(cached)?.checked_sub(written)?;
                        Some(total+(uncached as f64*price.input+cached as f64*price.cache_read+if written==0{0.0}else{written as f64*price.cache_write?}+usage.output_tokens as f64*price.output)/1e6)
                    });
                }, AgentEvent::ModelRequestFinished{usage:None,..}=>cost=None, _=>{} }
                if matches!(event,AgentEvent::ModelRequestStarted|AgentEvent::ModelRequestFinished{..}){let mut state=lock(&binding.inner.state);if let Some(run)=state.disk.runs.get_mut(&work.run){run.requests=requests;run.tokens=tokens;run.cost=cost;}}
                if let Ok(mut file)=OpenOptions::new().create(true).append(true).open(binding.inner.directory().join(format!("{}.events.jsonl",record.id))) { let _=writeln!(file,"{}",json!({"root_id":binding.inner.directory().file_name().map(|n|n.to_string_lossy().into_owned()),"agent_id":record.id,"run_id":work.run,"generation":generation,"sequence":sequence,"event":event})); }
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
            let result = if outcome == crate::cancellation::RunOutcome::Cancelled {
                json!({"ok":false,"cancelled":true,"output":last})
            } else {
                ensure!(
                    !last.trim().is_empty() || native.yielded().is_some(),
                    "子 Agent 未返回结果"
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
                                tracker.record(
                                    path.to_owned(),
                                    crate::tools::changes::Original::Unavailable,
                                    Vec::new(),
                                );
                            }
                            tracker.source(path, &record.id);
                        }
                    }
                }
            }
            Ok(value)
        });
        let mut result=outcome.unwrap_or_else(|e|json!({"ok":false,"cancelled":work.cancellation.is_cancelled(),"error":format!("{e:#}"),"partial_output":last}));
        let output_path = binding
            .inner
            .directory()
            .join(format!("{}.result.json", work.run));
        if let Err(error) = atomic_json(&output_path, &result) {
            result = json!({"ok":false,"error":format!("结果文件保存失败：{error:#}")});
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
            tracing::error!(error=%e,"子 Agent 结果持久化失败");
        }
        let mut state = lock(&binding.inner.state);
        if let Some(current) = state
            .disk
            .agents
            .get_mut(&record.id)
            .filter(|r| r.generation == generation)
        {
            current.status = "idle".into();
            current.updated_ms = now();
        }
        if binding.inner.save(&state).is_err() {
            break;
        }
        if state.stopping
            || state
                .disk
                .agents
                .get(&record.id)
                .is_none_or(|r| r.generation != generation)
        {
            break;
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
    let mut state = lock(&binding.inner.state);
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    context: String,
    tasks: Vec<Task>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    task: String,
    name: Option<String>,
    agent: Option<String>,
    #[serde(default)]
    isolated: bool,
    output_schema: Option<Value>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wait {
    #[serde(default)]
    run_ids: Vec<String>,
    #[serde(default = "wait_default")]
    timeout_ms: u64,
}
fn wait_default() -> u64 {
    60000
}
fn function(name: &str, description: &str, properties: Value, required: &[&str]) -> FunctionObject {
    FunctionObject {
        name: name.into(),
        description: Some(description.into()),
        parameters: Some(
            json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        ),
        ..Default::default()
    }
}
pub(crate) fn yield_definition() -> FunctionObject {
    function(
        "yield",
        "Submit the final structured result in data. Successful submission completes the assigned run.",
        json!({"data":{}}),
        &["data"],
    )
}
pub(crate) fn is_uri(path: &str) -> bool {
    ["agent://", "history://", "proc://"]
        .iter()
        .any(|p| path.starts_with(p))
}

pub(crate) fn validate_schema(schema: &Value) -> Result<()> {
    jsonschema::validator_for(schema)
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("无效输出 schema：{error}"))
}
pub(crate) fn validate_output(schema: &Value, data: &Value) -> Result<()> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| anyhow::anyhow!("无效输出 schema：{error}"))?;
    validator
        .validate(data)
        .map_err(|error| anyhow::anyhow!("输出不符合 schema：{error}"))
}
