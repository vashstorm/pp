//! 子 Agent 的身份、调度、结果投递与持久生命周期。
mod binding;
pub(crate) mod catalog;
mod dispatch;
mod followup;
mod lifecycle;
mod observe;
mod results;
mod task_tool;
#[cfg(test)]
mod tests;
mod uri;
mod worker;
mod workspace;

pub use observe::HubAgent;
pub use observe::ObservedRecord;
pub use observe::{EventCursor, EventPage, HubEvent, HubSnapshot, ObservedEntry};

use pp_config::config::TaskSettings;
use tokio::sync::Semaphore;

use crate::agent::Agent;
use crate::agent::input::InputQueue;
use anyhow::{Context, Result};
use async_openai::types::chat::FunctionObject;
use pp_core::cancellation::CancellationToken;
use pp_core::model::ThinkLevel;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, atomic::AtomicU64, mpsc},
    time::Duration,
};

#[cfg(test)]
use anyhow::bail;
#[cfg(test)]
use pp_config::config::ModelRole;
#[cfg(test)]
use pp_core::events::AgentEvent;
#[cfg(test)]
use pp_core::plan::WorkMode;
#[cfg(test)]
use std::thread;
#[cfg(test)]
use std::time::Instant;

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
    #[serde(default, deserialize_with = "persisted_cost")]
    pub cost: Option<pp_config::models::ModelCost>,
}

/// 读取注册表中持久化的费率。旧版本把缺省的 `cacheWrite` 序列化为 `null`，
/// 配置文件对 `null` 严格拒绝，但已写入的 registry 必须仍可恢复：
/// 这里只把 `cacheWrite: null` 视为省略，其余字段仍由 `ModelCost` 严格校验。
fn persisted_cost<'de, D>(deserializer: D) -> Result<Option<pp_config::models::ModelCost>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let Some(mut value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    if let Some(object) = value.as_object_mut()
        && object.get("cacheWrite").is_some_and(Value::is_null)
    {
        object.remove("cacheWrite");
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(D::Error::custom)
}

/// 子 Agent 每次已结算请求累加到 run 费用上。
///
/// 计价只走 [`pp_config::models::ModelCost::calculate`]：未知缓存、缺费率或非有限金额
/// 一律让整段金额保持未知，不按零补齐。
fn accumulate_cost(
    total: Option<f64>,
    price: Option<pp_config::models::ModelCost>,
    usage: pp_core::context::TokenUsage,
) -> Option<f64> {
    let cost = price?.calculate(usage)?.total();
    let total = total? + cost;
    total.is_finite().then_some(total)
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
    matches!(name, "read" | "rg" | "glob" | "lsp") || mcp(name)
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
    shell_previews: Mutex<BTreeMap<String, observe::ObservedRecord>>,
    runtime_models: Mutex<Option<crate::model_control::Binding>>,
    model_roles: Mutex<pp_config::config::ModelRoles>,
    accounting: Mutex<Option<pp_config::task_status::Ledger>>,
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
