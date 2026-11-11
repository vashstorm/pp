//! 与会话日志独立的请求统计；数据库故障不改变模型调用结果。
mod activity;
mod analytics;
mod insights;
pub mod server;

use anyhow::{Context, Result, ensure};
use pp_config::models::{ModelCost, ModelPricing};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, DirBuilder, OpenOptions},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct RequestContext {
    pub model_reference: String,
    /// 统计库路径。生产由 [`database_path`] 解析，测试可指向临时库以免写真实 HOME。
    pub database: PathBuf,
    pub cwd: PathBuf,
    pub purpose: &'static str,
    pub session_file: Option<PathBuf>,
    /// 会话文件尚未分配时，保留已写入的统计键以便随后关联。
    pub unlinked_requests: Option<Arc<Mutex<Vec<String>>>>,
    pub first_request: Arc<AtomicBool>,
    pub skills: Vec<pp_config::skills::SkillHeader>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct RequestSnapshot {
    pub request: Value,
    pub skills: Vec<pp_config::skills::SkillHeader>,
}

impl RequestContext {
    pub fn capture(&self, request: &impl Serialize) -> Option<RequestSnapshot> {
        if !self.first_request.swap(false, Ordering::Relaxed) {
            return None;
        }
        match serde_json::to_value(request) {
            Ok(request) => Some(RequestSnapshot {
                request,
                skills: self.skills.clone(),
            }),
            Err(_) => {
                warn();
                None
            }
        }
    }
}

pub struct Observation {
    pub timestamp: i64,
    pub ttft_ms: Option<u64>,
    pub text: String,
    pub thinking: String,
    pub output: Value,
    pub stop_reason: Option<String>,
    pub usage: Option<pp_core::context::TokenUsage>,
    pub request_context: Option<RequestSnapshot>,
}
impl Observation {
    pub fn new() -> Self {
        Self {
            timestamp: chrono::Utc::now().timestamp_millis(),
            ttft_ms: None,
            text: String::new(),
            thinking: String::new(),
            output: Value::Null,
            stop_reason: None,
            usage: None,
            request_context: None,
        }
    }
    pub fn event(&mut self, event: &pp_core::events::AgentEvent, started: Instant) {
        use pp_core::events::AgentEvent;
        match event {
            AgentEvent::ModelOutputProgress { output_bytes } if *output_bytes > 0 => {
                self.ttft_ms
                    .get_or_insert(started.elapsed().as_millis() as u64);
            }
            AgentEvent::AssistantDelta { text, .. } => self.text.push_str(text),
            AgentEvent::ThinkingDelta { text } => self.thinking.push_str(text),
            _ => {}
        }
    }
}

impl Default for Observation {
    fn default() -> Self {
        Self::new()
    }
}

/// 一次请求在采集时记录的费用分量（USD），字段与 `pp_config::models::TokenCost` 一致。
///
/// `requests.cost_json` 为 NULL 表示该请求未定价（旧记录或缺价格／usage）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct CostSnapshot {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
}

impl CostSnapshot {
    pub fn total(&self) -> f64 {
        self.input + self.cache_read + self.cache_write + self.output
    }
}

/// 采集时匹配到的费率（USD／百万 token）；`cache_write` 缺失表示该费率没有独立写入价。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RateSnapshot {
    pub input: f64,
    pub cached_input: f64,
    pub cache_write: Option<f64>,
    pub output: f64,
}

/// 采集时的计价依据：请求结束时读一次 `models.yml` 与内置表，之后只写快照。
///
/// `pricing` 为 `None` 表示模型配置无法解析：与旧行为一致，此时既不计价也不记录费率。
pub(crate) struct Rates {
    pricing: Option<ModelPricing>,
}

impl Rates {
    pub fn load(models_path: &Path) -> Self {
        let pricing = match pp_config::models::load_models(models_path) {
            Ok(providers) => Some(ModelPricing::from_providers(&providers)),
            // 没有 models.yml 时只保留内置价格表。
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                Some(ModelPricing::default())
            }
            Err(_) => None,
        };
        Self { pricing }
    }

    /// 返回 `(费用, 费率)`：命中费率而 usage 不全时只记录费率，未命中费率时两者皆空。
    pub fn snapshot(
        &self,
        reference: &str,
        model: &str,
        input: Option<i64>,
        output: Option<i64>,
        cached: Option<i64>,
        written: Option<i64>,
    ) -> (Option<CostSnapshot>, Option<RateSnapshot>) {
        let Some(price) = self
            .pricing
            .as_ref()
            .and_then(|pricing| pricing.rate(reference, model))
        else {
            return (None, None);
        };
        let rate = RateSnapshot {
            input: price.input,
            cached_input: price.cache_read,
            cache_write: price.cache_write,
            output: price.output,
        };
        (calculate(price, input, output, cached, written), Some(rate))
    }
}

/// 与 `pp_config::models::ModelCost::calculate` 共享严格规则：未知缓存读不补零。
fn calculate(
    price: ModelCost,
    input: Option<i64>,
    output: Option<i64>,
    cached: Option<i64>,
    written: Option<i64>,
) -> Option<CostSnapshot> {
    let usage = pp_core::context::TokenUsage {
        input_tokens: input?.try_into().ok()?,
        output_tokens: output?.try_into().ok()?,
        cached_tokens: Some(cached?.try_into().ok()?),
        // 写入量未知时保留 None，由费率决定能否计价。
        cache_write_tokens: written.map(u64::try_from).transpose().ok()?,
    };
    price.calculate(usage).map(|cost| CostSnapshot {
        input: cost.input,
        cache_read: cost.cache_read,
        cache_write: cost.cache_write,
        output: cost.output,
    })
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RequestRow {
    pub id: i64,
    pub request_key: String,
    pub timestamp: i64,
    pub model: String,
    pub provider: String,
    pub model_reference: String,
    pub api: String,
    pub project: String,
    pub cwd: String,
    pub purpose: String,
    pub session_file: Option<String>,
    pub entry_id: Option<String>,
    pub duration_ms: f64,
    pub ttft_ms: Option<f64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub status: String,
    pub stop_reason: Option<String>,
    pub error_message: Option<String>,
    /// 采集时记录的费用快照；旧行没有快照时保持未定价。
    #[serde(skip)]
    pub cost: Option<CostSnapshot>,
    /// 采集时记录的费率快照，供概览的模型费率面板读取。
    #[serde(skip)]
    pub rates: Option<RateSnapshot>,
}

const COLUMNS: &str = "id,request_key,timestamp,model,provider,model_reference,api,project,cwd,purpose,session_file,entry_id,duration_ms,ttft_ms,input_tokens,output_tokens,cached_tokens,status,stop_reason,error_message,cache_write_tokens,cost_json,rate_json";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RequestRow> {
    Ok(RequestRow {
        id: r.get(0)?,
        request_key: r.get(1)?,
        timestamp: r.get(2)?,
        model: r.get(3)?,
        provider: r.get(4)?,
        model_reference: r.get(5)?,
        api: r.get(6)?,
        project: r.get(7)?,
        cwd: r.get(8)?,
        purpose: r.get(9)?,
        session_file: r.get(10)?,
        entry_id: r.get(11)?,
        duration_ms: r.get(12)?,
        ttft_ms: r.get(13)?,
        input_tokens: r.get(14)?,
        output_tokens: r.get(15)?,
        cached_tokens: r.get(16)?,
        status: r.get(17)?,
        stop_reason: r.get(18)?,
        error_message: r.get(19)?,
        cache_write_tokens: r.get(20)?,
        // 单行快照损坏只按未定价处理，不让整页查询失败。
        cost: r
            .get::<_, Option<String>>(21)?
            .and_then(|json| serde_json::from_str(&json).ok()),
        rates: r
            .get::<_, Option<String>>(22)?
            .and_then(|json| serde_json::from_str(&json).ok()),
    })
}

impl RequestRow {
    pub fn cost(&self) -> Option<CostSnapshot> {
        self.cost.clone()
    }
    pub fn rates(&self) -> Option<RateSnapshot> {
        self.rates.clone()
    }
    /// 列表与详情共用的请求 JSON：`cost` 为快照总额，`cost_components` 为四分量。
    pub fn value(&self) -> Result<Value> {
        let mut value = serde_json::to_value(self)?;
        let cost = self.cost();
        value["cost"] = json!(cost.as_ref().map(CostSnapshot::total));
        value["cost_components"] = json!(cost);
        Ok(value)
    }
}

/// 列表查询的 SQL；最后一列是 `has_context`（是否已采集原始请求 JSON）。
fn latest_sql() -> String {
    format!(
        "SELECT {COLUMNS},EXISTS(SELECT 1 FROM request_payloads WHERE request_id=requests.id AND request_json IS NOT NULL) FROM requests WHERE timestamp>=?1 AND timestamp<=?2 ORDER BY timestamp DESC,id DESC LIMIT ?3"
    )
}

/// 工具执行的采集状态；错误与耗时只在 runtime 实际执行工具后写入。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Ok,
    Error,
    Cancelled,
}

impl ToolStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

/// 一条工具执行记录。`request_key` 关联发起该调用的模型请求，缺失时只能按工具聚合。
pub struct ToolCall<'a> {
    pub request_key: Option<&'a str>,
    pub session_file: &'a Path,
    pub call_id: &'a str,
    pub tool: &'a str,
    pub status: ToolStatus,
    pub error_message: Option<&'a str>,
    pub timestamp: i64,
    pub duration_ms: f64,
}

/// 生产环境的统计库路径：`$HOME/.pp/stats/stats.db`。
///
/// 采集入口不直接调用它——路径随 [`RequestContext::database`] 与 `link`/`tool`
/// 参数传入，测试因此能把写入限制在临时目录。
pub fn database_path() -> Result<PathBuf> {
    let agent = pp_config::config::global_dir()?;
    Ok(agent
        .parent()
        .context("statistics directory unavailable")?
        .join("stats/stats.db"))
}

/// 汇总已关联到根会话及其子 Agent 的请求所记录的费用。
///
/// 缺费用快照的旧记录只把 [`pp_config::task_status::HistoricalCost::incomplete`] 置真，
/// 不按当前配置回算。旧库不存在时不建库。
pub fn session_cost(session: &Path) -> Result<pp_config::task_status::HistoricalCost> {
    let path = database_path()?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(metadata.is_file(), "statistics database must be a file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error.into()),
    }
    Store::open(&path)?.session_cost(session)
}

/// 根会话与其专属子 Agent 目录的索引范围。
///
/// `upper` 是 `prefix` 的字典序上界（取末字符加一个码点），因此 `[prefix, upper)` 恰好覆盖
/// `<root>.subagents/sessions/` 下的文件，而 `sessions-other/` 等相邻目录不在区间内。
pub(crate) struct SessionScope {
    pub root: String,
    pub prefix: String,
    pub upper: String,
}

impl SessionScope {
    pub(crate) fn new(root: String, prefix: String) -> Result<Self> {
        let last = prefix
            .chars()
            .next_back()
            .context("session prefix must not be empty")?;
        let next = char::from_u32(u32::from(last) + 1)
            .context("session prefix must end with a character that has a successor")?;
        let mut upper = prefix.clone();
        upper.pop();
        upper.push(next);
        Ok(Self {
            root,
            prefix,
            upper,
        })
    }
}

/// 根会话与子 Agent 目录的两段 `UNION ALL`；参数固定为 `?1`=root、`?2`=prefix、`?3`=upper。
pub(crate) fn session_requests_sql(columns: &str) -> String {
    format!(
        "SELECT {columns} FROM requests WHERE session_file=?1 \
         UNION ALL \
         SELECT {columns} FROM requests WHERE session_file>=?2 AND session_file<?3"
    )
}

/// Returns (total requests before cleanup, removed), counted in the same transaction.
/// Delete request snapshots before their parents, then reclaim the freed pages.
pub fn gc(cutoff: i64) -> Result<(u64, u64)> {
    let path = database_path()?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(metadata.is_file(), "statistics database must be a file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(error) => return Err(error.into()),
    }
    let mut store = Store::open(&path)?;
    let tx = store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let total = tx.query_row("SELECT COUNT(*) FROM requests", [], |row| {
        row.get::<_, i64>(0)
    })? as u64;
    tx.execute(
        "DELETE FROM request_payloads WHERE request_id IN (SELECT id FROM requests WHERE timestamp < ?1)",
        [cutoff],
    )?;
    let removed = tx.execute("DELETE FROM requests WHERE timestamp < ?1", [cutoff])? as u64;
    tx.commit()?;
    if removed > 0 {
        store.conn.execute_batch("VACUUM;")?;
    }
    Ok((total, removed))
}

/// 建表、补列、建索引并在结构发生变化时刷新 planner 统计。
///
/// 不读写 `PRAGMA user_version`：旧库按缺列／缺索引幂等补齐，历史行保持不变；
/// 结构齐全时只做几次廉价查询，不执行 `ANALYZE`。
fn ensure_schema(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS requests (
            id INTEGER PRIMARY KEY, request_key TEXT NOT NULL UNIQUE, timestamp INTEGER NOT NULL,
            model TEXT NOT NULL, provider TEXT NOT NULL, model_reference TEXT NOT NULL, api TEXT NOT NULL,
            project TEXT NOT NULL, cwd TEXT NOT NULL, purpose TEXT NOT NULL,
            session_file TEXT, entry_id TEXT, duration_ms REAL NOT NULL, ttft_ms REAL,
            input_tokens INTEGER, output_tokens INTEGER, cached_tokens INTEGER,
            status TEXT NOT NULL CHECK(status IN ('ok','failed','aborted')), stop_reason TEXT, error_message TEXT,
            cache_write_tokens INTEGER, cost_json TEXT, rate_json TEXT
        );
        CREATE TABLE IF NOT EXISTS request_payloads (
            request_id INTEGER PRIMARY KEY REFERENCES requests(id), output_json TEXT NOT NULL,
            session_entry_json TEXT, request_json TEXT, skills_json TEXT
        );
        CREATE TABLE IF NOT EXISTS tool_calls (
            id INTEGER PRIMARY KEY, request_key TEXT, session_file TEXT NOT NULL, call_id TEXT NOT NULL,
            tool TEXT NOT NULL, timestamp INTEGER NOT NULL,
            status TEXT NOT NULL CHECK(status IN ('ok','error','cancelled')),
            error_message TEXT, duration_ms REAL NOT NULL, UNIQUE(session_file, call_id)
        );",
    )?;
    let mut changed = false;
    for (name, ddl) in [
        (
            "requests_time",
            "CREATE INDEX IF NOT EXISTS requests_time ON requests(timestamp DESC,id DESC)",
        ),
        (
            "requests_failed_time",
            "CREATE INDEX IF NOT EXISTS requests_failed_time ON requests(timestamp DESC,id DESC) WHERE status='failed'",
        ),
        (
            "requests_session",
            "CREATE INDEX IF NOT EXISTS requests_session ON requests(session_file)",
        ),
        (
            "tool_calls_time",
            "CREATE INDEX IF NOT EXISTS tool_calls_time ON tool_calls(timestamp DESC,id DESC)",
        ),
        (
            "tool_calls_error_time",
            "CREATE INDEX IF NOT EXISTS tool_calls_error_time ON tool_calls(timestamp DESC,id DESC) WHERE status='error'",
        ),
    ] {
        changed |= !index_exists(&tx, name)?;
        tx.execute_batch(ddl)?;
    }
    let mut requests = table_columns(&tx, "requests")?;
    let mut payloads = table_columns(&tx, "request_payloads")?;
    changed |= ensure_column(
        &tx,
        "requests",
        &mut requests,
        "cache_write_tokens",
        "INTEGER",
    )?;
    changed |= ensure_column(&tx, "requests", &mut requests, "cost_json", "TEXT")?;
    changed |= ensure_column(&tx, "requests", &mut requests, "rate_json", "TEXT")?;
    changed |= ensure_column(
        &tx,
        "request_payloads",
        &mut payloads,
        "request_json",
        "TEXT",
    )?;
    changed |= ensure_column(
        &tx,
        "request_payloads",
        &mut payloads,
        "skills_json",
        "TEXT",
    )?;
    if changed {
        // 只在新结构第一次出现时给 planner 补统计，之后每次打开不写库。
        tx.execute_batch("ANALYZE requests; ANALYZE tool_calls;")?;
    }
    tx.commit()?;
    Ok(())
}

fn index_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1",
        [name],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
}

fn table_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns)
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    columns: &mut Vec<String>,
    column: &str,
    kind: &str,
) -> rusqlite::Result<bool> {
    if columns.iter().any(|name| name == column) {
        return Ok(false);
    }
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
    columns.push(column.to_owned());
    Ok(true)
}

pub(crate) struct Store {
    pub conn: Connection,
    models_path: PathBuf,
}
impl Store {
    fn session_cost(&self, session: &Path) -> Result<pp_config::task_status::HistoricalCost> {
        let scope = SessionScope::new(
            session.to_string_lossy().into_owned(),
            format!(
                "{}/",
                session
                    .with_extension("subagents")
                    .join("sessions")
                    .display()
            ),
        )?;
        let mut statement = self.conn.prepare(&session_requests_sql("cost_json"))?;
        let mut rows = statement.query(params![scope.root, scope.prefix, scope.upper])?;
        let mut result = pp_config::task_status::HistoricalCost::default();
        while let Some(row) = rows.next()? {
            let cost: Option<String> = row.get(0)?;
            match cost.as_deref().map(serde_json::from_str::<CostSnapshot>) {
                Some(Ok(cost)) if cost.total().is_finite() => {
                    result.usd = Some(result.usd.unwrap_or(0.0) + cost.total());
                }
                // NULL 或损坏的快照：只标记覆盖不完整，不臆造金额。
                _ => result.incomplete = true,
            }
        }
        Ok(result)
    }

    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_models(path, pp_config::config::global_dir()?.join("models.yml"))
    }
    fn open_with_models(path: &Path, models_path: PathBuf) -> Result<Self> {
        let dir = path.parent().context("statistics directory unavailable")?;
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        ensure!(
            !fs::symlink_metadata(dir)?.file_type().is_symlink(),
            "statistics directory must not be a symlink"
        );
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path)?;
        ensure!(
            file.metadata()?.is_file(),
            "statistics database must be a file"
        );
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        ensure_schema(&mut conn)?;
        Ok(Self { conn, models_path })
    }
    #[cfg(test)]
    pub fn insert(&mut self, record: &RequestRow, output: &Value) -> Result<()> {
        self.insert_with_context(record, output, None)
    }
    pub fn insert_with_context(
        &mut self,
        record: &RequestRow,
        output: &Value,
        context: Option<&RequestSnapshot>,
    ) -> Result<()> {
        let cost_json = record
            .cost
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let rate_json = record
            .rates
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let tx = self.conn.transaction()?;
        let inserted = tx.execute("INSERT INTO requests (request_key,timestamp,model,provider,model_reference,api,project,cwd,purpose,session_file,duration_ms,ttft_ms,input_tokens,output_tokens,cached_tokens,status,stop_reason,error_message,cache_write_tokens,cost_json,rate_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21) ON CONFLICT(request_key) DO NOTHING",params![record.request_key,record.timestamp,record.model,record.provider,record.model_reference,record.api,record.project,record.cwd,record.purpose,record.session_file,record.duration_ms,record.ttft_ms,record.input_tokens,record.output_tokens,record.cached_tokens,record.status,record.stop_reason,record.error_message,record.cache_write_tokens,cost_json,rate_json])?;
        if inserted != 0 {
            tx.execute(
                "INSERT INTO request_payloads(request_id,output_json,request_json,skills_json) VALUES (?1,?2,?3,?4)",
                params![tx.last_insert_rowid(), serde_json::to_string(output)?, context.map(|c| serde_json::to_string(&c.request)).transpose()?, context.map(|c| serde_json::to_string(&c.skills)).transpose()?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn link(&mut self, key: &str, file: &Path, entries: &[Value]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let entry_id = entries.first().and_then(|e| e["id"].as_str());
        tx.execute("UPDATE requests SET session_file=?1,entry_id=COALESCE(?2,entry_id) WHERE request_key=?3",params![file.to_string_lossy(),entry_id,key])?;
        if !entries.is_empty() {
            tx.execute("UPDATE request_payloads SET session_entry_json=?1 WHERE request_id=(SELECT id FROM requests WHERE request_key=?2)",params![serde_json::to_string(entries)?,key])?;
        }
        tx.commit()?;
        Ok(())
    }
    /// 幂等写入一次工具执行：同一会话内同一个 call ID 只保留最后一次结果。
    pub(crate) fn tool(&mut self, call: &ToolCall<'_>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tool_calls (request_key,session_file,call_id,tool,timestamp,status,error_message,duration_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(session_file,call_id) DO UPDATE SET
             request_key=COALESCE(excluded.request_key,tool_calls.request_key),tool=excluded.tool,
             timestamp=excluded.timestamp,status=excluded.status,
             error_message=excluded.error_message,duration_ms=excluded.duration_ms",
            params![
                call.request_key,
                call.session_file.to_string_lossy(),
                call.call_id,
                call.tool,
                call.timestamp,
                call.status.as_str(),
                call.error_message,
                call.duration_ms
            ],
        )?;
        Ok(())
    }
    pub fn latest(&self, since: i64, until: i64, limit: usize) -> Result<Vec<RequestRow>> {
        Ok(self
            .latest_with_context(since, until, limit)?
            .into_iter()
            .map(|(row, _)| row)
            .collect())
    }
    pub fn latest_with_context(
        &self,
        since: i64,
        until: i64,
        limit: usize,
    ) -> Result<Vec<(RequestRow, bool)>> {
        Ok(self
            .conn
            .prepare(&latest_sql())?
            .query_map(params![since, until, limit as i64], |r| {
                Ok((row(r)?, r.get(23)?))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn detail(&self, id: i64) -> Result<Option<Value>> {
        let record = self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM requests WHERE id=?1"),
                [id],
                row,
            )
            .optional()?;
        let Some(record) = record else {
            return Ok(None);
        };
        let (output, entry, request, skills): (String, Option<String>, Option<String>, Option<String>) = self.conn.query_row(
            "SELECT output_json,session_entry_json,request_json,skills_json FROM request_payloads WHERE request_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let request_context = request
            .map(|request| -> Result<RequestSnapshot> {
                Ok(RequestSnapshot {
                    request: serde_json::from_str(&request)?,
                    skills: skills
                        .map(|skills| serde_json::from_str(&skills))
                        .transpose()?
                        .unwrap_or_default(),
                })
            })
            .transpose()?;
        Ok(Some(
            json!({"request":record.value()?,"output_message":serde_json::from_str::<Value>(&output)?,"session_entry":entry.map(|s|serde_json::from_str::<Value>(&s)).transpose()?,"request_context":request_context}),
        ))
    }
    pub fn overview(&self, range: Range, now: i64) -> Result<Value> {
        let mut value = self.analytics(range, now)?;
        value["latest"] = json!(
            self.latest(range.since(now), now, 12)?
                .iter()
                .map(|r| r.value())
                .collect::<Result<Vec<_>>>()?
        );
        Ok(value)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Range {
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    All,
}
impl Range {
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "1h" => Self::Hour,
            "24h" => Self::Day,
            "7d" => Self::Week,
            "30d" => Self::Month,
            "90d" => Self::Quarter,
            "all" => Self::All,
            _ => anyhow::bail!("invalid time range"),
        })
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Hour => "1h",
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "30d",
            Self::Quarter => "90d",
            Self::All => "all",
        }
    }
    pub fn since(self, now: i64) -> i64 {
        let hours = match self {
            Self::Hour => 1,
            Self::Day => 24,
            Self::Week => 168,
            Self::Month => 720,
            Self::Quarter => 2160,
            Self::All => return 0,
        };
        now.saturating_sub(hours * 3600000)
    }
}

static WARNED: AtomicBool = AtomicBool::new(false);
pub fn warn() {
    tracing::warn!(event = "statistics_unavailable");
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("warning: statistics unavailable; model tasks are unaffected");
    }
}
pub fn save(
    context: &RequestContext,
    model: &str,
    api: &str,
    observation: Observation,
    started: Instant,
    outcome: &Result<impl Sized>,
) -> Option<String> {
    let result = (|| -> Result<String> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)
            .map_err(|_| anyhow::anyhow!("statistics random source unavailable"))?;
        let key = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let status = if outcome.is_ok() {
            "ok"
        } else if outcome
            .as_ref()
            .err()
            .is_some_and(|e| e.is::<pp_core::cancellation::Cancelled>())
        {
            "aborted"
        } else {
            "failed"
        };
        let mut record = RequestRow {
            id: 0,
            request_key: key.clone(),
            timestamp: observation.timestamp,
            model: model.into(),
            provider: context
                .model_reference
                .split('/')
                .next()
                .unwrap_or_default()
                .into(),
            model_reference: context.model_reference.clone(),
            api: api.into(),
            project: context
                .cwd
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into(),
            cwd: context.cwd.to_string_lossy().into(),
            purpose: context.purpose.into(),
            session_file: context
                .session_file
                .as_ref()
                .map(|p| p.to_string_lossy().into()),
            entry_id: None,
            duration_ms: started.elapsed().as_secs_f64() * 1000.0,
            ttft_ms: observation.ttft_ms.map(|n| n as f64),
            input_tokens: observation
                .usage
                .map(|u| i64::try_from(u.input_tokens))
                .transpose()?,
            output_tokens: observation
                .usage
                .map(|u| i64::try_from(u.output_tokens))
                .transpose()?,
            cached_tokens: observation
                .usage
                .and_then(|u| u.cached_tokens)
                .map(i64::try_from)
                .transpose()?,
            cache_write_tokens: observation
                .usage
                .and_then(|u| u.cache_write_tokens)
                .map(i64::try_from)
                .transpose()?,
            status: status.into(),
            stop_reason: observation.stop_reason,
            error_message: outcome.as_ref().err().map(|e| format!("{e:#}")),
            cost: None,
            rates: None,
        };
        let output = json!({"native":observation.output,"text":observation.text,"thinking":observation.thinking,"partial":outcome.is_err()});
        let mut store = Store::open(&context.database)?;
        // 费用与费率在采集时按当次模型配置固定下来，读取路径不再回算。
        let (cost, rates) = Rates::load(&store.models_path).snapshot(
            &record.model_reference,
            &record.model,
            record.input_tokens,
            record.output_tokens,
            record.cached_tokens,
            record.cache_write_tokens,
        );
        record.cost = cost;
        record.rates = rates;
        store.insert_with_context(&record, &output, observation.request_context.as_ref())?;
        if let Some(unlinked) = &context.unlinked_requests
            && context.session_file.is_none()
        {
            unlinked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(key.clone());
        }
        Ok(key)
    })();
    match result {
        Ok(key) => Some(key),
        Err(_) => {
            warn();
            None
        }
    }
}
pub fn link(database: &Path, key: &str, file: &Path, entries: &[Value]) {
    if (|| -> Result<()> { Store::open(database)?.link(key, file, entries) })().is_err() {
        warn();
    }
}
/// 记录一次工具执行；存储失败只提示，不改变工具结果。
pub fn tool(database: &Path, call: &ToolCall<'_>) {
    if (|| -> Result<()> { Store::open(database)?.tool(call) })().is_err() {
        warn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    fn open_store(path: &Path) -> Result<Store> {
        Store::open_with_models(path, path.with_file_name("models.yml"))
    }
    fn models_with_cost(cost: Value) -> String {
        json!({"providers":{"fixture": {"api_key":"secret-marker-fixture-key",
            "models":{"primary": {"model":"fixture-model","cost":cost}}}}})
        .to_string()
    }
    fn fixture(key: &str, timestamp: i64) -> RequestRow {
        RequestRow {
            id: 0,
            request_key: key.into(),
            timestamp,
            model: "fixture-model".into(),
            provider: "fixture".into(),
            model_reference: "fixture/primary".into(),
            api: "responses".into(),
            project: "project".into(),
            cwd: "/tmp/project".into(),
            purpose: "task".into(),
            session_file: None,
            entry_id: None,
            duration_ms: 2000.0,
            ttft_ms: Some(500.0),
            input_tokens: Some(100),
            output_tokens: Some(20),
            cached_tokens: Some(80),
            cache_write_tokens: Some(0),
            status: "ok".into(),
            stop_reason: Some("stop".into()),
            error_message: None,
            cost: None,
            rates: None,
        }
    }
    /// 按给定费率写入采集时会写入的费用／费率快照。
    fn snapshot_for(rates: &Rates, record: &mut RequestRow) {
        let (cost, rate) = rates.snapshot(
            &record.model_reference,
            &record.model,
            record.input_tokens,
            record.output_tokens,
            record.cached_tokens,
            record.cache_write_tokens,
        );
        record.cost = cost;
        record.rates = rate;
    }
    #[test]
    fn session_cost_uses_root_and_child_records_and_recorded_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        fs::write(
            &store.models_path,
            models_with_cost(json!({"input":2,"cacheRead":0.2,"output":8})),
        )
        .unwrap();
        let rates = Rates::load(&store.models_path);
        let root = dir.path().join("root.jsonl");
        let child = dir.path().join("root.subagents/sessions/child.jsonl");
        let other = dir.path().join("root.subagents/sessions-other/child.jsonl");
        for (key, file) in [("root", &root), ("child", &child), ("other", &other)] {
            let mut record = fixture(key, 1000);
            record.session_file = Some(file.to_string_lossy().into_owned());
            snapshot_for(&rates, &mut record);
            assert!(record.cost.is_some());
            store.insert(&record, &Value::Null).unwrap();
        }
        // root 与 child 各 20 未缓存输入 + 80 缓存读 + 20 输出；sessions-other 不计入。
        let history = store.session_cost(&root).unwrap();
        assert!(!history.incomplete);
        assert!((history.usd.unwrap() - 0.000432).abs() < 1e-12);

        // 没有费用快照的旧记录只标记覆盖不完整，金额不变。
        let mut legacy = fixture("legacy", 1001);
        legacy.session_file = Some(root.to_string_lossy().into_owned());
        store.insert(&legacy, &Value::Null).unwrap();
        let history = store.session_cost(&root).unwrap();
        assert!(history.incomplete);
        assert!((history.usd.unwrap() - 0.000432).abs() < 1e-12);

        // 改写 models.yml 不重算历史金额。
        fs::write(
            &store.models_path,
            models_with_cost(json!({"input":0,"cacheRead":0,"output":0})),
        )
        .unwrap();
        let history = store.session_cost(&root).unwrap();
        assert!(history.incomplete);
        assert!((history.usd.unwrap() - 0.000432).abs() < 1e-12);

        assert_eq!(
            store
                .session_cost(&dir.path().join("missing.jsonl"))
                .unwrap(),
            Default::default()
        );
    }

    #[test]
    fn stats_context_list_marks_saved_first_requests_without_loading_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        let snapshot = RequestSnapshot {
            request: json!({"instructions":"private-system-marker","unknown":{"preserved":true}}),
            skills: Vec::new(),
        };
        let mut first = fixture("first", 1000);
        first.status = "failed".into();
        store
            .insert_with_context(&first, &Value::Null, Some(&snapshot))
            .unwrap();
        store.insert(&fixture("later", 1001), &Value::Null).unwrap();
        store.insert(&fixture("legacy", 900), &Value::Null).unwrap();
        let (status, _, body) = server::dispatch(&store, "/api/stats/requests?range=all", &[]);
        assert_eq!(status, 200);
        let list: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            list["requests"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| (
                    r["request_key"].as_str().unwrap(),
                    r["has_context"].as_bool().unwrap()
                ))
                .collect::<Vec<_>>(),
            vec![("later", false), ("first", true), ("legacy", false)]
        );
        assert!(!body.contains("private-system-marker"));
        assert!(!body.contains("request_context"));
        assert!(!store.latest_with_context(1001, 2000, 1).unwrap()[0].1);
        assert!(!store.latest_with_context(0, 2000, 1).unwrap()[0].1);
        store
            .conn
            .execute_batch("DELETE FROM request_payloads WHERE request_id=(SELECT id FROM requests WHERE request_key='first'); DELETE FROM requests WHERE request_key='first';")
            .unwrap();
        assert!(
            store
                .latest_with_context(0, 2000, 500)
                .unwrap()
                .iter()
                .all(|(_, has_context)| !has_context)
        );
    }

    #[test]
    fn stats_payload_columns_restore_and_snapshots_deduplicate_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        let mut store = open_store(&path).unwrap();
        let record = fixture("legacy", 1000);
        store
            .insert(&record, &json!({"text":"old output"}))
            .unwrap();
        store
            .link(
                "legacy",
                Path::new("/missing/session.jsonl"),
                &[json!({"id":"retained-entry"})],
            )
            .unwrap();
        store.conn.execute_batch("ALTER TABLE request_payloads DROP COLUMN request_json; ALTER TABLE request_payloads DROP COLUMN skills_json;").unwrap();
        drop(store);
        let mut store = open_store(&path).unwrap();
        let old = store.detail(1).unwrap().unwrap();
        assert_eq!(old["output_message"]["text"], "old output");
        assert_eq!(old["session_entry"], json!([{"id":"retained-entry"}]));
        assert!(old["request_context"].is_null());
        let snapshot = RequestSnapshot {
            request: json!({"instructions":"full\n\t<script>&</script>","input":[{"type":"input_image","image_url":"data:image/png;base64,AAAA","future_field":{"nested":[1,null]}}],"tools":[{"type":"function","name":"read","parameters":{"additionalProperties":false}}],"unknown":{"preserved":true}}),
            skills: vec![pp_config::skills::SkillHeader {
                name: "available".into(),
                description: "header".into(),
                uri: "skill://available".into(),
            }],
        };
        let record = fixture("snapshot", 1001);
        store
            .insert_with_context(&record, &json!({"text":"new output"}), Some(&snapshot))
            .unwrap();
        store
            .insert_with_context(&record, &json!({"text":"must not overwrite"}), None)
            .unwrap();
        assert_eq!(store.latest(0, 2000, 500).unwrap().len(), 2);
        drop(store);
        let store = open_store(&path).unwrap();
        let detail = store.detail(2).unwrap().unwrap();
        assert_eq!(
            detail["request_context"],
            serde_json::to_value(&snapshot).unwrap()
        );
        assert_eq!(detail["output_message"]["text"], "new output");
        let (status, _, body) = server::dispatch(&store, "/api/stats/requests?range=all", &[]);
        assert_eq!(status, 200);
        assert!(!body.contains("request_context"));
        assert!(!body.contains("future_field"));
    }

    #[test]
    fn stats_analytics_groups_all_rows_and_prices_cached_input_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        fs::write(
            dir.path().join("models.yml"),
            r#"# 价格与普通模型配置共用
providers:
  fixture:
    api_key: secret-marker-fixture-key
    models:
      primary:
        model: fixture-model
        cost: {input: 2, cacheRead: 0.2, output: 8}
      unpriced:
        model: fixture-model
  other:
    api_key: secret-marker-other-key
    models:
      primary:
        model: fixture-model
        cost: {input: 0, cacheRead: 0, output: 0}
"#,
        )
        .unwrap();
        let now = 1_800_000_000_000;
        let rates = Rates::load(&store.models_path);
        for n in 0..2005 {
            let mut record = fixture(&format!("priced-{n}"), now);
            snapshot_for(&rates, &mut record);
            store.insert(&record, &Value::Null).unwrap();
        }
        let mut other = fixture("other", now - 86_400_000);
        other.provider = "other".into();
        other.model_reference = "other/primary".into();
        other.status = "failed".into();
        snapshot_for(&rates, &mut other);
        store.insert(&other, &Value::Null).unwrap();
        let mut unknown = fixture("unknown", now);
        unknown.cached_tokens = None;
        unknown.ttft_ms = None;
        unknown.status = "aborted".into();
        snapshot_for(&rates, &mut unknown);
        assert!(unknown.cost.is_none());
        assert!(unknown.rates.is_some());
        store.insert(&unknown, &Value::Null).unwrap();
        store
            .insert(&fixture("outside", now + 1), &Value::Null)
            .unwrap();
        // 配置引用了没有价格的 model 时既无费用也无费率快照。
        let (alias_cost, alias_rate) = rates.snapshot(
            "fixture/unpriced",
            "fixture-model",
            Some(100),
            Some(20),
            Some(80),
            Some(0),
        );
        assert!(alias_cost.is_none());
        assert!(alias_rate.is_none());
        let result = store.analytics(Range::Day, now).unwrap();
        let s = &result["summary"];
        assert_eq!(s["requests"], 2007);
        assert_eq!(s["priced"], 2006);
        assert_eq!(s["unpriced"], 1);
        assert_eq!(s["failed"], 1);
        assert_eq!(s["aborted"], 1);
        assert_eq!(result["models"].as_array().unwrap().len(), 2);
        assert_eq!(result["providers"].as_array().unwrap().len(), 2);
        assert_eq!(result["active_days"], 2);
        let expected = 2005.0 * (20.0 * 2.0 + 80.0 * 0.2 + 20.0 * 8.0) / 1e6;
        assert!((s["cost"].as_f64().unwrap() - expected).abs() < 1e-10);
        for field in ["models", "providers", "buckets", "daily", "hours"] {
            let rows = result[field].as_array().unwrap();
            assert_eq!(
                rows.iter()
                    .map(|r| r["requests"].as_u64().unwrap())
                    .sum::<u64>(),
                2007
            );
            assert!(
                (rows.iter().filter_map(|r| r["cost"].as_f64()).sum::<f64>() - expected).abs()
                    < 1e-10
            );
        }
        let detail = store.detail(1).unwrap().unwrap();
        assert!((detail["request"]["cost"].as_f64().unwrap() - expected / 2005.0).abs() < 1e-10);
        assert_eq!(result["providers"][0]["models"], 1);
        assert_eq!(result["summary"]["avg_ttft_ms"], 500.0);
        assert_eq!(result["models"][0]["buckets"][0]["tokens_per_second"], 10.0);
    }
    #[test]
    fn stats_cache_write_column_restore_does_not_invent_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        let mut store = open_store(&path).unwrap();
        let mut failed = fixture("old", 1000);
        failed.status = "failed".into();
        failed.error_message = Some("retained failure".into());
        store.insert(&failed, &json!({"text":"retained"})).unwrap();
        let snapshot = json!({"id":"old-entry","type":"message","message":{"role":"assistant","content":"retained snapshot"}});
        store
            .link(
                "old",
                Path::new("/missing/session.jsonl"),
                std::slice::from_ref(&snapshot),
            )
            .unwrap();
        store
            .conn
            .execute_batch(
                "DROP INDEX requests_failed_time; ALTER TABLE requests DROP COLUMN cache_write_tokens; ALTER TABLE request_payloads DROP COLUMN request_json; ALTER TABLE request_payloads DROP COLUMN skills_json;",
            )
            .unwrap();
        drop(store);
        let mut store = open_store(&path).unwrap();
        let old = store.detail(1).unwrap().unwrap();
        assert_eq!(old["request"]["cached_tokens"], 80);
        assert!(old["request"]["cache_write_tokens"].is_null());
        assert_eq!(old["output_message"]["text"], "retained");
        assert_eq!(old["request"]["status"], "failed");
        assert_eq!(old["request"]["entry_id"], "old-entry");
        assert_eq!(old["session_entry"], json!([snapshot]));
        let errors = store.errors(Range::All, 2000, 500).unwrap();
        assert_eq!(errors["summary"]["requests"], 1);
        assert_eq!(errors["requests"][0]["error_message"], "retained failure");
        let mut new = fixture("new", 1001);
        new.cache_write_tokens = Some(10);
        store.insert(&new, &Value::Null).unwrap();
        drop(store);
        let store = open_store(&path).unwrap();
        assert_eq!(
            store.latest(0, 2000, 500).unwrap()[0].cache_write_tokens,
            Some(10)
        );
    }

    #[test]
    fn stats_failed_index_restore_preserves_error_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        let mut store = open_store(&path).unwrap();
        for (key, status) in [
            ("failed-one", "failed"),
            ("ok", "ok"),
            ("aborted", "aborted"),
            ("failed-two", "failed"),
        ] {
            let mut record = fixture(key, 1000);
            record.status = status.into();
            record.error_message = (status == "failed").then(|| format!("{key} error"));
            store
                .insert(&record, &json!({"text":format!("{key} full output")}))
                .unwrap();
        }
        store.link("failed-one", Path::new("/missing/session.jsonl"), &[json!({"id":"entry","type":"message","message":{"role":"assistant","content":"retained snapshot"}})]).unwrap();
        let errors = store.errors(Range::All, 2000, 500).unwrap();
        assert_eq!(errors["summary"]["requests"], 2);
        let latest = serde_json::to_value(store.latest(0, 2000, 500).unwrap()).unwrap();
        let details = (1..=4)
            .map(|id| store.detail(id).unwrap().unwrap())
            .collect::<Vec<_>>();
        store.conn.execute_batch("DROP INDEX requests_failed_time; ALTER TABLE request_payloads DROP COLUMN request_json; ALTER TABLE request_payloads DROP COLUMN skills_json;").unwrap();
        drop(store);
        for _ in 0..2 {
            let store = open_store(&path).unwrap();
            assert_eq!(store.errors(Range::All, 2000, 500).unwrap(), errors);
            assert_eq!(
                serde_json::to_value(store.latest(0, 2000, 500).unwrap()).unwrap(),
                latest
            );
            for (id, detail) in (1..=4).zip(&details) {
                assert_eq!(&store.detail(id).unwrap().unwrap(), detail);
            }
        }
    }

    #[test]
    fn stats_cache_write_cost_and_aggregates_preserve_unknown_zero_and_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        let path = dir.path().join("models.yml");
        fs::write(
            &path,
            models_with_cost(json!({"input":2,"cacheRead":0.2,"cacheWrite":2.5,"output":8})),
        )
        .unwrap();
        let rates = Rates::load(&path);
        let mut record = fixture("written", 1000);
        record.cache_write_tokens = Some(10);
        let (cost, rate) = rates.snapshot(
            "fixture/primary",
            "fixture-model",
            Some(100),
            Some(20),
            Some(80),
            Some(10),
        );
        let cost = cost.unwrap();
        assert_eq!(cost.cache_write, 0.000025);
        assert!((cost.total() - 0.000221).abs() < 1e-10);
        assert_eq!(rate.as_ref().unwrap().cache_write, Some(2.5));
        record.cost = Some(cost);
        record.rates = rate;
        store.insert(&record, &Value::Null).unwrap();
        record.request_key = "zero".into();
        record.cache_write_tokens = Some(0);
        snapshot_for(&rates, &mut record);
        store.insert(&record, &Value::Null).unwrap();
        record.request_key = "unknown".into();
        record.cache_write_tokens = None;
        snapshot_for(&rates, &mut record);
        assert!(record.cost.is_none());
        assert!(record.rates.is_some());
        store.insert(&record, &Value::Null).unwrap();
        for written in [-1, 21, i64::MAX] {
            let (cost, rate) = rates.snapshot(
                "fixture/primary",
                "fixture-model",
                Some(100),
                Some(20),
                Some(80),
                Some(written),
            );
            assert!(cost.is_none(), "{written}");
            assert!(rate.is_some(), "{written}");
        }
        let result = store.analytics(Range::All, 2000).unwrap();
        for group in [
            result["summary"].clone(),
            result["models"][0].clone(),
            result["providers"][0].clone(),
            result["buckets"][0].clone(),
            result["daily"][0].clone(),
        ] {
            assert_eq!(group["cache_write_tokens"], 10);
            assert_eq!(group["cache_write_known"], 2);
            assert_eq!(group["uncached_tokens"], 30);
            assert_eq!(group["tokens"], 360);
            assert_eq!(group["priced"], 2);
            assert_eq!(group["unpriced"], 1);
            assert_eq!(group["cost_components"]["cache_write"], 0.000025);
            assert!((group["cost"].as_f64().unwrap() - 0.000437).abs() < 1e-10);
        }
        fs::write(
            &path,
            models_with_cost(json!({"input":2,"cacheRead":0.2,"output":8})),
        )
        .unwrap();
        let rates = Rates::load(&path);
        let snapshot = |written: Option<i64>| {
            rates.snapshot(
                "fixture/primary",
                "fixture-model",
                Some(100),
                Some(20),
                Some(80),
                written,
            )
        };
        assert!(snapshot(Some(10)).0.is_none());
        assert!(snapshot(Some(0)).0.is_some());
        // 费率没有写入价时，写入量未知不影响总价，不能整条请求记为未定价。
        assert!((snapshot(None).0.unwrap().total() - 0.000216).abs() < 1e-10);
        fs::write(
            &path,
            models_with_cost(json!({"input":0,"cacheRead":0,"cacheWrite":0,"output":0})),
        )
        .unwrap();
        let rates = Rates::load(&path);
        let (cost, _) = rates.snapshot(
            "fixture/primary",
            "fixture-model",
            Some(100),
            Some(20),
            Some(80),
            Some(10),
        );
        record.cost = cost;
        assert_eq!(record.value().unwrap()["cost"], 0.0);
        let empty = store.analytics(Range::Hour, 10_000_000).unwrap();
        assert_eq!(empty["summary"]["cache_write_tokens"], 0);
    }

    #[test]
    fn stats_rates_preserve_unknown_and_reject_invalid_config() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_store(&dir.path().join("stats.db")).unwrap();
        let path = dir.path().join("models.yml");
        let unpriced = |rates: &Rates| {
            rates.snapshot(
                "fixture/primary",
                "fixture-model",
                Some(100),
                Some(20),
                Some(80),
                Some(0),
            )
        };
        // 没有 models.yml 时只保留内置价格表。
        let rates = Rates::load(&path);
        assert!(unpriced(&rates).0.is_none());
        assert!(unpriced(&rates).1.is_none());
        let (cost, rate) = rates.snapshot(
            "openai/gpt-5.5",
            "openai/gpt-5.5",
            Some(1_000_000),
            Some(0),
            Some(0),
            Some(0),
        );
        assert_eq!(rate.unwrap().input, 5.0);
        assert!((cost.unwrap().total() - 5.0).abs() < 1e-12);

        // 配置无法解析时完全不计价，连内置价格表也不用。
        for invalid in [
            models_with_cost(json!({"input":-1,"cacheRead":0,"output":1})),
            models_with_cost(json!({"input":1,"output":1})),
            models_with_cost(json!({"input":1,"cacheRead":0,"output":1,"typo":"secret-marker"})),
            "invalid secret-marker".into(),
        ] {
            fs::write(&path, invalid).unwrap();
            let rates = Rates::load(&path);
            assert_eq!(unpriced(&rates), (None, None));
            assert_eq!(
                rates.snapshot(
                    "openai/gpt-5.5",
                    "openai/gpt-5.5",
                    Some(1_000_000),
                    Some(0),
                    Some(0),
                    Some(0),
                ),
                (None, None)
            );
            assert!(store.overview(Range::All, 2000).is_ok());
        }

        fs::write(
            &path,
            models_with_cost(json!({"input":2,"cacheRead":0.2,"output":8})),
        )
        .unwrap();
        let rates = Rates::load(&path);
        let (cost, rate) = unpriced(&rates);
        assert!((cost.unwrap().total() - 0.000216).abs() < 1e-10);
        assert_eq!(rate.unwrap().cached_input, 0.2);

        fs::write(
            &path,
            models_with_cost(json!({"input":0,"cacheRead":0,"output":0})),
        )
        .unwrap();
        let rates = Rates::load(&path);
        assert_eq!(unpriced(&rates).0.unwrap().total(), 0.0);
        // 缓存读超过输入、缓存读未知、输出未知都不能按零计价。
        for (input, output, cached) in [
            (Some(100), Some(20), Some(101)),
            (Some(100), Some(20), None),
            (Some(100), None, Some(80)),
        ] {
            let (cost, rate) = rates.snapshot(
                "fixture/primary",
                "fixture-model",
                input,
                output,
                cached,
                Some(0),
            );
            assert!(cost.is_none());
            assert!(rate.is_some());
        }
        let empty = store.analytics(Range::All, 2000).unwrap();
        assert_eq!(empty["summary"]["cost"], 0.0);
        assert_eq!(empty["summary"]["unpriced"], 0);
        assert_eq!(empty["summary"]["tokens"], 0);
    }
    #[test]
    fn stats_traces_fold_subagents_and_aggregate_tools() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        let now = 10_000_000;
        let root = dir.path().join("root.jsonl");
        let child = dir.path().join("root.subagents/sessions/child.jsonl");
        let root_file = root.to_string_lossy().into_owned();
        let child_file = child.to_string_lossy().into_owned();
        for (key, file, timestamp, duration_ms) in [
            ("root-1", &root_file, now - 10_000, 2_000.0),
            ("child-1", &child_file, now - 5_000, 1_000.0),
            ("root-2", &root_file, now - 2_000, 3_000.0),
        ] {
            let mut record = fixture(key, timestamp);
            record.session_file = Some(file.clone());
            record.duration_ms = duration_ms;
            store.insert(&record, &Value::Null).unwrap();
        }
        for (request_key, file, call_id, status, error, timestamp, duration_ms) in [
            (
                "root-1",
                &root,
                "call-1",
                ToolStatus::Ok,
                None,
                now - 9_500,
                500.0,
            ),
            (
                "child-1",
                &child,
                "call-2",
                ToolStatus::Error,
                Some("boom"),
                now - 4_500,
                250.0,
            ),
        ] {
            store
                .tool(&ToolCall {
                    request_key: Some(request_key),
                    session_file: file,
                    call_id,
                    tool: "bash",
                    status,
                    error_message: error,
                    timestamp,
                    duration_ms,
                })
                .unwrap();
        }
        let titles = HashMap::from([("root", "修复 traces 会话标题")]);
        let traces = store.traces(Range::Hour, now, 500, &titles).unwrap();
        assert_eq!(traces["total"], 1);
        let session = &traces["sessions"][0];
        assert_eq!(session["session_file"], root_file);
        assert_eq!(session["title"], "修复 traces 会话标题");
        assert_eq!(session["requests"], 3);
        assert_eq!(session["agents"], 1);
        assert_eq!(session["tool_calls"], 2);
        let untitled = store
            .traces(Range::Hour, now, 500, &HashMap::new())
            .unwrap();
        assert!(untitled["sessions"][0].get("title").is_none());
        let id = session["id"].as_i64().unwrap();
        let trace = store.trace(id, now, &titles).unwrap().unwrap();
        assert_eq!(trace["title"], "修复 traces 会话标题");
        assert_eq!(trace["session_file"], root_file);
        assert_eq!(trace["summary"]["requests"], 3);
        assert_eq!(trace["summary"]["agents"], 1);
        assert_eq!(trace["summary"]["tool_calls"], 2);
        assert_eq!(trace["summary"]["model_ms"], 6_000.0);
        assert_eq!(trace["summary"]["tool_ms"], 750.0);
        assert_eq!(trace["summary"]["wall_ms"], 11_000.0);
        assert_eq!(trace["summary"]["idle_ms"], 4_250.0);
        assert_eq!(trace["requests"].as_array().unwrap().len(), 3);
        assert_eq!(trace["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(trace["tool_stats"][0]["tool"], "bash");
        assert_eq!(trace["tool_stats"][0]["calls"], 2);
        assert_eq!(trace["tool_stats"][0]["errors"], 1);
        assert_eq!(trace["tool_stats"][0]["total_ms"], 750.0);
        assert_eq!(trace["tool_stats"][0]["max_ms"], 500.0);
        assert_eq!(
            trace["tool_calls"][1]["request_id"],
            trace["requests"][1]["id"]
        );
    }
    #[test]
    fn stats_server_serves_traces_assets() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_store(&dir.path().join("stats.db")).unwrap();
        for (path, kind) in [
            ("/traces.js", "text/javascript"),
            ("/traces.css", "text/css"),
        ] {
            let (status, content_type, body) = server::dispatch(&store, path, &[]);
            assert_eq!(status, 200, "{path}");
            assert_eq!(content_type, kind, "{path}");
            assert!(!body.is_empty(), "{path}");
        }
    }
    #[test]
    fn stats_activity_uses_complete_ranges_and_full_session_traces() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let mut failed = fixture("old-failure", now - 7_200_000);
        failed.status = "failed".into();
        failed.error_message = Some("HTTP 429 request 0123456789abcdef after 12ms".into());
        failed.session_file = Some("/missing/session.jsonl".into());
        store
            .insert(&failed, &json!({"text":"private-output"}))
            .unwrap();
        for n in 0..501 {
            let mut r = fixture(&format!("failure-{n}"), now - 1000);
            r.status = "failed".into();
            r.error_message = Some("HTTP 429 request fedcba9876543210 after 99ms".into());
            store.insert(&r, &Value::Null).unwrap();
        }
        let mut latest = fixture("latest", now);
        latest.session_file = failed.session_file.clone();
        store.insert(&latest, &Value::Null).unwrap();
        store
            .insert(&fixture("future", now + 1), &Value::Null)
            .unwrap();
        let errors = store.errors(Range::Day, now, 500).unwrap();
        assert_eq!(errors["summary"]["requests"], 502);
        assert_eq!(errors["signatures"].as_array().unwrap().len(), 1);
        assert_eq!(errors["signatures"][0]["requests"], 502);
        assert_eq!(errors["requests"].as_array().unwrap().len(), 500);
        assert_eq!(errors["has_more"], true);
        assert!(!errors.to_string().contains("private-output"));
        let traces = store
            .traces(Range::Hour, now, 500, &HashMap::new())
            .unwrap();
        assert_eq!(traces["total"], 502);
        assert_eq!(traces["sessions"][0]["requests"], 1);
        let id = traces["sessions"][0]["id"].as_i64().unwrap();
        let trace = store.trace(id, now, &HashMap::new()).unwrap().unwrap();
        assert_eq!(trace["summary"]["requests"], 2);
        assert_eq!(trace["requests"][0]["id"], 1);
        assert_eq!(trace["summary"]["wall_ms"], 7_202_000.0);
        assert_eq!(trace["summary"]["model_ms"], 4000.0);
        assert!(!trace.to_string().contains("private-output"));
        assert_eq!(
            store.detail(1).unwrap().unwrap()["output_message"]["text"],
            "private-output"
        );
        assert!(store.trace(9999, now, &HashMap::new()).unwrap().is_none());
        assert_eq!(
            store.trace(2, now, &HashMap::new()).unwrap().unwrap()["summary"]["requests"],
            1
        );
        let since = Range::Day.since(now);
        for (key, timestamp, status) in [
            ("before-range", since - 1, "failed"),
            ("at-lower", since, "failed"),
            ("at-upper-one", now, "failed"),
            ("at-upper-two", now, "failed"),
            ("after-range", now + 1, "failed"),
            ("in-range-ok", now - 500, "ok"),
            ("in-range-aborted", now - 500, "aborted"),
        ] {
            let mut record = fixture(key, timestamp);
            record.status = status.into();
            record.error_message = failed.error_message.clone();
            store.insert(&record, &Value::Null).unwrap();
        }
        let complete = store.errors(Range::Day, now, 2000).unwrap();
        let keys = complete["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["request_key"].as_str().unwrap())
            .collect::<Vec<_>>();
        let expected = ["at-upper-two".to_owned(), "at-upper-one".to_owned()]
            .into_iter()
            .chain((0..501).rev().map(|n| format!("failure-{n}")))
            .chain(["old-failure".to_owned(), "at-lower".to_owned()])
            .collect::<Vec<_>>();
        assert_eq!(keys, expected);
        assert_eq!(complete["summary"]["requests"], 505);
        let limited = store.errors(Range::Day, now, 500).unwrap();
        for field in ["summary", "signatures", "models"] {
            assert_eq!(limited[field], complete[field]);
        }
        assert_eq!(
            limited["requests"],
            json!(&complete["requests"].as_array().unwrap()[..500])
        );
        assert_eq!(limited["has_more"], true);
        for endpoint in ["errors", "traces"] {
            for query in [
                "range=bad",
                "range=all&range=24h",
                "limit=1",
                "unexpected=1",
            ] {
                assert_eq!(
                    server::dispatch(&store, &format!("/api/stats/{endpoint}?{query}"), &[]).0,
                    400
                );
            }
            assert_eq!(
                server::dispatch(
                    &store,
                    &format!("/api/stats/{endpoint}?range=all&limit=2000"),
                    &[]
                )
                .0,
                200
            );
        }
        assert_eq!(
            server::dispatch(&store, "/api/stats/traces/99999", &[]).0,
            404
        );
        assert_eq!(server::dispatch(&store, "/api/stats/traces/0", &[]).0, 400);
        assert_eq!(
            server::dispatch(&store, "/api/stats/traces/1?file=/tmp/private", &[]).0,
            400
        );
    }
    #[test]
    fn stats_insights_count_both_protocols_without_double_counting_or_leaking_payloads() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("models.yml"),
            models_with_cost(json!({"input":2.0,"cacheRead":0.2,"output":8.0})),
        )
        .unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let rates = Rates::load(&store.models_path);
        let completion = json!({"native":{"tool_calls":[
            {"function":{"name":"read","arguments":"private-arguments"}},
            {"function":{"name":"bash","arguments":"{}"}}
        ]},"text":"private-output"});
        let mut first = fixture("one", now - 1000);
        first.session_file = Some("/missing/session.jsonl".into());
        snapshot_for(&rates, &mut first);
        store.insert(&first, &completion).unwrap();
        store
            .link(
                "one",
                Path::new("/missing/session.jsonl"),
                &[json!({"id":"entry","message":{"content":[
                    {"type":"toolCall","name":"read","arguments":{"path":"private-arguments"}},
                    {"type":"toolCall","name":"bash","arguments":{}}
                ]}})],
            )
            .unwrap();
        let responses = json!({"native":[{"type":"function_call","name":"read","arguments":"{}"}]});
        for n in 0..501 {
            let mut r = fixture(&format!("response-{n}"), now);
            r.api = "responses".into();
            r.cwd = "/other/project".into();
            r.model_reference = "other/primary".into();
            r.input_tokens = None;
            r.output_tokens = None;
            r.cached_tokens = None;
            snapshot_for(&rates, &mut r);
            store.insert(&r, &responses).unwrap();
        }
        let mut failed = fixture("failed", now);
        failed.status = "aborted".into();
        snapshot_for(&rates, &mut failed);
        store.insert(&failed, &json!({"native":{"output":[{"type":"function_call","name":"rg","arguments":"中😀"}]}})).unwrap();
        store
            .insert(&fixture("future", now + 1), &responses)
            .unwrap();
        store
            .insert(&fixture("old", now - 3_600_001), &responses)
            .unwrap();
        let tools = store.tools(Range::Hour, now).unwrap();
        assert_eq!(tools["summary"]["calls"], 504);
        assert_eq!(tools["requests_with_tools"], 503);
        assert_eq!(tools["partial_requests"], 1);
        assert_eq!(tools["summary"]["priced"], 3);
        assert_eq!(tools["summary"]["unpriced"], 501);
        assert_eq!(tools["summary"]["tokens"], 240.0);
        assert!((tools["summary"]["cost"].as_f64().unwrap() - 0.000432).abs() < 1e-12);
        let read = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["tool"] == "read")
            .unwrap();
        assert_eq!(read["calls"], 502);
        assert_eq!(read["tokens"], 60.0);
        assert_eq!(read["models"].as_array().unwrap().len(), 2);
        assert!(tools["summary"]["errors"].is_null());
        assert!(!tools.to_string().contains("private-arguments"));
        assert!(!tools.to_string().contains("private-output"));
        let projects = store.projects(Range::Hour, now).unwrap();
        assert_eq!(projects["summary"]["requests"], 503);
        assert_eq!(projects["projects"].as_array().unwrap().len(), 2);
        let p = projects["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["key"] == "/tmp/project")
            .unwrap();
        assert_eq!(p["sessions"], 1);
        assert_eq!(p["requests_without_session"], 1);
        assert_eq!(p["requests"], 2);
        assert!(!projects.to_string().contains("private-output"));
        let empty = store.tools(Range::Hour, now - 7_200_000).unwrap();
        assert_eq!(empty["summary"]["cost"], 0.0);
        assert_eq!(empty["summary"]["tokens"], 0.0);
        assert!(empty["tools"].as_array().unwrap().is_empty());
    }
    #[test]
    fn stats_tool_executions_record_errors_survive_table_rebuild_and_stay_in_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        let mut store = open_store(&path).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let session = dir.path().join("session.jsonl");
        let mut first = fixture("one", now - 1000);
        first.session_file = Some(session.to_string_lossy().into_owned());
        store
            .insert(
                &first,
                &json!({"native":{"tool_calls":[
                    {"function":{"name":"bash","arguments":"{}"}},
                    {"function":{"name":"read","arguments":"{}"}}
                ]}}),
            )
            .unwrap();
        fn tool_call<'a>(
            session: &'a Path,
            call_id: &'a str,
            tool: &'a str,
            status: ToolStatus,
            error: Option<&'a str>,
            timestamp: i64,
            duration_ms: f64,
        ) -> ToolCall<'a> {
            ToolCall {
                request_key: Some("one"),
                session_file: session,
                call_id,
                tool,
                status,
                error_message: error,
                timestamp,
                duration_ms,
            }
        }
        store
            .tool(&tool_call(
                &session,
                "c1",
                "bash",
                ToolStatus::Ok,
                None,
                now - 900,
                120.0,
            ))
            .unwrap();
        store
            .tool(&tool_call(
                &session,
                "c2",
                "bash",
                ToolStatus::Error,
                Some("exit status 1"),
                now - 800,
                240.0,
            ))
            .unwrap();
        store
            .tool(&tool_call(
                &session,
                "c3",
                "read",
                ToolStatus::Error,
                Some("No such file: /tmp/x"),
                now - 700,
                10.0,
            ))
            .unwrap();
        store
            .tool(&tool_call(
                &session,
                "c4",
                "read",
                ToolStatus::Cancelled,
                Some("Tool cancelled"),
                now - 600,
                5.0,
            ))
            .unwrap();
        // 同一会话的同一个 call ID 只保留最后一次结果。
        store
            .tool(&tool_call(
                &session,
                "c2",
                "bash",
                ToolStatus::Error,
                Some("exit status 2"),
                now - 800,
                300.0,
            ))
            .unwrap();
        // 范围外与无法关联请求的执行不能污染范围内的计数。
        store
            .tool(&ToolCall {
                request_key: None,
                session_file: &session,
                call_id: "old",
                tool: "bash",
                status: ToolStatus::Error,
                error_message: Some("old failure"),
                timestamp: Range::Day.since(now) - 1,
                duration_ms: 1.0,
            })
            .unwrap();

        let errors = store.errors(Range::Day, now, 500).unwrap();
        assert_eq!(errors["tool_summary"]["failures"], 2);
        assert_eq!(errors["tool_summary"]["tools"], 2);
        assert_eq!(errors["tool_signatures"].as_array().unwrap().len(), 2);
        assert_eq!(errors["tool_failures"].as_array().unwrap().len(), 2);
        assert_eq!(errors["tool_failures"][0]["tool"], "read");
        assert_eq!(errors["tool_failures"][0]["model"], "fixture-model");
        assert_eq!(errors["tool_failures"][0]["project"], "project");
        assert!(errors["tool_failures"][0]["request_id"].as_i64().is_some());
        assert_eq!(errors["tool_has_more"], false);
        // 取消不是工具错误，结束时间戳覆盖范围内最新一次失败。
        assert!(!errors.to_string().contains("Tool cancelled"));
        assert_eq!(
            errors["tool_signatures"][0]["signature"],
            "read: No such file: /tmp/x"
        );
        let shifted = store.errors(Range::Day, now - 3_600_000, 500).unwrap();
        // 上界之后的主记录被排除；未关联请求的执行仍计入工具错误。
        assert_eq!(shifted["tool_summary"]["failures"], 1);
        assert_eq!(shifted["tool_failures"][0]["call_id"], "old");
        assert!(shifted["tool_failures"][0]["request_id"].is_null());

        let tools = store.tools(Range::Day, now).unwrap();
        assert_eq!(tools["summary"]["executions"], 4);
        assert_eq!(tools["summary"]["errors"], 2);
        assert_eq!(tools["summary"]["cancelled"], 1);
        assert_eq!(tools["summary"]["avg_duration_ms"], 108.75);
        let bash = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["tool"] == "bash")
            .unwrap();
        assert_eq!(bash["calls"], 1);
        assert_eq!(bash["executions"], 2);
        assert_eq!(bash["errors"], 1);
        assert_eq!(bash["avg_duration_ms"], 210.0);
        assert_eq!(bash["models"][0]["errors"], 1);
        assert_eq!(bash["models"][0]["avg_duration_ms"], 210.0);
        let read = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["tool"] == "read")
            .unwrap();
        assert_eq!(read["errors"], 1);
        assert_eq!(read["cancelled"], 1);
        assert_eq!(read["avg_duration_ms"], 7.5);
        assert!(!tools.to_string().contains("/tmp/x"));

        store.conn.execute_batch("DROP TABLE tool_calls;").unwrap();
        drop(store);
        let store = open_store(&path).unwrap();
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM tool_calls", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        // 重开后表与索引按幂等 schema 重建。
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name IN ('tool_calls_time','tool_calls_error_time')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        assert_eq!(store.latest(0, now + 1, 10).unwrap().len(), 1);
        let empty = store.errors(Range::All, now, 500).unwrap();
        assert_eq!(empty["tool_summary"]["failures"], 0);
        assert!(empty["tool_failures"].as_array().unwrap().is_empty());
    }
    #[test]
    fn stats_analytics_api_validates_ranges_and_omits_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        store
            .insert(
                &fixture("one", chrono::Utc::now().timestamp_millis()),
                &json!({"text":"private-output"}),
            )
            .unwrap();
        for endpoint in ["models", "providers", "costs", "projects", "tools"] {
            for range in ["1h", "24h", "7d", "30d", "90d", "all"] {
                let (code, _, body) =
                    server::dispatch(&store, &format!("/api/stats/{endpoint}?range={range}"), &[]);
                assert_eq!(code, 200);
                assert!(!body.contains("private-output"));
                if endpoint != "projects" {
                    assert!(!body.contains("session_file"));
                }
                let value: Value = serde_json::from_str(&body).unwrap();
                if endpoint == "tools" {
                    assert_eq!(value["summary"]["calls"], 0);
                    assert_eq!(value["summary"]["cost"], 0.0);
                } else {
                    assert_eq!(value["summary"]["requests"], 1);
                    assert_eq!(value["summary"]["unpriced"], 1);
                    assert!(value["summary"]["cost"].is_null());
                }
            }
            for query in [
                "range=bad",
                "range=24h&range=all",
                "limit=500",
                "unexpected=1",
            ] {
                assert_eq!(
                    server::dispatch(&store, &format!("/api/stats/{endpoint}?{query}"), &[]).0,
                    400
                );
            }
        }
    }
    #[test]
    fn stats_creation_reopen_dedup_and_snapshot_are_transactional() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats/stats.db");
        let mut store = open_store(&path).unwrap();
        let record = fixture("one", 1000);
        store.insert(&record, &json!({"text":"hello"})).unwrap();
        store.insert(&record, &json!({"text":"wrong"})).unwrap();
        store.link("one",Path::new("/tmp/session.jsonl"),&[json!({"id":"entry","type":"message","message":{"role":"assistant","content":"hello"}})]).unwrap();
        drop(store);
        let store = open_store(&path).unwrap();
        assert_eq!(store.latest(0, 2000, 500).unwrap().len(), 1);
        let detail = store.detail(1).unwrap().unwrap();
        assert_eq!(detail["output_message"]["text"], "hello");
        assert_eq!(detail["request"]["entry_id"], "entry");
        assert_eq!(detail["session_entry"][0]["id"], "entry");
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            store
                .conn
                .pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
    }
    #[test]
    fn stats_ranges_include_boundaries_and_preserve_unknown_usage() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        let now = 10_000_000;
        store
            .insert(&fixture("boundary", now - 3600000), &Value::Null)
            .unwrap();
        store
            .insert(&fixture("outside", now - 3600001), &Value::Null)
            .unwrap();
        let mut missing = fixture("missing", now);
        missing.input_tokens = None;
        missing.output_tokens = None;
        missing.cached_tokens = None;
        missing.cache_write_tokens = None;
        missing.ttft_ms = None;
        missing.status = "aborted".into();
        store.insert(&missing, &Value::Null).unwrap();
        store
            .insert(&fixture("future", now + 1), &Value::Null)
            .unwrap();
        let summary = store.overview(Range::Hour, now).unwrap();
        let s = &summary["summary"];
        assert_eq!(s["requests"], 2);
        assert_eq!(s["aborted"], 1);
        assert_eq!(s["input_tokens"], 100);
        assert_eq!(s["uncached_tokens"], 20);
        assert_eq!(s["output_tokens"], 20);
        assert_eq!(s["usage_known"], 1);
        assert_eq!(s["avg_ttft_ms"], 500.0);
        assert_eq!(
            store.overview(Range::All, now).unwrap()["summary"]["requests"],
            3
        );
        assert!(store.detail(999).unwrap().is_none());
        for label in ["1h", "24h", "7d", "30d", "90d", "all"] {
            assert!(Range::parse(label).is_ok());
        }
        assert!(Range::parse("1d").is_err());
    }
    #[test]
    fn stats_multiple_connections_read_during_write_and_bound_lock_wait() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        open_store(&path).unwrap();
        std::thread::scope(|scope| {
            for i in 0..4 {
                let path = &path;
                scope.spawn(move || {
                    let mut store = open_store(path).unwrap();
                    for j in 0..15 {
                        store
                            .insert(&fixture(&format!("{i}-{j}"), 1000), &Value::Null)
                            .unwrap();
                    }
                });
            }
        });
        let mut writer = open_store(&path).unwrap();
        let mut reader = open_store(&path).unwrap();
        assert_eq!(reader.latest(0, 2000, 500).unwrap().len(), 60);
        let tx = writer
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("UPDATE requests SET project='pending'", [])
            .unwrap();
        assert_eq!(reader.latest(0, 2000, 500).unwrap()[0].project, "project");
        let started = Instant::now();
        assert!(
            reader
                .insert(&fixture("locked", 1000), &Value::Null)
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(tx);
        reader
            .insert(&fixture("unlocked", 1000), &Value::Null)
            .unwrap();
    }
    #[test]
    fn stats_api_rejects_invalid_queries_and_keeps_payloads_out_of_lists() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        store
            .insert(
                &fixture("test", chrono::Utc::now().timestamp_millis()),
                &json!({"text":"private output"}),
            )
            .unwrap();
        for path in [
            "/api/stats/overview?range=bad",
            "/api/stats/overview?range=1h&range=7d",
            "/api/stats/requests?limit=9999999",
            "/api/stats/requests/-1",
            "/api/stats/requests/not-a-number",
            "/api/stats/requests?foo=bar",
        ] {
            assert_eq!(server::dispatch(&store, path, &[]).0, 400, "{path}");
        }
        for path in [
            "/api/stats/overview?range=all",
            "/api/stats/requests?range=all&limit=500",
        ] {
            let (status, _, body) = server::dispatch(&store, path, &[]);
            assert_eq!(status, 200);
            assert!(!body.contains("private output"));
        }
        assert_eq!(
            server::dispatch(&store, "/api/stats/requests/999", &[]).0,
            404
        );
        assert!(
            server::dispatch(&store, "/api/stats/requests/1", &[])
                .2
                .contains("private output")
        );
    }
    #[test]
    fn stats_symlinks_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        open_store(&path).unwrap();
        let link = dir.path().join("link.db");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(open_store(&link).is_err());
    }

    #[test]
    fn stats_schema_is_ensured_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        let mut store = open_store(&path).unwrap();
        let mut priced = fixture("priced", 1000);
        priced.cost = Some(CostSnapshot {
            input: 0.25,
            cache_read: 0.25,
            cache_write: 0.25,
            output: 0.25,
        });
        priced.rates = Some(RateSnapshot {
            input: 2.0,
            cached_input: 0.2,
            cache_write: None,
            output: 8.0,
        });
        store.insert(&priced, &Value::Null).unwrap();
        store
            .insert(&fixture("legacy", 1001), &Value::Null)
            .unwrap();
        // 采集时写入的快照可被列表与详情读取。
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM requests WHERE cost_json IS NOT NULL AND rate_json IS NOT NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        let detail = store.detail(1).unwrap().unwrap();
        assert_eq!(detail["request"]["cost"], 1.0);
        assert_eq!(detail["request"]["cost_components"]["cache_write"], 0.25);
        assert_eq!(
            store.latest(0, 1000, 1).unwrap()[0].rates().unwrap().input,
            2.0
        );
        let session = dir.path().join("session.jsonl");
        store
            .tool(&ToolCall {
                request_key: Some("priced"),
                session_file: &session,
                call_id: "call-1",
                tool: "bash",
                status: ToolStatus::Error,
                error_message: Some("boom"),
                timestamp: 1000,
                duration_ms: 5.0,
            })
            .unwrap();
        // 模拟旧库：新索引与费用快照列都不存在。
        store
            .conn
            .execute_batch(
                "DROP INDEX requests_session; DROP INDEX tool_calls_error_time;
                 ALTER TABLE requests DROP COLUMN cost_json; ALTER TABLE requests DROP COLUMN rate_json;",
            )
            .unwrap();
        drop(store);

        let store = open_store(&path).unwrap();
        let indexes = |table: &str| -> Vec<String> {
            let mut statement = store
                .conn
                .prepare(&format!("PRAGMA index_list({table})"))
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert!(indexes("requests").contains(&"requests_session".to_owned()));
        assert!(indexes("requests").contains(&"requests_time".to_owned()));
        assert!(
            indexes("tool_calls").contains(&"tool_calls_error_time".to_owned()),
            "重开必须重建部分索引"
        );
        assert!(indexes("tool_calls").contains(&"tool_calls_time".to_owned()));
        let columns = table_columns(&store.conn, "requests").unwrap();
        assert!(columns.iter().any(|name| name == "cost_json"));
        assert!(columns.iter().any(|name| name == "rate_json"));
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM requests", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
        // 历史行保留；补列后旧行的费用快照全为 NULL，按未定价统计。
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM requests", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM requests WHERE cost_json IS NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        let summary = store.analytics(Range::All, 2000).unwrap();
        assert_eq!(summary["summary"]["requests"], 2);
        assert_eq!(summary["summary"]["priced"], 0);
        assert_eq!(summary["summary"]["unpriced"], 2);
        assert!(summary["summary"]["cost"].is_null());
        // 补齐结构的那次 open 会给 planner 补统计。
        for table in ["requests", "tool_calls"] {
            assert!(
                store
                    .conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_stat1 WHERE tbl=?1",
                        [table],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap()
                    > 0,
                "{table} 缺少 ANALYZE 统计"
            );
        }
        // 版本号不再被读写。
        assert_eq!(
            store
                .conn
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        let structure = |store: &Store| -> Vec<String> {
            let mut statement = store
                .conn
                .prepare("SELECT type,name,sql FROM sqlite_master ORDER BY type,name")
                .unwrap();
            statement
                .query_map([], |row| {
                    Ok(format!(
                        "{}:{}:{}",
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?.unwrap_or_default()
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let before = structure(&store);
        drop(store);
        let store = open_store(&path).unwrap();
        // 结构齐全时再次 open 不改变 schema。
        assert_eq!(structure(&store), before);
    }

    /// 取 `EXPLAIN QUERY PLAN` 的计划文本（`detail` 列）。
    fn query_plan(conn: &Connection, sql: &str, parameters: impl rusqlite::Params) -> String {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        statement
            .query_map(parameters, |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n")
    }

    #[test]
    fn stats_query_plans_use_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        let now = 1_800_000_000_000;
        let root = "root.jsonl";
        let child = "root.subagents/sessions/child.jsonl";
        for n in 0..200 {
            let mut record = fixture(&format!("request-{n}"), now - n * 60_000);
            // 真实库中一个会话只占少数行；根会话与子 Agent 各保留几条，其余归入其它会话。
            record.session_file = Some(match n {
                0..=3 => root.to_owned(),
                4..=7 => child.to_owned(),
                _ => format!("other-{n}.jsonl"),
            });
            if n % 20 == 0 {
                record.status = "failed".into();
                record.error_message = Some("HTTP 500".into());
            }
            store.insert(&record, &Value::Null).unwrap();
        }
        for n in 0..100 {
            let call_id = format!("call-{n}");
            let session = match n {
                0..=3 => root.to_owned(),
                4..=7 => child.to_owned(),
                _ => format!("other-{n}.jsonl"),
            };
            store
                .tool(&ToolCall {
                    request_key: Some("request-0"),
                    session_file: Path::new(&session),
                    call_id: &call_id,
                    tool: "bash",
                    status: if n % 10 == 0 {
                        ToolStatus::Error
                    } else {
                        ToolStatus::Ok
                    },
                    error_message: None,
                    timestamp: now - n * 60_000,
                    duration_ms: 12.0,
                })
                .unwrap();
        }
        store
            .conn
            .execute_batch("ANALYZE requests; ANALYZE tool_calls;")
            .unwrap();
        let scope =
            SessionScope::new(root.to_owned(), "root.subagents/sessions/".to_owned()).unwrap();
        let since = Range::Day.since(now);

        let latest = query_plan(&store.conn, &latest_sql(), params![since, now, 12_i64]);
        assert!(latest.contains("requests_time"), "{latest}");
        assert!(!latest.contains("SCAN requests"), "{latest}");

        let session = query_plan(
            &store.conn,
            &session_requests_sql(COLUMNS),
            params![scope.root, scope.prefix, scope.upper],
        );
        assert!(session.contains("requests_session"), "{session}");
        assert!(!session.contains("SCAN requests"), "{session}");

        let failed = query_plan(
            &store.conn,
            &activity::failed_requests_sql(),
            params![since, now],
        );
        assert!(failed.contains("requests_failed_time"), "{failed}");
        assert!(!failed.contains("SCAN requests"), "{failed}");

        let tools = query_plan(
            &store.conn,
            &activity::tool_failures_sql(),
            params![since, now],
        );
        assert!(tools.contains("tool_calls_error_time"), "{tools}");
        assert!(!tools.contains("SCAN tool_calls"), "{tools}");

        let trace_tools = query_plan(
            &store.conn,
            &activity::trace_tools_sql(),
            params![scope.root, scope.prefix, scope.upper],
        );
        assert!(
            trace_tools.contains("sqlite_autoindex_tool_calls_1"),
            "{trace_tools}"
        );
        assert!(!trace_tools.contains("SCAN tool_calls"), "{trace_tools}");

        // `range=all` 从时间下界 0 开始；记录实际计划，由报告说明是否退化为全表读取。
        for (name, plan) in [
            (
                "latest",
                query_plan(&store.conn, &latest_sql(), params![0_i64, now, 12_i64]),
            ),
            (
                "failed",
                query_plan(
                    &store.conn,
                    &activity::failed_requests_sql(),
                    params![0_i64, now],
                ),
            ),
        ] {
            eprintln!("range=all {name} plan:\n{plan}");
        }
    }
}
