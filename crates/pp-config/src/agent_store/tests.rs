use super::*;
use anyhow::bail;

#[test]
fn sqlite_corrupt_databases_are_preserved_without_sensitive_errors() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("agent.db");
    let bytes = b"not-sqlite fixture-private-credential";
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let error = AgentStore::open(&path).err().unwrap();
    assert!(!format!("{error:#}").contains("fixture-private-credential"));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

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
    let conn = pp_core::sqlite::open(&path).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn.execute_batch(
        &AUTH_SCHEMA
            .replace(",'command-code'", "")
            .replace(",'opencode-zen','opencode-go'", ""),
    )
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
        let conn = pp_core::sqlite::open(&path).unwrap();
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

#[test]
fn sqlite_v3_role_table_is_rebuilt_for_explorer_and_keeps_existing_rows() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("agent.db");
    {
        let conn = pp_core::sqlite::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(AUTH_SCHEMA).unwrap();
        conn.execute_batch(
            "DROP TABLE model_roles;
            CREATE TABLE model_roles (
                role TEXT PRIMARY KEY CHECK(role IN ('default','fast','slow','vision','plan','review')),
                enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
                model_reference TEXT NULL,
                fallback_reference TEXT NULL,
                fallback_effort TEXT NULL,
                effort TEXT NOT NULL,
                CHECK((fallback_reference IS NULL) = (fallback_effort IS NULL)),
                CHECK(enabled=0 OR model_reference IS NOT NULL)
            );
            INSERT INTO model_roles(role,enabled,model_reference,fallback_reference,fallback_effort,effort)
                VALUES ('fast',1,'fixture/fast','fixture/backup','low','high');
            INSERT INTO model_roles(role,enabled,model_reference,fallback_reference,fallback_effort,effort)
                VALUES ('review',1,'fixture/review',NULL,NULL,'medium');
            INSERT INTO model_roles(role,enabled,model_reference,fallback_reference,fallback_effort,effort)
                VALUES ('plan',0,NULL,NULL,NULL,'none');
            INSERT INTO model_usage VALUES ('fixture/fast',4);
            INSERT INTO auth_credentials(provider,generation,access_token) VALUES ('zai','gen','token');",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
    }
    let mut store = AgentStore::open(&path).unwrap();
    let prefs = store.preferences().unwrap();
    let fast = &prefs.model_roles[&ModelRole::Fast];
    assert_eq!(fast.model, "fixture/fast");
    assert_eq!(fast.effort, ThinkLevel::High);
    assert_eq!(
        fast.fallback,
        Some(crate::config::RoleModel {
            model: "fixture/backup".into(),
            effort: ThinkLevel::Low,
        })
    );
    assert_eq!(
        prefs.model_roles[&ModelRole::Review].effort,
        ThinkLevel::Medium
    );
    assert!(!prefs.model_roles[&ModelRole::Plan].enabled);
    assert!(!prefs.model_roles.contains_key(&ModelRole::Explorer));
    let explorer = RoleAssignment {
        fallback: None,
        enabled: true,
        model: "fixture/explore".into(),
        effort: ThinkLevel::Low,
    };
    store
        .set_model_role(ModelRole::Explorer, Some(&explorer))
        .unwrap();
    drop(store);
    let store = AgentStore::open(&path).unwrap();
    assert_eq!(
        store.preferences().unwrap().model_roles[&ModelRole::Explorer],
        explorer
    );
    assert_eq!(store.model_usage().unwrap()["fixture/fast"], 4);
    assert_eq!(
        store.load_auth().unwrap().credentials[&crate::auth::Provider::Zai].access_token,
        "token"
    );
}

#[test]
fn sqlite_v4_to_v5_migration_accepts_opencode_and_preserves_existing_credentials() {
    use crate::auth::{Credential, Provider, Store};

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("agent.db");
    {
        let conn = pp_core::sqlite::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(&AUTH_SCHEMA.replace(",'opencode-zen','opencode-go'", ""))
            .unwrap();
        conn.pragma_update(None, "user_version", 4).unwrap();
        conn.execute("INSERT INTO migration_state VALUES (1, 'ready')", [])
            .unwrap();
        let mut old = AgentStore { conn };
        old.save_auth(&Store {
            credentials: BTreeMap::from([(
                Provider::CommandCode,
                Credential::api_key("user_fixture".into()),
            )]),
        })
        .unwrap();
    }

    let mut store = AgentStore::open(&path).unwrap();
    let version: i64 = store
        .conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let mut auth = store.load_auth().unwrap();
    assert_eq!(
        auth.credentials[&Provider::CommandCode].access_token,
        "user_fixture"
    );
    for (provider, token) in [
        (Provider::OpencodeZen, "sk-zen-fixture"),
        (Provider::OpencodeGo, "sk-go-fixture"),
    ] {
        let mut credential = Credential::api_key(token.into());
        credential.device_id = Some(format!("pp-{}", provider.id()));
        auth.credentials.insert(provider, credential);
    }
    store.save_auth(&auth).unwrap();
    drop(store);
    assert!(AgentStore::open(&path).unwrap().load_auth().unwrap() == auth);
}
