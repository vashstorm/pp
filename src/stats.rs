//! 与会话日志独立的请求统计；数据库故障不改变模型调用结果。
mod activity;
mod analytics;
mod insights;
pub(crate) mod server;

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, DirBuilder, OpenOptions},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub(crate) struct RequestContext {
    pub model_reference: String,
    pub cwd: PathBuf,
    pub purpose: &'static str,
    pub session_file: Option<PathBuf>,
}

pub(crate) struct Observation {
    pub timestamp: i64,
    pub ttft_ms: Option<u64>,
    pub text: String,
    pub thinking: String,
    pub output: Value,
    pub stop_reason: Option<String>,
    pub usage: Option<crate::context::TokenUsage>,
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
        }
    }
    pub fn event(&mut self, event: &crate::events::AgentEvent, started: Instant) {
        use crate::events::AgentEvent;
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
}

const COLUMNS: &str = "id,request_key,timestamp,model,provider,model_reference,api,project,cwd,purpose,session_file,entry_id,duration_ms,ttft_ms,input_tokens,output_tokens,cached_tokens,status,stop_reason,error_message,cache_write_tokens";
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
    })
}

pub(crate) fn database_path() -> Result<PathBuf> {
    let agent = crate::config::global_dir()?;
    Ok(agent
        .parent()
        .context("statistics directory unavailable")?
        .join("stats/stats.db"))
}

/// Returns (total requests before cleanup, removed), counted in the same transaction.
/// Delete request snapshots before their parents, then reclaim the freed pages.
pub(crate) fn gc(cutoff: i64) -> Result<(u64, u64)> {
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

pub(crate) struct Store {
    pub conn: Connection,
    models_path: PathBuf,
}
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_models(path, crate::config::global_dir()?.join("models.yml"))
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
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(version <= 2, "unsupported statistics database version");
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        if version < 2 {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
            ensure!(version <= 2, "unsupported statistics database version");
            if version == 0 {
                tx.execute_batch("CREATE TABLE IF NOT EXISTS requests (
                id INTEGER PRIMARY KEY, request_key TEXT NOT NULL UNIQUE, timestamp INTEGER NOT NULL,
                model TEXT NOT NULL, provider TEXT NOT NULL, model_reference TEXT NOT NULL, api TEXT NOT NULL,
                project TEXT NOT NULL, cwd TEXT NOT NULL, purpose TEXT NOT NULL,
                session_file TEXT, entry_id TEXT, duration_ms REAL NOT NULL, ttft_ms REAL,
                input_tokens INTEGER, output_tokens INTEGER, cached_tokens INTEGER,
                status TEXT NOT NULL CHECK(status IN ('ok','failed','aborted')), stop_reason TEXT, error_message TEXT
            );
            CREATE INDEX IF NOT EXISTS requests_time ON requests(timestamp DESC,id DESC);
            CREATE TABLE IF NOT EXISTS request_payloads (
                request_id INTEGER PRIMARY KEY REFERENCES requests(id), output_json TEXT NOT NULL,
                session_entry_json TEXT
            );
            PRAGMA user_version=1;")?;
            }
            if version < 2 {
                tx.execute_batch("ALTER TABLE requests ADD COLUMN cache_write_tokens INTEGER; PRAGMA user_version=2;")?;
            }
            tx.commit()?;
        }
        Ok(Self { conn, models_path })
    }
    pub fn insert(&mut self, record: &RequestRow, output: &Value) -> Result<()> {
        let tx = self.conn.transaction()?;
        let inserted = tx.execute("INSERT INTO requests (request_key,timestamp,model,provider,model_reference,api,project,cwd,purpose,session_file,duration_ms,ttft_ms,input_tokens,output_tokens,cached_tokens,status,stop_reason,error_message,cache_write_tokens) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19) ON CONFLICT(request_key) DO NOTHING",params![record.request_key,record.timestamp,record.model,record.provider,record.model_reference,record.api,record.project,record.cwd,record.purpose,record.session_file,record.duration_ms,record.ttft_ms,record.input_tokens,record.output_tokens,record.cached_tokens,record.status,record.stop_reason,record.error_message,record.cache_write_tokens])?;
        if inserted != 0 {
            tx.execute(
                "INSERT INTO request_payloads(request_id,output_json) VALUES (?1,?2)",
                params![tx.last_insert_rowid(), serde_json::to_string(output)?],
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
    pub fn latest(&self, since: i64, until: i64, limit: usize) -> Result<Vec<RequestRow>> {
        Ok(self.conn.prepare(&format!("SELECT {COLUMNS} FROM requests WHERE timestamp>=?1 AND timestamp<=?2 ORDER BY timestamp DESC,id DESC LIMIT ?3"))?
            .query_map(params![since,until,limit as i64], row)?.collect::<rusqlite::Result<_>>()?)
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
        let (output, entry): (String, Option<String>) = self.conn.query_row(
            "SELECT output_json,session_entry_json FROM request_payloads WHERE request_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(Some(
            json!({"request":analytics::Pricing::load(self).request(&record)?,"output_message":serde_json::from_str::<Value>(&output)?,"session_entry":entry.map(|s|serde_json::from_str::<Value>(&s)).transpose()?}),
        ))
    }
    pub fn overview(&self, range: Range, now: i64) -> Result<Value> {
        let pricing = analytics::Pricing::load(self);
        let mut value = self.analytics(range, now, &pricing)?;
        value["latest"] = json!(
            self.latest(range.since(now), now, 12)?
                .iter()
                .map(|r| pricing.request(r))
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
pub(crate) fn warn() {
    tracing::warn!(event = "statistics_unavailable");
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("warning: statistics unavailable; model tasks are unaffected");
    }
}
pub(crate) fn save(
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
            .is_some_and(|e| e.is::<crate::cancellation::Cancelled>())
        {
            "aborted"
        } else {
            "failed"
        };
        let record = RequestRow {
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
        };
        let output = json!({"native":observation.output,"text":observation.text,"thinking":observation.thinking,"partial":outcome.is_err()});
        Store::open(&database_path()?)?.insert(&record, &output)?;
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
pub(crate) fn link(key: &str, file: &Path, entries: &[Value]) {
    if (|| -> Result<()> { Store::open(&database_path()?)?.link(key, file, entries) })().is_err() {
        warn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        }
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
        for n in 0..2005 {
            store
                .insert(&fixture(&format!("priced-{n}"), now), &Value::Null)
                .unwrap();
        }
        let mut other = fixture("other", now - 86_400_000);
        other.provider = "other".into();
        other.model_reference = "other/primary".into();
        other.status = "failed".into();
        store.insert(&other, &Value::Null).unwrap();
        let mut unknown = fixture("unknown", now);
        unknown.cached_tokens = None;
        unknown.ttft_ms = None;
        unknown.status = "aborted".into();
        store.insert(&unknown, &Value::Null).unwrap();
        store
            .insert(&fixture("outside", now + 1), &Value::Null)
            .unwrap();
        let pricing = analytics::Pricing::load(&store);
        let mut unpriced_alias = fixture("alias", now);
        unpriced_alias.model_reference = "fixture/unpriced".into();
        assert!(pricing.cost(&unpriced_alias).is_none());
        let result = store.analytics(Range::Day, now, &pricing).unwrap();
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
    fn stats_cache_write_migrates_v1_without_inventing_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        let mut store = open_store(&path).unwrap();
        store
            .insert(&fixture("old", 1000), &json!({"text":"retained"}))
            .unwrap();
        store
            .conn
            .execute_batch(
                "ALTER TABLE requests DROP COLUMN cache_write_tokens; PRAGMA user_version=1;",
            )
            .unwrap();
        drop(store);
        let mut store = open_store(&path).unwrap();
        let old = store.detail(1).unwrap().unwrap();
        assert_eq!(old["request"]["cached_tokens"], 80);
        assert!(old["request"]["cache_write_tokens"].is_null());
        assert_eq!(old["output_message"]["text"], "retained");
        let mut new = fixture("new", 1001);
        new.cache_write_tokens = Some(10);
        store.insert(&new, &Value::Null).unwrap();
        drop(store);
        let store = open_store(&path).unwrap();
        assert_eq!(
            store.latest(0, 2000, 500).unwrap()[0].cache_write_tokens,
            Some(10)
        );
        assert_eq!(
            store
                .conn
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
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
        let pricing = analytics::Pricing::load(&store);
        let mut record = fixture("written", 1000);
        record.cache_write_tokens = Some(10);
        let cost = pricing.request(&record).unwrap();
        assert_eq!(cost["cost_components"]["cache_write"], 0.000025);
        assert!((cost["cost"].as_f64().unwrap() - 0.000221).abs() < 1e-10);
        store.insert(&record, &Value::Null).unwrap();
        record.request_key = "zero".into();
        record.cache_write_tokens = Some(0);
        store.insert(&record, &Value::Null).unwrap();
        record.request_key = "unknown".into();
        record.cache_write_tokens = None;
        store.insert(&record, &Value::Null).unwrap();
        assert!(pricing.cost(&record).is_none());
        for written in [-1, 21, i64::MAX] {
            record.cache_write_tokens = Some(written);
            assert!(pricing.cost(&record).is_none());
        }
        let result = store.analytics(Range::All, 2000, &pricing).unwrap();
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
        let pricing = analytics::Pricing::load(&store);
        record.cache_write_tokens = Some(10);
        assert!(pricing.cost(&record).is_none());
        record.cache_write_tokens = Some(0);
        assert!(pricing.cost(&record).is_some());
        fs::write(
            &path,
            models_with_cost(json!({"input":0,"cacheRead":0,"cacheWrite":0,"output":0})),
        )
        .unwrap();
        record.cache_write_tokens = Some(10);
        assert_eq!(
            analytics::Pricing::load(&store).request(&record).unwrap()["cost"],
            0.0
        );
        let empty = store.analytics(Range::Hour, 10_000_000, &pricing).unwrap();
        assert_eq!(empty["summary"]["cache_write_tokens"], 0);
    }

    #[test]
    fn stats_pricing_preserves_unknown_and_reloads_validated_rates() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_store(&dir.path().join("stats.db")).unwrap();
        let mut record = fixture("one", 1000);
        assert!(analytics::Pricing::load(&store).cost(&record).is_none());
        fs::write(
            dir.path().join("pricing.json"),
            r#"{"fixture/primary":{"input":0,"cached_input":0,"output":0}}"#,
        )
        .unwrap();
        assert!(analytics::Pricing::load(&store).cost(&record).is_none());
        let path = dir.path().join("models.yml");
        for invalid in [
            models_with_cost(json!({"input":-1,"cacheRead":0,"output":1})),
            models_with_cost(json!({"input":1,"output":1})),
            models_with_cost(json!({"input":1,"cacheRead":0,"output":1,"typo":"secret-marker"})),
            "invalid secret-marker".into(),
        ] {
            fs::write(&path, invalid).unwrap();
            let pricing = analytics::Pricing::load(&store);
            assert!(pricing.error.is_some());
            assert!(!pricing.error.unwrap().contains("secret-marker"));
            assert!(pricing.cost(&record).is_none());
            assert!(store.overview(Range::All, 2000).is_ok());
        }
        fs::write(
            &path,
            models_with_cost(json!({"input":2,"cacheRead":0.2,"output":8})),
        )
        .unwrap();
        let priced = store.overview(Range::All, 2000).unwrap();
        assert!(priced["pricing_error"].is_null());
        assert!(
            (analytics::Pricing::load(&store).request(&record).unwrap()["cost"]
                .as_f64()
                .unwrap()
                - 0.000216)
                .abs()
                < 1e-10
        );
        fs::write(
            &path,
            models_with_cost(json!({"input":0,"cacheRead":0,"output":0})),
        )
        .unwrap();
        let pricing = analytics::Pricing::load(&store);
        assert_eq!(pricing.request(&record).unwrap()["cost"], 0.0);
        record.cached_tokens = Some(101);
        assert!(pricing.cost(&record).is_none());
        record.cached_tokens = None;
        assert!(pricing.cost(&record).is_none());
        record.cached_tokens = Some(80);
        record.output_tokens = None;
        assert!(pricing.cost(&record).is_none());
        let empty = store.analytics(Range::All, 2000, &pricing).unwrap();
        assert_eq!(empty["summary"]["cost"], 0.0);
        assert_eq!(empty["summary"]["unpriced"], 0);
        assert_eq!(empty["summary"]["tokens"], 0);
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
        let pricing = analytics::Pricing::default();
        let errors = store.errors(Range::Day, now, 500, &pricing).unwrap();
        assert_eq!(errors["summary"]["requests"], 502);
        assert_eq!(errors["signatures"].as_array().unwrap().len(), 1);
        assert_eq!(errors["signatures"][0]["requests"], 502);
        assert_eq!(errors["requests"].as_array().unwrap().len(), 500);
        assert_eq!(errors["has_more"], true);
        assert!(!errors.to_string().contains("private-output"));
        let traces = store.traces(Range::Hour, now, 500, &pricing).unwrap();
        assert_eq!(traces["total"], 502);
        assert_eq!(traces["sessions"][0]["requests"], 1);
        let id = traces["sessions"][0]["id"].as_i64().unwrap();
        let trace = store.trace(id, now, &pricing).unwrap().unwrap();
        assert_eq!(trace["summary"]["requests"], 2);
        assert_eq!(trace["requests"][0]["id"], 1);
        assert_eq!(trace["summary"]["wall_ms"], 7_202_000.0);
        assert_eq!(trace["summary"]["model_ms"], 4000.0);
        assert!(!trace.to_string().contains("private-output"));
        assert_eq!(
            store.detail(1).unwrap().unwrap()["output_message"]["text"],
            "private-output"
        );
        assert!(store.trace(9999, now, &pricing).unwrap().is_none());
        assert_eq!(
            store.trace(2, now, &pricing).unwrap().unwrap()["summary"]["requests"],
            1
        );
        for endpoint in ["errors", "traces"] {
            for query in [
                "range=bad",
                "range=all&range=24h",
                "limit=1",
                "unexpected=1",
            ] {
                assert_eq!(
                    server::dispatch(&store, &format!("/api/stats/{endpoint}?{query}")).0,
                    400
                );
            }
            assert_eq!(
                server::dispatch(
                    &store,
                    &format!("/api/stats/{endpoint}?range=all&limit=2000")
                )
                .0,
                200
            );
        }
        assert_eq!(server::dispatch(&store, "/api/stats/traces/99999").0, 404);
        assert_eq!(server::dispatch(&store, "/api/stats/traces/0").0, 400);
        assert_eq!(
            server::dispatch(&store, "/api/stats/traces/1?file=/tmp/private").0,
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
        let completion = json!({"native":{"tool_calls":[
            {"function":{"name":"read","arguments":"private-arguments"}},
            {"function":{"name":"bash","arguments":"{}"}}
        ]},"text":"private-output"});
        let mut first = fixture("one", now - 1000);
        first.session_file = Some("/missing/session.jsonl".into());
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
            store.insert(&r, &responses).unwrap();
        }
        let mut failed = fixture("failed", now);
        failed.status = "aborted".into();
        store.insert(&failed, &json!({"native":{"output":[{"type":"function_call","name":"grep","arguments":"中😀"}]}})).unwrap();
        store
            .insert(&fixture("future", now + 1), &responses)
            .unwrap();
        store
            .insert(&fixture("old", now - 3_600_001), &responses)
            .unwrap();
        let pricing = analytics::Pricing::load(&store);
        let tools = store.tools(Range::Hour, now, &pricing).unwrap();
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
        let projects = store.projects(Range::Hour, now, &pricing).unwrap();
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
        let empty = store.tools(Range::Hour, now - 7_200_000, &pricing).unwrap();
        assert_eq!(empty["summary"]["cost"], 0.0);
        assert_eq!(empty["summary"]["tokens"], 0.0);
        assert!(empty["tools"].as_array().unwrap().is_empty());
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
                    server::dispatch(&store, &format!("/api/stats/{endpoint}?range={range}"));
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
                    server::dispatch(&store, &format!("/api/stats/{endpoint}?{query}")).0,
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
        assert_eq!(
            store
                .conn
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            2
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
            assert_eq!(server::dispatch(&store, path).0, 400, "{path}");
        }
        for path in [
            "/api/stats/overview?range=all",
            "/api/stats/requests?range=all&limit=500",
        ] {
            let (status, _, body) = server::dispatch(&store, path);
            assert_eq!(status, 200);
            assert!(!body.contains("private output"));
        }
        assert_eq!(server::dispatch(&store, "/api/stats/requests/999").0, 404);
        assert!(
            server::dispatch(&store, "/api/stats/requests/1")
                .2
                .contains("private output")
        );
    }
    #[test]
    fn stats_future_schema_and_symlinks_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        let store = open_store(&path).unwrap();
        store.conn.pragma_update(None, "user_version", 999).unwrap();
        drop(store);
        assert!(open_store(&path).is_err());
        let link = dir.path().join("link.db");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(open_store(&link).is_err());
    }
}
