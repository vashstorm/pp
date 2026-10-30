//! agent.db（`~/.pp/agent/agent.db`）的偏好与授权存储。
//!
//! 安全打开、WAL/外键/FULL/250ms 由 `crate::sqlite::open` 统一负责；本模块只拥有
//! agent.db 的固定 schema 与领域操作。所有存储/授权错误都转换为不含 SQL 参数、
//! 令牌与正文的固定领域提示；不实现通用 key/value 接口或跨库事务。

use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{collections::BTreeMap, path::Path};

use crate::config::{ModelPreferences, ModelRole, ModelRoles, RoleAssignment, ThinkLevel};
use crate::models::ModelThinking;

mod migration;
pub(crate) use migration::initialize;

const UNAVAILABLE: &str = "Agent database unavailable.";
const UNSUPPORTED_VERSION: &str = "Agent database schema version is not supported.";

static USAGE_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 逻辑请求使用计数失败的固定脱敏警告；与 `stats::warn` 相同的 once 模式。
pub(crate) fn warn_usage_unavailable() {
    tracing::warn!(event = "model_usage_unavailable");
    if !USAGE_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!("warning: model usage accounting unavailable; model tasks are unaffected");
    }
}

pub(crate) struct AgentStore {
    // 仅供本模块及其子模块（迁移）使用；连接由本结构独占，不跨线程共享。
    conn: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS preferences (
    id INTEGER PRIMARY KEY CHECK(id=1),
    default_model TEXT NULL,
    think_level TEXT NOT NULL CHECK(think_level IN ('none','low','medium','high','xhigh','max'))
);
INSERT OR IGNORE INTO preferences(id, default_model, think_level) VALUES (1, NULL, 'none');
CREATE TABLE IF NOT EXISTS favorite_models (
    model_reference TEXT PRIMARY KEY,
    position INTEGER NOT NULL UNIQUE CHECK(position>=0)
);
CREATE TABLE IF NOT EXISTS model_roles (
    role TEXT PRIMARY KEY CHECK(role IN ('default','fast','slow','vision','plan','review')),
    enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
    model_reference TEXT NOT NULL,
    effort TEXT NOT NULL CHECK(effort IN ('none','low','medium','high','xhigh','max'))
);
CREATE TABLE IF NOT EXISTS model_usage (
    model_reference TEXT PRIMARY KEY,
    request_count INTEGER NOT NULL DEFAULT 0 CHECK(request_count>=0)
);
CREATE TABLE IF NOT EXISTS auth_credentials (
    provider TEXT PRIMARY KEY CHECK(provider IN ('github-copilot','openai-codex','kimi-code','glm','zai')),
    generation TEXT NOT NULL,
    access_token TEXT NOT NULL,
    refresh_token TEXT NULL,
    expires_at INTEGER NULL CHECK(expires_at>=0),
    account_id TEXT NULL,
    api_endpoint TEXT NULL,
    device_id TEXT NULL
);
CREATE TABLE IF NOT EXISTS migration_state (
    id INTEGER PRIMARY KEY CHECK(id=1),
    stage TEXT NOT NULL CHECK(stage IN ('imported','ready'))
);";

impl AgentStore {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let mut conn = crate::sqlite::open(path).map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        ensure!(version <= 1, UNSUPPORTED_VERSION);
        if version < 1 {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
            // 在写锁内重读版本，避免与并发初始化竞争。
            let version: i64 = tx
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
            ensure!(version <= 1, UNSUPPORTED_VERSION);
            if version == 0 {
                tx.execute_batch(SCHEMA)
                    .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
                tx.pragma_update(None, "user_version", 1)
                    .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
            }
            tx.commit().map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        }
        Ok(Self { conn })
    }

    pub(crate) fn preferences(&self) -> Result<ModelPreferences> {
        read_preferences(&self.conn)
    }

    /// 读取持久化授权；逐 provider 调用 `Credential::validate`。
    pub(crate) fn load_auth(&self) -> Result<crate::auth::Store> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT provider, generation, access_token, refresh_token, expires_at,
                        account_id, api_endpoint, device_id FROM auth_credentials",
            )
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            })
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        let mut credentials = BTreeMap::new();
        for row in rows {
            let (
                provider,
                generation,
                access_token,
                refresh_token,
                expires_at,
                account_id,
                api_endpoint,
                device_id,
            ) = row.map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
            let provider = crate::auth::Provider::ALL
                .into_iter()
                .find(|candidate| candidate.id() == provider)
                .ok_or_else(|| anyhow::anyhow!(UNAVAILABLE))?;
            let credential = crate::auth::Credential {
                generation,
                access_token,
                refresh_token,
                expires_at: expires_at.map(|value| value as u64),
                account_id,
                api_endpoint,
                device_id,
            };
            credential.validate(provider)?;
            credentials.insert(provider, credential);
        }
        Ok(crate::auth::Store { credentials })
    }

    /// 在一笔事务内整体替换授权表，不触碰偏好或 model_usage。
    pub(crate) fn save_auth(&mut self, store: &crate::auth::Store) -> Result<()> {
        for (provider, credential) in &store.credentials {
            credential.validate(*provider)?;
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        tx.execute("DELETE FROM auth_credentials", [])
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        for (provider, credential) in &store.credentials {
            tx.execute(
                "INSERT INTO auth_credentials(provider, generation, access_token, refresh_token,
                                              expires_at, account_id, api_endpoint, device_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    provider.id(),
                    credential.generation,
                    credential.access_token,
                    credential.refresh_token,
                    credential.expires_at.map(|value| value as i64),
                    credential.account_id,
                    credential.api_endpoint,
                    credential.device_id,
                ],
            )
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        }
        tx.commit().map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        Ok(())
    }

    /// 保存默认模型，并用传入模型能力在事务内同步已启用 Default Role 的 effort。
    pub(crate) fn set_default_model(
        &mut self,
        reference: &str,
        thinking: ModelThinking,
    ) -> Result<ModelPreferences> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        let role: Option<(String, String)> = tx
            .query_row(
                "SELECT r.effort,p.think_level FROM model_roles r CROSS JOIN preferences p
             WHERE r.role='default' AND r.enabled=1 AND p.id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        let default_role = role
            .map(|(effort, global)| -> Result<ThinkLevel> {
                let effort = effort.parse().map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
                let global = global.parse().map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
                Ok(if thinking.supports(effort) {
                    effort
                } else {
                    thinking.effective_default(global)
                })
            })
            .transpose()?;
        tx.execute(
            "UPDATE preferences SET default_model=?1 WHERE id=1",
            params![reference],
        )
        .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        if let Some(effort) = default_role {
            tx.execute(
                "UPDATE model_roles SET model_reference=?1, effort=?2 WHERE role='default'",
                params![reference, effort.as_str()],
            )
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        }
        let updated = read_preferences(&tx)?;
        tx.commit().map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        Ok(updated)
    }

    /// 保存全局 effort；事务内读到的已启用 Default Role 模型经 `validate` 回调校验并同步 effort。
    pub(crate) fn set_default_effort(
        &mut self,
        level: ThinkLevel,
        validate: impl FnOnce(&str, ThinkLevel) -> Result<()>,
    ) -> Result<ModelPreferences> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        let reference: Option<String> = tx
            .query_row(
                "SELECT model_reference FROM model_roles WHERE role='default' AND enabled=1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        if let Some(reference) = &reference {
            validate(reference, level)?;
        }
        tx.execute(
            "UPDATE preferences SET think_level=?1 WHERE id=1",
            params![level.as_str()],
        )
        .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        if reference.is_some() {
            tx.execute(
                "UPDATE model_roles SET effort=?1 WHERE role=?2",
                params![level.as_str(), role_name(ModelRole::Default)],
            )
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        }
        let updated = read_preferences(&tx)?;
        tx.commit().map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        Ok(updated)
    }

    /// 保存或移除单个 Role 行；assignment 与收藏 reference 的可用性由调用方校验。
    pub(crate) fn set_model_role(
        &mut self,
        role: ModelRole,
        assignment: Option<&RoleAssignment>,
    ) -> Result<ModelPreferences> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        match assignment {
            Some(assignment) => {
                tx.execute(
                    "INSERT INTO model_roles(role, enabled, model_reference, effort)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(role) DO UPDATE SET
                        enabled=excluded.enabled,
                        model_reference=excluded.model_reference,
                        effort=excluded.effort",
                    params![
                        role_name(role),
                        assignment.enabled as i64,
                        assignment.model,
                        assignment.effort.as_str(),
                    ],
                )
                .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
            }
            None => {
                tx.execute(
                    "DELETE FROM model_roles WHERE role=?1",
                    params![role_name(role)],
                )
                .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
            }
        }
        let updated = read_preferences(&tx)?;
        tx.commit().map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        Ok(updated)
    }

    /// 切换收藏并保持原顺序；仅在事务内重读当前收藏后更新 favorite_models。
    pub(crate) fn toggle_favorite(&mut self, reference: &str) -> Result<ModelPreferences> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        let removed = tx
            .execute(
                "DELETE FROM favorite_models WHERE model_reference=?1",
                params![reference],
            )
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        if removed == 0 {
            tx.execute(
                "INSERT INTO favorite_models(model_reference,position)
                 VALUES (?1,(SELECT COALESCE(MAX(position),-1)+1 FROM favorite_models))",
                params![reference],
            )
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        }
        let updated = read_preferences(&tx)?;
        tx.commit().map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        Ok(updated)
    }

    /// 原子 +1 一次逻辑模型请求；不依赖 stats 写入结果。
    pub(crate) fn increment_model_usage(&mut self, reference: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        tx.execute(
            "INSERT INTO model_usage(model_reference, request_count) VALUES (?1, 1)
             ON CONFLICT(model_reference) DO UPDATE SET request_count=request_count+1",
            params![reference],
        )
        .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        tx.commit().map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        Ok(())
    }

    pub(crate) fn model_usage(&self) -> Result<BTreeMap<String, u64>> {
        let mut statement = self
            .conn
            .prepare("SELECT model_reference, request_count FROM model_usage")
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        let mut usage = BTreeMap::new();
        for row in rows {
            let (reference, count) = row.map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
            usage.insert(reference, count);
        }
        Ok(usage)
    }
}

fn role_name(role: ModelRole) -> &'static str {
    match role {
        ModelRole::Default => "default",
        ModelRole::Fast => "fast",
        ModelRole::Slow => "slow",
        ModelRole::Vision => "vision",
        ModelRole::Plan => "plan",
        ModelRole::Review => "review",
    }
}

fn parse_role(name: &str) -> Option<ModelRole> {
    ModelRole::ALL
        .into_iter()
        .find(|&role| role_name(role) == name)
}

fn read_preferences(conn: &Connection) -> Result<ModelPreferences> {
    let (default_model, think_level): (Option<String>, String) = conn
        .query_row(
            "SELECT default_model, think_level FROM preferences WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
    let think_level: ThinkLevel = think_level
        .parse()
        .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
    Ok(ModelPreferences {
        default_model,
        favorite_models: read_favorites(conn)?,
        model_roles: read_roles(conn)?,
        think_level,
    })
}

fn read_favorites(conn: &Connection) -> Result<Vec<String>> {
    let mut statement = conn
        .prepare("SELECT model_reference FROM favorite_models ORDER BY position")
        .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
    let mut favorites = Vec::new();
    for row in rows {
        favorites.push(row.map_err(|_| anyhow::anyhow!(UNAVAILABLE))?);
    }
    Ok(favorites)
}

fn read_roles(conn: &Connection) -> Result<ModelRoles> {
    let mut statement = conn
        .prepare("SELECT role, enabled, model_reference, effort FROM model_roles")
        .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
    let mut roles = ModelRoles::new();
    for row in rows {
        let (name, enabled, model, effort) = row.map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        let role = parse_role(&name).ok_or_else(|| anyhow::anyhow!(UNAVAILABLE))?;
        let effort: ThinkLevel = effort.parse().map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        roles.insert(
            role,
            RoleAssignment {
                enabled: enabled != 0,
                model,
                effort,
            },
        );
    }
    Ok(roles)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    fn temp_store(name: &str) -> (tempfile::TempDir, AgentStore) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(name);
        let store = AgentStore::open(&path).unwrap();
        (root, store)
    }

    #[test]
    fn sqlite_default_preferences_after_open() {
        let (_root, store) = temp_store("agent.db");
        let prefs = store.preferences().unwrap();
        assert_eq!(prefs, ModelPreferences::default());
        assert!(store.model_usage().unwrap().is_empty());
        assert!(store.load_auth().unwrap().credentials.is_empty());
    }

    #[test]
    fn sqlite_concurrent_preferences_independent_connections() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("agent.db");
        let mut first = AgentStore::open(&path).unwrap();
        let mut second = AgentStore::open(&path).unwrap();
        // 独立连接交错更新不同行：后提交者不得用陈旧快照覆盖先提交者。
        second.toggle_favorite("fixture/alpha").unwrap();
        first
            .set_default_model("fixture/beta", ModelThinking::default())
            .unwrap();
        second
            .set_model_role(
                ModelRole::Fast,
                Some(&RoleAssignment {
                    enabled: true,
                    model: "fixture/gamma".into(),
                    effort: ThinkLevel::Low,
                }),
            )
            .unwrap();
        first.increment_model_usage("fixture/beta").unwrap();
        second.increment_model_usage("fixture/beta").unwrap();
        drop(first);
        drop(second);
        let merged = AgentStore::open(&path).unwrap();
        let prefs = merged.preferences().unwrap();
        assert_eq!(prefs.default_model.as_deref(), Some("fixture/beta"));
        assert_eq!(prefs.favorite_models, ["fixture/alpha"]);
        assert_eq!(
            prefs.model_roles.get(&ModelRole::Fast).unwrap().model,
            "fixture/gamma"
        );
        assert_eq!(merged.model_usage().unwrap().get("fixture/beta"), Some(&2));
    }

    #[test]
    fn sqlite_effort_synchronization() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("agent.db");
        let mut store = AgentStore::open(&path).unwrap();
        store
            .set_model_role(
                ModelRole::Default,
                Some(&RoleAssignment {
                    enabled: true,
                    model: "fixture/one".into(),
                    effort: ThinkLevel::High,
                }),
            )
            .unwrap();

        // set_default_model 用传入模型能力修正事务内 Default Role 的 effort。
        let thinking = ModelThinking::without_reasoning();
        let prefs = store.set_default_model("fixture/two", thinking).unwrap();
        let assignment = prefs.model_roles.get(&ModelRole::Default).unwrap();
        assert_eq!(
            assignment.effort,
            thinking.effective_default(prefs.think_level)
        );
        assert_eq!(assignment.effort, ThinkLevel::None);
        assert_eq!(prefs.default_model.as_deref(), Some("fixture/two"));

        // set_default_effort 以事务内读到的 Default Role 模型回调校验，并同步 effort 行。
        let mut reopened = AgentStore::open(&path).unwrap();
        let mut validated = Vec::new();
        let prefs = reopened
            .set_default_effort(ThinkLevel::Medium, |reference, level| {
                validated.push((reference.to_owned(), level));
                Ok(())
            })
            .unwrap();
        assert_eq!(validated, [("fixture/two".to_owned(), ThinkLevel::Medium)]);
        assert_eq!(prefs.think_level, ThinkLevel::Medium);
        assert_eq!(
            prefs.model_roles.get(&ModelRole::Default).unwrap().effort,
            ThinkLevel::Medium
        );

        // 回调失败时整个写入不生效。
        assert!(
            reopened
                .set_default_effort(ThinkLevel::High, |_, _| bail!(
                    "model does not support this effort"
                ))
                .is_err()
        );
        let prefs = reopened.preferences().unwrap();
        assert_eq!(prefs.think_level, ThinkLevel::Medium);
        drop(reopened);
        let persisted = AgentStore::open(&path).unwrap().preferences().unwrap();
        assert_eq!(persisted.think_level, ThinkLevel::Medium);
        assert_eq!(
            persisted
                .model_roles
                .get(&ModelRole::Default)
                .unwrap()
                .effort,
            ThinkLevel::Medium
        );
    }

    #[test]
    fn sqlite_auth_save_isolated() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("agent.db");
        let mut store = AgentStore::open(&path).unwrap();
        store
            .set_default_model("fixture/one", ModelThinking::default())
            .unwrap();
        store.toggle_favorite("fixture/one").unwrap();
        store.increment_model_usage("fixture/one").unwrap();

        let mut glm = crate::auth::Credential::api_key("glm-token".into());
        glm.generation = "gen-glm".into();
        let mut auth = crate::auth::Store {
            credentials: BTreeMap::new(),
        };
        auth.credentials.insert(crate::auth::Provider::Glm, glm);
        auth.credentials.insert(
            crate::auth::Provider::OpenaiCodex,
            crate::auth::Credential {
                generation: "gen-codex".into(),
                access_token: "codex-token".into(),
                refresh_token: Some("refresh-token".into()),
                expires_at: Some(123_456),
                account_id: Some("account".into()),
                api_endpoint: None,
                device_id: Some("device".into()),
            },
        );
        store.save_auth(&auth).unwrap();

        // save_auth 不触碰偏好或 model_usage。
        let prefs = store.preferences().unwrap();
        assert_eq!(prefs.default_model.as_deref(), Some("fixture/one"));
        assert_eq!(prefs.favorite_models, ["fixture/one"]);
        assert_eq!(store.model_usage().unwrap().get("fixture/one"), Some(&1));

        // 往返保留 generation 与全部字段。
        let loaded = store.load_auth().unwrap();
        assert_eq!(loaded.credentials.len(), 2);
        let glm = loaded.credentials.get(&crate::auth::Provider::Glm).unwrap();
        assert_eq!(glm.access_token, "glm-token");
        assert_eq!(glm.generation, "gen-glm");
        let codex = loaded
            .credentials
            .get(&crate::auth::Provider::OpenaiCodex)
            .unwrap();
        assert_eq!(codex.access_token, "codex-token");
        assert_eq!(codex.generation, "gen-codex");
        assert_eq!(codex.refresh_token.as_deref(), Some("refresh-token"));
        assert_eq!(codex.expires_at, Some(123_456));
        assert_eq!(codex.account_id.as_deref(), Some("account"));
        assert_eq!(codex.device_id.as_deref(), Some("device"));

        // 空 Store（logout）清空授权且不影响偏好。
        store
            .save_auth(&crate::auth::Store {
                credentials: BTreeMap::new(),
            })
            .unwrap();
        assert!(store.load_auth().unwrap().credentials.is_empty());
        assert_eq!(
            store.preferences().unwrap().default_model.as_deref(),
            Some("fixture/one")
        );
    }

    #[test]
    fn sqlite_rejects_future_schema() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("agent.db");
        drop(AgentStore::open(&path).unwrap());
        {
            let raw = Connection::open(&path).unwrap();
            raw.pragma_update(None, "user_version", 2).unwrap();
        }
        let error = AgentStore::open(&path).err().unwrap();
        assert_eq!(error.to_string(), UNSUPPORTED_VERSION);
    }
}
