use super::{Credential, Provider, Store, StoreLock, generation, models};
use crate::cancellation::{CancellationToken, check};
use anyhow::{Result, ensure};
use std::{
    path::Path,
    process::{Command, Stdio},
};

pub(crate) enum Progress {
    Verification { url: String, code: String },
    Discovering,
    BrowserFailed,
}

pub(crate) struct Update {
    pub(crate) logged: Vec<Provider>,
    pub(crate) message: String,
}

pub(crate) fn list(dir: &Path) -> Result<Vec<Provider>> {
    // list 没有外部取消令牌；初始化仅读取迁移状态，用独立令牌即可。
    crate::agent_store::initialize(dir, &CancellationToken::new())?;
    let store = Store::load(&dir.join("agent.db"))?;
    Ok(Provider::ALL
        .into_iter()
        .filter(|p| store.credentials.contains_key(p))
        .collect())
}

pub(crate) fn login(
    dir: &Path,
    provider: Provider,
    key: Option<String>,
    cancellation: &CancellationToken,
    progress: &mut dyn FnMut(Progress),
) -> Result<Update> {
    let http = crate::llm::auth::Http::new()?;
    let credential = if provider.key_url().is_some() {
        let key = key.unwrap_or_default();
        ensure!(
            !key.trim().is_empty() && key.len() <= 16384,
            "API key must contain 1–16384 bytes."
        );
        Credential::api_key(key.trim().to_owned())
    } else {
        http.login(provider, cancellation, |url, code| {
            progress(Progress::Verification {
                url: url.to_owned(),
                code: code.to_owned(),
            });
            if open_browser(url).is_err() {
                progress(Progress::BrowserFailed);
            }
        })?
    };
    commit_login(dir, provider, credential, cancellation)?;
    discover(dir, provider, cancellation, progress)
}

pub(crate) fn commit_login(
    dir: &Path,
    provider: Provider,
    mut credential: Credential,
    cancellation: &CancellationToken,
) -> Result<()> {
    credential.validate(provider)?;
    let path = dir.join("agent.db");
    // 在取得授权锁之前完成一次性迁移；Store::load 不再读旧文件，锁内不触发迁移。
    crate::agent_store::initialize(dir, cancellation)?;
    let _lock = StoreLock::acquire(&path, cancellation)?;
    let mut store = Store::load(&path)?;
    // 只有能确认身份相同才沿用缓存；未知身份按新授权处理。
    credential.generation = match store.credentials.get(&provider) {
        Some(old)
            if (old.account_id.is_some() && old.account_id == credential.account_id
                || old.access_token == credential.access_token)
                && !old.generation.is_empty() =>
        {
            old.generation.clone()
        }
        _ => generation()?,
    };
    check(cancellation)?;
    store.credentials.insert(provider, credential);
    store.save(&path)
}

pub(crate) fn discover(
    dir: &Path,
    provider: Provider,
    cancellation: &CancellationToken,
    progress: &mut dyn FnMut(Progress),
) -> Result<Update> {
    progress(Progress::Discovering);
    let path = dir.join("agent.db");
    // 网络发现之外的所有存储步骤都走 agent.db；初始化必须在取锁前完成。
    crate::agent_store::initialize(dir, cancellation)?;
    let result = (|| {
        let http = crate::llm::auth::Http::new()?;
        let mut credential = {
            let _lock = StoreLock::acquire(&path, cancellation)?;
            let mut store = Store::load(&path)?;
            let mut credential = store
                .credentials
                .remove(&provider)
                .ok_or_else(|| anyhow::anyhow!("Provider is logged out; use /login."))?;
            ensure!(
                !credential.generation.is_empty(),
                "Authorization needs to be renewed; use /login."
            );
            if credential
                .expires_at
                .is_some_and(|expiry| expiry <= super::now().saturating_add(60))
            {
                credential = http.renew(provider, &credential, cancellation)?;
                // 刷新令牌与原授权共享同一目录版本。
                store.credentials.insert(provider, credential);
                check(cancellation)?;
                store.save(&path)?;
                store.credentials.remove(&provider).expect("刚保存的授权")
            } else {
                credential
            }
        };
        let discovered = http.discover(provider, &credential, cancellation)?;
        let _lock = StoreLock::acquire(&path, cancellation)?;
        let store = Store::load(&path)?;
        ensure!(
            store
                .credentials
                .get(&provider)
                .is_some_and(|current| current.generation == credential.generation),
            "Authorization changed; retry model discovery."
        );
        let models_path = dir.join("models-sub.yml");
        let mut catalog = models::Catalog::load(&models_path)?;
        let count = discovered.len();
        catalog.providers.insert(
            provider.id().into(),
            models::Entry {
                auth: provider,
                generation: std::mem::take(&mut credential.generation),
                protocol: provider.protocol(),
                models: discovered,
            },
        );
        check(cancellation)?;
        catalog.save(&models_path)?;
        Ok(count)
    })();
    // 授权提交之后发现失败也必须显示已保存状态；不把失败伪装成发现成功。
    let message = match result {
        Ok(count) => format!("Logged in. Discovered {count} models. Use /model to select a model."),
        Err(error) => format!(
            "Authorization saved; model discovery did not complete: {error:#}. Retry discovery in /login."
        ),
    };
    Ok(Update {
        logged: list(dir)?,
        message,
    })
}

pub(crate) fn logout(
    dir: &Path,
    provider: Provider,
    cancellation: &CancellationToken,
) -> Result<Update> {
    let path = dir.join("agent.db");
    // 在取得授权锁之前完成一次性迁移；锁内不再读旧 auth.yml。
    crate::agent_store::initialize(dir, cancellation)?;
    let _lock = StoreLock::acquire(&path, cancellation)?;
    let mut store = Store::load(&path)?;
    let models_path = dir.join("models-sub.yml");
    check(cancellation)?;
    store.credentials.remove(&provider);
    store.save(&path)?;
    let saved = models::Catalog::load(&models_path).and_then(|mut catalog| {
        catalog.providers.retain(|_, entry| entry.auth != provider);
        catalog.save(&models_path)
    });
    let message = if saved.is_ok() {
        "Logged out locally.".into()
    } else {
        "Logged out locally; cached models could not be removed and will be ignored.".into()
    };
    Ok(Update {
        logged: store.credentials.keys().copied().collect(),
        message,
    })
}

pub(crate) fn open_browser(url: &str) -> Result<()> {
    let parsed =
        reqwest::Url::parse(url).map_err(|_| anyhow::anyhow!("Invalid authorization URL."))?;
    ensure!(
        parsed.scheme() == "https"
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && !url.chars().any(char::is_control),
        "Invalid authorization URL."
    );
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = Command::new(program)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| anyhow::anyhow!("Could not open browser; open the displayed URL manually."))?;
    let started = std::time::Instant::now();
    let opened = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if started.elapsed() < std::time::Duration::from_secs(3) => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    ensure!(
        opened,
        "Could not open browser; open the displayed URL manually."
    );
    Ok(())
}
