use super::{AgentStore, UNAVAILABLE, role_name};
use crate::{
    auth::{self, StoreLock},
    cancellation::{CancellationToken, check},
    config::{ModelPreferences, Settings},
    yaml,
};
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_yaml::{Mapping, Value};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

const FIELDS: &[&str] = &[
    "default_model",
    "favorite_models",
    "model_roles",
    "think_level",
];
const CONFLICT: &str = "Legacy migration conflict; source files and database were preserved.";

pub(crate) fn initialize(dir: &Path, cancellation: &CancellationToken) -> Result<()> {
    let path = dir.join("agent.db");
    let mut store = AgentStore::open(&path)?;
    if stage(&store)?.as_deref() == Some("ready") {
        return Ok(());
    }
    let _lock = StoreLock::acquire(&path, cancellation)?;
    if stage(&store)?.as_deref() == Some("ready") {
        return Ok(());
    }
    check(cancellation)?;
    if stage(&store)?.is_none() {
        let settings = snapshot(&dir.join("settings.yml"))?;
        let (preferences, _) =
            split_settings(settings.as_ref().map(|s| s.text.as_str()).unwrap_or("{}"))?;
        let credentials = auth::load_legacy_store(&dir.join("auth.yml"))?;
        check(cancellation)?;
        import(&mut store, &preferences, &credentials)?;
    }
    cleanup(&store, dir, cancellation)?;
    check(cancellation)?;
    store
        .conn
        .execute("UPDATE migration_state SET stage='ready' WHERE id=1", [])
        .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
    Ok(())
}

fn stage(store: &AgentStore) -> Result<Option<String>> {
    store
        .conn
        .query_row("SELECT stage FROM migration_state WHERE id=1", [], |r| {
            r.get(0)
        })
        .optional()
        .map_err(|_| anyhow::anyhow!(UNAVAILABLE))
}

fn split_settings(text: &str) -> Result<(ModelPreferences, Mapping)> {
    let mut remaining: Mapping = yaml::parse(text, "settings")?;
    let mut migrated = Mapping::new();
    for field in FIELDS {
        let key = Value::String((*field).into());
        if let Some(value) = remaining.remove(&key) {
            migrated.insert(key, value);
        }
    }
    let mut preferences: ModelPreferences = yaml::parse(
        &serde_yaml::to_string(&migrated)
            .map_err(|_| anyhow::anyhow!("Invalid legacy settings.yml."))?,
        "settings",
    )?;
    preferences.validate()?;
    normalize(&mut preferences);
    let settings: Settings = yaml::parse(
        &serde_yaml::to_string(&remaining)
            .map_err(|_| anyhow::anyhow!("Invalid legacy settings.yml."))?,
        "settings",
    )?;
    settings.validate()?;
    Ok((preferences, remaining))
}

fn normalize(preferences: &mut ModelPreferences) {
    let mut seen = HashSet::new();
    preferences
        .favorite_models
        .retain(|reference| seen.insert(reference.clone()));
}

fn import(
    store: &mut AgentStore,
    preferences: &ModelPreferences,
    credentials: &auth::Store,
) -> Result<()> {
    let result = (|| -> Result<()> {
        let tx = store
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE preferences SET default_model=?1,think_level=?2 WHERE id=1",
            params![preferences.default_model, preferences.think_level.as_str()],
        )?;
        for (position, reference) in preferences.favorite_models.iter().enumerate() {
            tx.execute(
                "INSERT INTO favorite_models(model_reference,position) VALUES (?1,?2)",
                params![reference, position as i64],
            )?;
        }
        for (role, assignment) in &preferences.model_roles {
            tx.execute(
                "INSERT INTO model_roles(role,enabled,model_reference,effort) VALUES (?1,?2,?3,?4)",
                params![
                    role_name(*role),
                    assignment.enabled,
                    assignment.model,
                    assignment.effort.as_str()
                ],
            )?;
        }
        for (provider, credential) in &credentials.credentials {
            tx.execute("INSERT INTO auth_credentials(provider,generation,access_token,refresh_token,expires_at,account_id,api_endpoint,device_id)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8)", params![provider.id(),credential.generation,credential.access_token,
                    credential.refresh_token,credential.expires_at.map(i64::try_from).transpose()?,
                    credential.account_id,credential.api_endpoint,credential.device_id])?;
        }
        tx.execute(
            "INSERT INTO migration_state(id,stage) VALUES (1,'imported')",
            [],
        )?;
        tx.commit()?;
        Ok(())
    })();
    result.map_err(|_| anyhow::anyhow!(UNAVAILABLE))
}

struct Snapshot {
    text: String,
    device: u64,
    inode: u64,
    file: File,
}

fn snapshot(path: &Path) -> Result<Option<Snapshot>> {
    let read = || -> Result<Option<Snapshot>> {
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            )
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "Legacy source must be a regular file.");
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok(Some(Snapshot {
            text,
            device: metadata.dev(),
            inode: metadata.ino(),
            file,
        }))
    };
    read().map_err(|_| anyhow::anyhow!("Legacy migration source unavailable."))
}

fn unchanged(path: &Path, baseline: &Snapshot) -> Result<()> {
    let current = snapshot(path)?.ok_or_else(|| anyhow::anyhow!(CONFLICT))?;
    let held = baseline
        .file
        .metadata()
        .map_err(|_| anyhow::anyhow!(CONFLICT))?;
    ensure!(
        current.device == baseline.device
            && current.inode == baseline.inode
            && held.dev() == current.device
            && held.ino() == current.inode
            && current.text == baseline.text,
        CONFLICT
    );
    Ok(())
}

fn cleanup(store: &AgentStore, dir: &Path, cancellation: &CancellationToken) -> Result<()> {
    let expected = store.preferences()?;
    let settings_path = dir.join("settings.yml");
    let settings = snapshot(&settings_path)?;
    if let Some(settings) = &settings {
        let mapping: Mapping = yaml::parse(&settings.text, "settings")?;
        let (actual, _) = split_settings(&settings.text)?;
        // 缺失字段可能已经在上次清理中删除；只比较仍存在的源字段。
        for field in FIELDS {
            if mapping.contains_key(Value::String((*field).into())) {
                let matches = match *field {
                    "default_model" => actual.default_model == expected.default_model,
                    "favorite_models" => actual.favorite_models == expected.favorite_models,
                    "model_roles" => actual.model_roles == expected.model_roles,
                    "think_level" => actual.think_level == expected.think_level,
                    _ => unreachable!(),
                };
                ensure!(matches, CONFLICT);
            }
        }
    }
    let auth_path = dir.join("auth.yml");
    // 安全旧读取先检查权限与大小；比较和删除使用随后持有的精确快照。
    auth::load_legacy_store(&auth_path)?;
    let legacy_auth = snapshot(&auth_path)?;
    if let Some(baseline) = &legacy_auth {
        let metadata = baseline
            .file
            .metadata()
            .map_err(|_| anyhow::anyhow!(CONFLICT))?;
        ensure!(
            metadata.mode() & 0o077 == 0 && baseline.text.len() <= 1024 * 1024,
            CONFLICT
        );
        let actual: auth::Store = yaml::parse(&baseline.text, "auth")?;
        for (provider, credential) in &actual.credentials {
            credential.validate(*provider)?;
        }
        let expected = store.load_auth()?;
        ensure!(actual == expected, CONFLICT);
        unchanged(&auth_path, baseline)?;
    }
    // 先检查两端冲突再清理任一源文件，失败始终保留数据库权威。
    check(cancellation)?;
    if let Some(settings) = settings {
        let updated = yaml::remove_fields(&settings.text, FIELDS)?;
        if updated != settings.text {
            let mut temporary = tempfile::NamedTempFile::new_in(dir)
                .map_err(|_| anyhow::anyhow!("Legacy settings cleanup failed."))?;
            temporary
                .write_all(updated.as_bytes())
                .and_then(|_| temporary.as_file().sync_all())
                .map_err(|_| anyhow::anyhow!("Legacy settings cleanup failed."))?;
            unchanged(&settings_path, &settings)?;
            temporary
                .persist(&settings_path)
                .map_err(|_| anyhow::anyhow!("Legacy settings cleanup failed."))?;
        }
    }
    check(cancellation)?;
    if let Some(baseline) = legacy_auth {
        unchanged(&auth_path, &baseline)?;
        fs::remove_file(&auth_path)
            .map_err(|_| anyhow::anyhow!("Legacy authorization cleanup failed."))?;
    }
    File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(|_| anyhow::anyhow!("Legacy migration directory sync failed."))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelRole, ThinkLevel};
    use std::os::unix::fs::PermissionsExt;

    fn legacy(dir: &Path) {
        fs::write(dir.join("settings.yml"), "# 保留注释\ndefault_model: fixture/one\nfavorite_models: [fixture/one, fixture/one, removed/two]\nthink_level: high\nmodel_roles: {fast: {model: fixture/two, effort: low}}\ndisabled_skills: [keep-disabled]\nskill_dirs: [unchanged]\n").unwrap();
        fs::write(dir.join("auth.yml"), "credentials:\n  glm: {generation: fixed-glm, access_token: fixture-private-glm}\n  zai: {generation: fixed-zai, access_token: fixture-private-zai}\n").unwrap();
        fs::set_permissions(dir.join("auth.yml"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(dir.join("models.yml"), "manual unchanged").unwrap();
        fs::write(dir.join("models-sub.yml"), "subscription unchanged").unwrap();
    }

    #[test]
    fn sqlite_migration_preserves_preferences_and_auth() {
        let root = tempfile::tempdir().unwrap();
        legacy(root.path());
        let token = CancellationToken::new();
        initialize(root.path(), &token).unwrap();
        initialize(root.path(), &token).unwrap();
        let store = AgentStore::open(&root.path().join("agent.db")).unwrap();
        let prefs = store.preferences().unwrap();
        assert_eq!(prefs.default_model.as_deref(), Some("fixture/one"));
        assert_eq!(prefs.favorite_models, ["fixture/one", "removed/two"]);
        assert_eq!(prefs.think_level, ThinkLevel::High);
        assert_eq!(prefs.model_roles[&ModelRole::Fast].effort, ThinkLevel::Low);
        let auth = store.load_auth().unwrap();
        assert_eq!(
            auth.credentials[&auth::Provider::Glm].access_token,
            "fixture-private-glm"
        );
        assert_eq!(
            auth.credentials[&auth::Provider::Zai].generation,
            "fixed-zai"
        );
        let text = fs::read_to_string(root.path().join("settings.yml")).unwrap();
        assert!(text.contains("# 保留注释"));
        let settings: Mapping = yaml::parse(&text, "settings").unwrap();
        assert_eq!(settings["disabled_skills"][0], "keep-disabled");
        assert_eq!(settings["skill_dirs"][0], "unchanged");
        for field in FIELDS {
            assert!(!settings.contains_key(Value::String((*field).into())));
        }
        assert!(!root.path().join("auth.yml").exists());
        assert_eq!(
            fs::read_to_string(root.path().join("models.yml")).unwrap(),
            "manual unchanged"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("models-sub.yml")).unwrap(),
            "subscription unchanged"
        );
    }

    #[test]
    fn sqlite_migration_resume_and_conflicts() {
        let root = tempfile::tempdir().unwrap();
        legacy(root.path());
        let mut store = AgentStore::open(&root.path().join("agent.db")).unwrap();
        let source = fs::read_to_string(root.path().join("settings.yml")).unwrap();
        let (prefs, _) = split_settings(&source).unwrap();
        let credentials = auth::load_legacy_store(&root.path().join("auth.yml")).unwrap();
        store.conn.execute_batch("CREATE TRIGGER reject_import BEFORE INSERT ON auth_credentials BEGIN SELECT RAISE(ABORT,'fixture-private'); END;").unwrap();
        assert!(import(&mut store, &prefs, &credentials).is_err());
        assert!(stage(&store).unwrap().is_none());
        assert!(store.load_auth().unwrap().credentials.is_empty());
        assert_eq!(store.preferences().unwrap(), ModelPreferences::default());
        store
            .conn
            .execute_batch("DROP TRIGGER reject_import")
            .unwrap();
        import(&mut store, &prefs, &credentials).unwrap();
        fs::write(
            root.path().join("settings.yml"),
            source.replace("think_level: high", "think_level: low"),
        )
        .unwrap();
        let error = initialize(root.path(), &CancellationToken::new()).unwrap_err();
        assert_eq!(error.to_string(), CONFLICT);
        assert!(root.path().join("auth.yml").exists());
        assert_eq!(store.preferences().unwrap().think_level, ThinkLevel::High);
        // 上次退出已清理授权和一个字段；其余清理仍可继续。
        fs::remove_file(root.path().join("auth.yml")).unwrap();
        fs::write(
            root.path().join("settings.yml"),
            yaml::remove_fields(&source, &["think_level"]).unwrap(),
        )
        .unwrap();
        initialize(root.path(), &CancellationToken::new()).unwrap();
        assert_eq!(stage(&store).unwrap().as_deref(), Some("ready"));
        // ready 后人为放回旧凭证也不能在登出或重启时复活。
        fs::write(
            root.path().join("auth.yml"),
            "credentials: {glm: {access_token: resurrected}}\n",
        )
        .unwrap();
        store.save_auth(&auth::Store::default()).unwrap();
        initialize(root.path(), &CancellationToken::new()).unwrap();
        assert!(store.load_auth().unwrap().credentials.is_empty());
    }
}
