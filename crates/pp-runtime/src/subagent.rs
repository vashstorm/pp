//! 子 Agent 的身份、调度、结果投递与持久生命周期。
mod catalog;
mod observe;
#[cfg(test)]
mod tests;
mod workspace;

pub use observe::HubAgent;
pub use observe::ObservedRecord;
pub use observe::{EventCursor, EventPage, HubEvent, HubSnapshot, ObservedEntry};

use pp_config::config::{ModelRole, ModelRoles, TaskSettings};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::agent::Agent;
use crate::agent::input::InputQueue;
use anyhow::{Context, Result, bail, ensure};
use async_openai::types::chat::FunctionObject;
use catalog::{BUILTIN_ROLES, Definition};
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::events::AgentEvent;
use pp_core::model::ThinkLevel;
use pp_core::plan::WorkMode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, Weak, atomic::AtomicU64, mpsc},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Spec {
    #[serde(default)]
    pub model_role: Option<pp_config::config::ModelRole>,
    #[serde(default)]
    pub role_route: Option<pp_config::config::RoleRoute>,
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
    pub cost: Option<pp_config::models::ModelCost>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub name: String,
    pub parent: String,
    pub generation: u64,
    pub status: String,
    pub spec: Spec,
    pub session_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registered_ms: Option<u64>,
    pub updated_ms: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub agent_id: String,
    pub owner: String,
    pub status: String,
    pub task: String,
    pub result: Option<Value>,
    pub consumed: bool,
    pub duration_ms: u64,
    #[serde(default)]
    pub started_ms: u64,
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
    /// 该 worker 发布的记录代次；只有它才能移除自己的 Live 槽。
    generation: u64,
}
struct State {
    disk: Disk,
    live: BTreeMap<String, Live>,
    workers: usize,
    retiring: BTreeMap<String, u64>,
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
pub(crate) fn is_read_only_child_tool(name: &str, mcp: impl FnOnce(&str) -> bool) -> bool {
    matches!(name, "read" | "grep" | "glob" | "lsp") || mcp(name)
}

#[derive(Default)]
struct McpTools {
    all: Vec<String>,
    read_only: Vec<String>,
}

struct Inner {
    // 进程内观察修订号：仅驱动 UI 增量刷新，不持久化。
    revision: AtomicU64,
    // 每个 agent 一条整行追加锁；worker 持有自己的 Arc。
    appends: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    observe_errors: Mutex<BTreeMap<String, String>>,
    /// 独立子 Registry 清理失败后保留可重试句柄，并向父 Agent 传播 typed error。
    lsp_cleanup: Mutex<Vec<crate::lsp::Cleanup>>,
    cleanup_errors: Mutex<Vec<anyhow::Error>>,
    output_previews: Mutex<BTreeMap<String, observe::ObservedRecord>>,
    /// 管道阶段预览与主输出快照分开保留；种类之间不互相覆盖。
    pipeline_previews: Mutex<BTreeMap<String, observe::ObservedRecord>>,
    runtime_models: Mutex<Option<crate::model_control::Binding>>,
    model_roles: Mutex<pp_config::config::ModelRoles>,
    state: Mutex<State>,
    changed: Condvar,
    settings: TaskSettings,
    semaphore: Arc<Semaphore>,
    dir: Mutex<PathBuf>,
    user: PathBuf,
    factory: Mutex<Option<Arc<Factory>>>,
    validate: Arc<Validator>,
    changes: Mutex<Option<ChangeSink>>,
    mcp_tools: Mutex<McpTools>,
    plan_mode: bool,
}
#[derive(Clone)]
pub struct Binding {
    pub(crate) accounting: Option<pp_config::task_status::Ledger>,
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
    accounting: Option<pp_config::task_status::Ledger>,
    run: String,
    task: String,
    cancellation: CancellationToken,
}

/// 持久 worker 的空闲轮询周期：既检查空闲 TTL，也让迟到 LSP 诊断及时上报。
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(50);

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
    use std::io::{BufWriter, Write};
    use std::os::unix::fs::PermissionsExt;
    let parent = path.parent().context("missing persist directory")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    {
        // Registry 在共享调度锁内保存，缓冲细碎 JSON 写入，避免阻塞 permit 与结果结算。
        let mut writer = BufWriter::new(&mut tmp);
        serde_json::to_writer(&mut writer, value)?;
        writer.flush()?;
    }
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
        atomic_json(&self.directory().join("registry.json"), &state.disk)?;
        self.bump_revision();
        Ok(())
    }
}
pub(crate) struct RootOptions {
    pub dir: PathBuf,
    pub user: PathBuf,
    pub cwd: PathBuf,
    pub model: String,
    pub thinking: ThinkLevel,
    pub settings: TaskSettings,
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
            "task.max_concurrency must be in 1..=32"
        );
        ensure!(
            settings.max_recursion_depth <= 8,
            "task.max_recursion_depth must not exceed 8"
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
        ensure!(disk.version <= 1, "unrecognized subagent storage version");
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
                "unknown subagent task status"
            );
            if matches!(run.status.as_str(), "queued" | "running") {
                match fs::read(dir.join(format!("{}.result.json", run.id))) {
                    Ok(bytes) => {
                        let result: Value = serde_json::from_slice(&bytes)?;
                        ensure!(result["ok"].is_boolean(), "invalid subagent result file");
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
                            json!({"ok":false,"error":"Process interrupted; explicit follow-up required to continue, tools will not be replayed."}),
                        );
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            ensure!(
                matches!(run.status.as_str(), "queued" | "running") || run.result.is_some(),
                "terminal subagent task missing result"
            );
        }
        let inner = Arc::new(Inner {
            revision: AtomicU64::new(1),
            appends: Mutex::new(BTreeMap::new()),
            observe_errors: Mutex::new(BTreeMap::new()),
            lsp_cleanup: Mutex::new(Vec::new()),
            cleanup_errors: Mutex::new(Vec::new()),
            output_previews: Mutex::new(BTreeMap::new()),
            pipeline_previews: Mutex::new(BTreeMap::new()),
            runtime_models: Mutex::new(None),
            model_roles: Mutex::new(Default::default()),
            state: Mutex::new(State {
                disk,
                live: BTreeMap::new(),
                workers: 0,
                retiring: Default::default(),
                stopping: false,
            }),
            changed: Condvar::new(),
            semaphore: Arc::new(Semaphore::new(settings.max_concurrency)),
            settings,
            dir: Mutex::new(dir),
            user,
            factory: Mutex::new(Some(factory)),
            validate,
            changes: Mutex::new(None),
            mcp_tools: Mutex::default(),
            plan_mode,
        });
        Ok(Self {
            accounting: None,
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
    pub(crate) fn set_runtime_models(&self, binding: Option<crate::model_control::Binding>) {
        *lock(&self.inner.runtime_models) = binding;
    }
    pub(crate) fn update_model(
        &self,
        model: &str,
        thinking: ThinkLevel,
        cost: Option<pp_config::models::ModelCost>,
    ) -> Result<()> {
        let mut state = lock(&self.inner.state);
        let previous = state.disk.agents.get(&self.owner).cloned();
        if let Some(record) = state.disk.agents.get_mut(&self.owner) {
            record.spec.model = model.into();
            record.spec.thinking = thinking;
            record.spec.cost = cost;
            record.updated_ms = now();
        }
        if let Err(error) = self.inner.save(&state) {
            if let Some(previous) = previous {
                state.disk.agents.insert(self.owner.clone(), previous);
            }
            return Err(error);
        }
        Ok(())
    }
    pub(crate) fn set_model_roles(&self, roles: pp_config::config::ModelRoles) {
        *lock(&self.inner.model_roles) = roles;
    }
    fn model_roles(&self) -> ModelRoles {
        lock(&self.inner.runtime_models)
            .as_ref()
            .and_then(|binding| binding.registry.catalog())
            .map(|catalog| catalog.subagent_roles())
            .unwrap_or_else(|| lock(&self.inner.model_roles).clone())
    }
    pub(crate) fn allow_mcp(&self, all: Vec<String>, read_only: Vec<String>) {
        *lock(&self.inner.mcp_tools) = McpTools { all, read_only };
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
    fn custom_roles(&self) -> Vec<Definition> {
        catalog::discover(&self.cwd, &self.inner.user, &self.inner.settings)
    }
    pub(crate) fn definitions(&self, _mode: WorkMode) -> Vec<FunctionObject> {
        if self.depth != 0
            || self.depth >= self.inner.settings.max_recursion_depth
            || self.spawns.is_empty()
            || self.readonly
        {
            return Vec::new();
        }
        let roles = self.model_roles();
        let custom = self.custom_roles();
        let builtin = BUILTIN_ROLES.iter().filter_map(|&(name, role)| {
            let assignment = roles.get(&role).filter(|assignment| assignment.enabled)?;
            Some((
                name,
                format!("{}:{}", assignment.model, assignment.effort.as_str()),
            ))
        });
        let available = builtin
            .chain(
                custom
                    .iter()
                    .map(|def| (def.name.as_str(), def.description.clone())),
            )
            .collect::<Vec<_>>();
        if available.is_empty() {
            return Vec::new();
        }
        let names = available.iter().map(|(name, _)| *name).collect::<Vec<_>>();
        let help = available
            .iter()
            .map(|(name, description)| format!("{name}: {description}"))
            .collect::<Vec<_>>()
            .join("; ");
        let review_help = if names.contains(&"review") {
            " Select role=review for read-only code review."
        } else {
            ""
        };
        vec![function(
            "task",
            &format!(
                "Delegate independent tasks with explicit shared context. Final results automatically return to you; use wait when blocked. When executing an existing TODO list, identify which items each child covers and keep checklist updates with the parent. After dispatch and before wait, use todo start for every actually running item covered by the children and reconcile verified completions and real blockers. Multiple TODO items may be in progress concurrently; starting one preserves the others. Delegation is not completion, and a running child is not blocked merely because the parent awaits its result. Do not mark queued or dependency-blocked work as started. Plan mode must not call todo. Every task must explicitly set role to one of the available roles: {help}.{review_help} Tasks are asynchronous unless the custom role sets blocking=true. Plan-mode children are always read-only."
            ),
            json!({"context":{"type":"string"},"tasks":{"type":"array","minItems":1,"maxItems":32,"items":{"type":"object","properties":{"name":{"type":"string"},"role":{"type":"string","enum":names},"task":{"type":"string"},"isolated":{"type":"boolean"},"readonly":{"type":"boolean","description":"Limit this task to read-only investigation without delegation."},"output_schema":{}},"required":["task","role"],"additionalProperties":false}}}),
            &["context", "tasks"],
        )]
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
            _ => bail!("unknown subagent tool"),
        }
    }
    fn spawn(&self, batch: Batch, plan: bool, token: &CancellationToken) -> Result<Value> {
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
            let model_role = BUILTIN_ROLES
                .iter()
                .find_map(|&(name, model_role)| (name == role).then_some(model_role));
            let assignment = model_role
                .map(|role| {
                    roles
                        .get(&role)
                        .filter(|role| role.enabled)
                        .with_context(|| {
                            format!(
                                "model role {} is not selected; enable it in /Roles first",
                                role.name()
                            )
                        })
                })
                .transpose()?;
            let custom = definitions.iter().find(|def| def.name == role);
            let (model, thinking) = if let Some(assignment) = assignment {
                (assignment.model.clone(), assignment.effort)
            } else {
                let def = custom.context("unknown or disabled task role; select a listed role")?;
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
                self.spawns.iter().any(|name| name == "*" || name == role),
                "parent role does not allow spawning {role}"
            );
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
            let output = item
                .output_schema
                .clone()
                .or_else(|| custom.and_then(|def| def.output.clone()));
            if let Some(schema) = &output {
                validate_schema(schema)?;
            }
            let mcp_tools = lock(&self.inner.mcp_tools);
            let readonly = plan
                || item.readonly
                || matches!(
                    model_role,
                    Some(
                        pp_config::config::ModelRole::Review
                            | pp_config::config::ModelRole::Explorer
                    )
                )
                || custom
                    .and_then(|def| def.tools.as_ref())
                    .is_some_and(|tools| {
                        tools.iter().all(|name| {
                            is_read_only_child_tool(name, |name| {
                                mcp_tools.read_only.iter().any(|tool| tool == name)
                            })
                        })
                    });
            let configured_tools = custom.and_then(|def| def.tools.as_ref());
            let mut tools = configured_tools.cloned().unwrap_or_else(|| {
                [
                    "read",
                    "grep",
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
            let prompt = if let Some(def) = custom {
                def.prompt.as_str()
            } else {
                match (model_role, readonly) {
                    (Some(ModelRole::Review), _) => include_str!("prompts/roles/review.md"),
                    (Some(ModelRole::Explorer), _) => include_str!("prompts/roles/explorer.md"),
                    (Some(ModelRole::Fast | ModelRole::Slow | ModelRole::Vision), true) => {
                        include_str!("prompts/roles/read-only.md")
                    }
                    (Some(ModelRole::Fast), false) => include_str!("prompts/roles/fast.md"),
                    (Some(ModelRole::Slow), false) => include_str!("prompts/roles/slow.md"),
                    (Some(ModelRole::Vision), false) => include_str!("prompts/roles/vision.md"),
                    (_, true) => "Investigate using the available read-only tools, including authorized MCP tools when relevant. Report evidence and source references.",
                    (_, false) => "Complete the assigned work thoroughly. Use tools as needed. Return a concise final result.",
                }
                .trim_end()
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
            prepared.push((item, spec, custom.is_some_and(|def| def.blocking)));
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
                accounting: self.accounting.clone(),
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
                accounting: self.accounting.clone(),
                run: run.into(),
                task,
                cancellation: token,
            })?;
        }
        Ok(())
    }
    fn settle(&self, run: &str, result: Value, duration_ms: u64) -> Result<()> {
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
    /// 当前正在运行的子代理数；排队和已结束的会话不计入。
    pub fn running_subagent_count(&self) -> usize {
        lock(&self.inner.state)
            .disk
            .agents
            .values()
            .filter(|agent| agent.status == "running")
            .count()
    }

    pub fn summary(&self) -> String {
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
            "Subagents: {active} running/queued · {} sessions · use /subagents",
            state.disk.agents.len()
        )
    }
    /// 行模式 CLI 与既有消费者读取的 JSON 形状：agents 保持扁平的 Record 字段。
    pub fn snapshots(&self) -> Value {
        serde_json::to_value(self.hub_snapshot()).unwrap_or_else(|_| json!({"agents":[],"runs":[]}))
    }
    pub(crate) fn uri(
        &self,
        op: &str,
        path: &str,
        content: Option<&str>,
        token: &CancellationToken,
    ) -> Result<Value> {
        let (scheme, rest) = path.split_once("://").context("invalid URI")?;
        let (id, suffix) = rest.split_once('/').unwrap_or((rest, ""));
        if scheme == "history" && id.is_empty() {
            let state = lock(&self.inner.state);
            let agents=state.disk.agents.values().map(|r|json!({"agent_id":r.id,"name":r.name,"parent":r.parent,"status":r.status,"output_uri":format!("agent://{}",r.id)})).collect::<Vec<_>>();
            return Ok(json!({"ok":true,"agents":agents}));
        }
        if scheme == "proc" {
            let state = lock(&self.inner.state);
            let run = state.disk.runs.get(id).context("unknown run")?;
            if op == "write" {
                ensure!(suffix == "kill", "only proc://run/kill is supported");
                if matches!(run.status.as_str(), "queued" | "running")
                    && let Some(live) = state.live.get(&run.agent_id)
                {
                    live.cancellation.cancel();
                }
                return Ok(json!({"ok":true}));
            }
            return Ok(json!({"ok":true,"run":run}));
        }
        ensure!(
            suffix.is_empty(),
            "agent URI does not support path suffixes"
        );
        let record = lock(&self.inner.state)
            .disk
            .agents
            .get(id)
            .cloned()
            .context("unknown agent")?;
        if op == "write" {
            ensure!(scheme == "agent", "history URI is read-only");
            return self.followup(
                &record,
                content.context("message must not be empty")?,
                token,
            );
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
        bail!("unsupported subagent URI operation")
    }

    pub(crate) fn read_agent_result(
        &self,
        uri: &str,
        token: &CancellationToken,
    ) -> Result<(Value, Option<Value>)> {
        use std::io::{BufReader, Read};
        use std::os::unix::fs::OpenOptionsExt;

        const MAX_RESULT_BYTES: u64 = 16 * 1024 * 1024;

        let rest = uri.strip_prefix("agent://").context("invalid agent URI")?;
        let (id, pinned) = rest
            .split_once('/')
            .map_or((rest, None), |(id, run)| (id, Some(run)));
        let (metadata, published_id) = {
            let state = lock(&self.inner.state);
            let record = state.disk.agents.get(id).context("unknown agent")?;
            let sequence = |run: &&Run| {
                run.id
                    .strip_prefix('r')
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(0)
            };
            let runs = || state.disk.runs.values().filter(|run| run.agent_id == id);
            let run = runs().max_by_key(sequence);
            let published = match pinned {
                Some(run_id) => Some(
                    runs()
                        .find(|run| run.id == run_id && run.result.is_some())
                        .context("unknown or unpublished agent run")?,
                ),
                None => runs()
                    .filter(|run| run.result.is_some())
                    .max_by_key(sequence),
            };
            (
                json!({"ok":true,"agent":record,"run":run,"published":published,"superseded":run.map(|r|&r.id)!=published.map(|r|&r.id),"history_uri":format!("history://{id}")}),
                published.map(|run| run.id.clone()),
            )
        };
        let Some(run_id) = published_id else {
            return Ok((metadata, None));
        };
        ensure!(
            run_id
                .strip_prefix('r')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|byte| byte.is_ascii_digit())),
            "invalid stored agent run id"
        );
        cancellation::check(token)?;
        let path = self.directory().join(format!("{run_id}.result.json"));
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            )
            .open(path);
        let result = match file {
            Ok(file) => {
                let info = file.metadata()?;
                ensure!(info.is_file(), "agent result must be a regular file");
                ensure!(
                    info.len() <= MAX_RESULT_BYTES,
                    "agent result exceeds 16 MiB"
                );
                let result: Value =
                    serde_json::from_reader(BufReader::new(file.take(MAX_RESULT_BYTES)))
                        .context("invalid agent result file")?;
                ensure!(result["ok"].is_boolean(), "invalid agent result file");
                result
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // 旧登记表和中断恢复可能只有内联结果。已截断的预览不能冒充完整正文。
                let preview = &metadata["published"]["result"];
                ensure!(preview["truncated"] != true, "agent result file is missing");
                preview.clone()
            }
            Err(error) => return Err(error).context("opening agent result file"),
        };
        cancellation::check(token)?;
        Ok((metadata, Some(result)))
    }

    fn followup(&self, record: &Record, text: &str, token: &CancellationToken) -> Result<Value> {
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
        Ok(json!({"ok":true,"run_id":run}))
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
    /// 只取消当前 owner 明确选中的未完成 Run，空集合不代表全部。
    pub(crate) fn cancel_runs(&self, ids: &[String]) -> Result<()> {
        let state = lock(&self.inner.state);
        for id in ids {
            ensure!(
                state
                    .disk
                    .runs
                    .get(id)
                    .is_some_and(|run| run.owner == self.owner),
                "run does not belong to this agent"
            );
        }
        for run in state.disk.runs.values().filter(|run| {
            ids.contains(&run.id) && matches!(run.status.as_str(), "queued" | "running")
        }) {
            if let Some(live) = state.live.get(&run.agent_id) {
                live.cancellation.cancel();
            }
        }
        self.inner.changed.notify_all();
        Ok(())
    }

    /// 终态由真实执行路径发布；不得靠改 registry 状态假装已经回收。
    pub(crate) fn reap_runs(&self, ids: &[String]) -> Result<()> {
        let started = Instant::now();
        loop {
            self.check_lsp_cleanup()?;
            if ids.is_empty()
                || !self
                    .wait_jobs(ids)?
                    .iter()
                    .any(crate::tools::wait::Job::pending)
            {
                return Ok(());
            }
            if started.elapsed() >= Duration::from_secs(5) {
                return Err(pp_core::cancellation::CleanupError(anyhow::anyhow!(
                    "subagent stop cleanup timed out"
                ))
                .into());
            }
            self.sleep();
        }
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
            self.check_lsp_cleanup()?;
            if !running {
                return Ok(());
            }
            if start.elapsed() > Duration::from_secs(5) {
                return Err(pp_core::cancellation::CleanupError(anyhow::anyhow!(
                    "subagent stop cleanup timed out"
                ))
                .into());
            }
            self.sleep();
        }
    }
    pub fn shutdown(&self) -> Result<()> {
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
                "subagent cleanup timed out"
            );
            self.sleep();
        }
        self.check_lsp_cleanup()?;
        lock(&self.inner.factory).take();
        Ok(())
    }
    pub(crate) fn check_lsp_cleanup(&self) -> Result<()> {
        let handles = std::mem::take(&mut *lock(&self.inner.lsp_cleanup));
        let mut errors = std::mem::take(&mut *lock(&self.inner.cleanup_errors));
        let mut pending = Vec::new();
        for mut handle in handles {
            if let Err(error) = handle.stop() {
                pending.push(handle);
                errors.push(error);
            }
        }
        lock(&self.inner.lsp_cleanup).extend(pending);
        let mut errors = errors.into_iter();
        if let Some(mut error) = errors.next() {
            for additional in errors {
                error = error.context(format!(
                    "Additional subagent cleanup failure: {additional:#}"
                ));
            }
            return Err(error);
        }
        Ok(())
    }

    fn permit(&self, token: &CancellationToken) -> Result<OwnedSemaphorePermit> {
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

fn worker(
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
        accounting: None,
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
                        cost = cost.zip(current_price).and_then(|(total, price)| {
                            let cached = usage.cached_tokens?;
                            let written = usage.cache_write_tokens.unwrap_or(0);
                            let uncached = usage
                                .input_tokens
                                .checked_sub(cached)?
                                .checked_sub(written)?;
                            Some(
                                total
                                    + (uncached as f64 * price.input
                                        + cached as f64 * price.cache_read
                                        + if written == 0 {
                                            0.0
                                        } else {
                                            written as f64 * price.cache_write?
                                        }
                                        + usage.output_tokens as f64 * price.output)
                                        / 1e6,
                            )
                        });
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    context: String,
    tasks: Vec<Task>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    role: String,
    task: String,
    name: Option<String>,
    #[serde(default)]
    isolated: bool,
    #[serde(default)]
    readonly: bool,
    output_schema: Option<Value>,
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
        .map_err(|error| anyhow::anyhow!("invalid output schema: {error}"))
}
pub(crate) fn validate_output(schema: &Value, data: &Value) -> Result<()> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| anyhow::anyhow!("invalid output schema: {error}"))?;
    validator
        .validate(data)
        .map_err(|error| anyhow::anyhow!("output does not match schema: {error}"))
}
