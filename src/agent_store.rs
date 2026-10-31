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
const SCHEMA_VERSION: i64 = 3;

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
    model_reference TEXT NULL,
    fallback_reference TEXT NULL,
    fallback_effort TEXT NULL CHECK(fallback_effort IS NULL OR fallback_effort IN ('none','low','medium','high','xhigh','max')),
    effort TEXT NOT NULL CHECK(effort IN ('none','low','medium','high','xhigh','max')),
    CHECK((fallback_reference IS NULL) = (fallback_effort IS NULL)),
    CHECK(enabled=0 OR model_reference IS NOT NULL)
);
CREATE TABLE IF NOT EXISTS model_usage (
    model_reference TEXT PRIMARY KEY,
    request_count INTEGER NOT NULL DEFAULT 0 CHECK(request_count>=0)
);
CREATE TABLE IF NOT EXISTS migration_state (
    id INTEGER PRIMARY KEY CHECK(id=1),
    stage TEXT NOT NULL CHECK(stage IN ('imported','ready'))
);";

const AUTH_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS auth_credentials (
    provider TEXT PRIMARY KEY CHECK(provider IN ('github-copilot','openai-codex','kimi-code','glm','zai','command-code')),
    generation TEXT NOT NULL,
    access_token TEXT NOT NULL,
    refresh_token TEXT NULL,
    expires_at INTEGER NULL CHECK(expires_at>=0),
    account_id TEXT NULL,
    api_endpoint TEXT NULL,
    device_id TEXT NULL
);";

impl AgentStore {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let mut conn = crate::sqlite::open(path).map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        ensure!(version <= SCHEMA_VERSION, UNSUPPORTED_VERSION);
        if version < SCHEMA_VERSION {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
            // 在写锁内重读版本，避免与并发初始化竞争。
            let version: i64 = tx
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
            ensure!(version <= SCHEMA_VERSION, UNSUPPORTED_VERSION);
            if version == 0 {
                tx.execute_batch(SCHEMA)
                    .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
                tx.execute_batch(AUTH_SCHEMA)
                    .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
            } else if version == 1 {
                // SQLite 不能直接扩展 CHECK；在同一事务中重建授权表并完整保留凭证。
                tx.execute_batch("ALTER TABLE auth_credentials RENAME TO auth_credentials_v1;")
                    .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
                tx.execute_batch(AUTH_SCHEMA)
                    .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
                tx.execute_batch(
                    "INSERT INTO auth_credentials(provider, generation, access_token, refresh_token,
                                                  expires_at, account_id, api_endpoint, device_id)
                     SELECT provider, generation, access_token, refresh_token,
                            expires_at, account_id, api_endpoint, device_id FROM auth_credentials_v1;
                     DROP TABLE auth_credentials_v1;",
                )
                .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
            }
            if version == 1 || version == 2 {
                tx.execute_batch("ALTER TABLE model_roles RENAME TO model_roles_v2;")
                    .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
                tx.execute_batch(SCHEMA)
                    .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
                tx.execute_batch(
                    "INSERT INTO model_roles(role,enabled,model_reference,effort)
                    SELECT role,enabled,model_reference,effort FROM model_roles_v2;
                    DROP TABLE model_roles_v2;",
                )
                .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
            }
            if version < SCHEMA_VERSION {
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)
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
            let duplicate: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM model_roles WHERE role='default' AND fallback_reference=?1)", params![reference], |row| row.get(0)).map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
            ensure!(!duplicate, "role primary and fallback must differ");
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
                assignment.validate()?;
                tx.execute(
                    "INSERT INTO model_roles(role, enabled, model_reference, effort, fallback_reference, fallback_effort)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(role) DO UPDATE SET
                        enabled=excluded.enabled,
                        model_reference=excluded.model_reference,
                        effort=excluded.effort,
                        fallback_reference=excluded.fallback_reference,
                        fallback_effort=excluded.fallback_effort",
                    params![
                        role_name(role),
                        assignment.enabled as i64,
                        (!assignment.model.is_empty()).then_some(&assignment.model),
                        assignment.effort.as_str(),
                        assignment.fallback.as_ref().map(|f| &f.model),
                        assignment.fallback.as_ref().map(|f| f.effort.as_str()),
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
        .prepare("SELECT role, enabled, model_reference, effort, fallback_reference, fallback_effort FROM model_roles")
        .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
    let mut roles = ModelRoles::new();
    for row in rows {
        let (name, enabled, model, effort, fallback, fallback_effort) =
            row.map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        let role = parse_role(&name).ok_or_else(|| anyhow::anyhow!(UNAVAILABLE))?;
        let effort: ThinkLevel = effort.parse().map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        roles.insert(
            role,
            RoleAssignment {
                fallback: match (fallback, fallback_effort) {
                    (Some(model), Some(effort)) => Some(crate::config::RoleModel {
                        model,
                        effort: effort.parse().map_err(|_| anyhow::anyhow!(UNAVAILABLE))?,
                    }),
                    (None, None) => None,
                    _ => return Err(anyhow::anyhow!(UNAVAILABLE)),
                },
                enabled: enabled != 0,
                model: model.unwrap_or_default(),
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
                    fallback: None,
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
                    fallback: None,
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
    fn sqlite_auth_commandcode_upgrade_preserves_credentials_preferences_and_usage() {
        use crate::auth::{Credential, Provider, Store};

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("agent.db");
        let conn = crate::sqlite::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(&AUTH_SCHEMA.replace(",'command-code'", ""))
            .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute("INSERT INTO migration_state VALUES (1, 'ready')", [])
            .unwrap();
        let mut old = AgentStore { conn };
        old.set_default_model("fixture/main", ModelThinking::default())
            .unwrap();
        old.toggle_favorite("fixture/favorite").unwrap();
        old.set_model_role(
            ModelRole::Fast,
            Some(&RoleAssignment {
                fallback: None,
                enabled: true,
                model: "fixture/fast".into(),
                effort: ThinkLevel::Low,
            }),
        )
        .unwrap();
        old.increment_model_usage("fixture/main").unwrap();
        let prefs = old.preferences().unwrap();
        let mut auth = Store {
            credentials: BTreeMap::from([
                (Provider::Glm, Credential::api_key("glm-fixture".into())),
                (
                    Provider::GithubCopilot,
                    Credential {
                        generation: "existing-generation".into(),
                        access_token: "copilot-fixture".into(),
                        refresh_token: Some("refresh-fixture".into()),
                        expires_at: Some(123456),
                        account_id: Some("account-fixture".into()),
                        api_endpoint: Some("https://api.githubcopilot.com".into()),
                        device_id: Some("device-fixture".into()),
                    },
                ),
            ]),
        };
        old.save_auth(&auth).unwrap();
        drop(old);

        let mut upgraded = AgentStore::open(&path).unwrap();
        assert!(upgraded.load_auth().unwrap() == auth);
        assert_eq!(upgraded.preferences().unwrap(), prefs);
        assert_eq!(
            upgraded.model_usage().unwrap().get("fixture/main"),
            Some(&1)
        );
        let version: i64 = upgraded
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let stage: String = upgraded
            .conn
            .query_row("SELECT stage FROM migration_state WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(stage, "ready");
        auth.credentials.insert(
            Provider::CommandCode,
            Credential::api_key("user_fixture".into()),
        );
        upgraded.save_auth(&auth).unwrap();
        drop(upgraded);
        assert!(AgentStore::open(&path).unwrap().load_auth().unwrap() == auth);
    }

    #[test]
    fn sqlite_rejects_future_schema() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("agent.db");
        drop(AgentStore::open(&path).unwrap());
        {
            let raw = Connection::open(&path).unwrap();
            raw.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        let error = AgentStore::open(&path).err().unwrap();
        assert_eq!(error.to_string(), UNSUPPORTED_VERSION);
    }
    #[test]
    fn fallback_sqlite_v2_migration_preserves_roles_preferences_and_usage() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("agent.db");
        {
            let conn = crate::sqlite::open(&path).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute_batch(AUTH_SCHEMA).unwrap();
            conn.execute_batch("DROP TABLE model_roles;
                CREATE TABLE model_roles(role TEXT PRIMARY KEY, enabled INTEGER NOT NULL, model_reference TEXT NOT NULL, effort TEXT NOT NULL);
                INSERT INTO model_roles VALUES ('fast',1,'fixture/one','low');
                UPDATE preferences SET default_model='fixture/one',think_level='high';
                INSERT INTO model_usage VALUES ('fixture/one',7);
                INSERT INTO favorite_models VALUES ('fixture/one',0);
                INSERT INTO auth_credentials(provider,generation,access_token) VALUES ('glm','test-gen','test-token');").unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }
        let mut store = AgentStore::open(&path).unwrap();
        let mut value = store.preferences().unwrap().model_roles[&ModelRole::Fast].clone();
        assert_eq!(value.model, "fixture/one");
        assert!(value.fallback.is_none());
        value.enabled = false;
        value.model.clear();
        value.fallback = Some(crate::config::RoleModel {
            model: "fixture/two".into(),
            effort: ThinkLevel::High,
        });
        store.set_model_role(ModelRole::Fast, Some(&value)).unwrap();
        drop(store);
        let store = AgentStore::open(&path).unwrap();
        assert_eq!(
            store.preferences().unwrap().model_roles[&ModelRole::Fast],
            value
        );
        assert_eq!(
            store.preferences().unwrap().favorite_models,
            ["fixture/one"]
        );
        assert_eq!(store.model_usage().unwrap()["fixture/one"], 7);
        assert_eq!(
            store.load_auth().unwrap().credentials[&crate::auth::Provider::Glm].access_token,
            "test-token"
        );
    }
}
