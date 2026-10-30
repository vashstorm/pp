use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::cancellation::{CancellationToken, Cancelled};

pub(crate) mod models;
pub(crate) mod service;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Provider {
    #[serde(alias = "copilot")]
    GithubCopilot,
    #[serde(alias = "codex", alias = "codex-plus")]
    OpenaiCodex,
    #[serde(alias = "kimi")]
    KimiCode,
    Glm,
    Zai,
    #[serde(alias = "commandcode")]
    CommandCode,
}

impl Provider {
    pub(crate) const ALL: [Self; 6] = [
        Self::OpenaiCodex,
        Self::Zai,
        Self::Glm,
        Self::KimiCode,
        Self::GithubCopilot,
        Self::CommandCode,
    ];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::GithubCopilot => "github-copilot",
            Self::OpenaiCodex => "openai-codex",
            Self::KimiCode => "kimi-code",
            Self::Glm => "glm",
            Self::Zai => "zai",
            Self::CommandCode => "command-code",
        }
    }

    pub(crate) fn base_url(self) -> &'static str {
        match self {
            Self::GithubCopilot => "https://api.githubcopilot.com",
            Self::OpenaiCodex => "https://chatgpt.com/backend-api/codex",
            Self::KimiCode => "https://api.kimi.com/coding/v1",
            Self::Glm => "https://open.bigmodel.cn/api/coding/paas/v4",
            Self::Zai => "https://api.z.ai/api/coding/paas/v4",
            Self::CommandCode => "https://api.commandcode.ai/provider/v1",
        }
    }

    pub(crate) fn protocol(self) -> crate::models::Protocol {
        if self == Self::OpenaiCodex {
            crate::models::Protocol::Response
        } else {
            crate::models::Protocol::Completion
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::OpenaiCodex => "ChatGPT Plus/Pro (Codex Subscription)",
            Self::GithubCopilot => "GitHub Copilot",
            Self::KimiCode => "Kimi Code",
            Self::Glm => "GLM (Coding Plan)",
            Self::Zai => "Z.AI (GLM Coding Plan)",
            Self::CommandCode => "Command Code",
        }
    }

    pub(crate) fn key_url(self) -> Option<&'static str> {
        match self {
            Self::Glm => Some("https://bigmodel.cn/coding-plan/personal/overview"),
            Self::Zai => Some("https://z.ai/manage-apikey/apikey-list"),
            Self::CommandCode => Some("https://commandcode.ai/studio/"),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Config {
    pub(crate) provider: Provider,
    pub(crate) path: PathBuf,
    pub(crate) generation: String,
}

// 凭证类型不实现 Debug，避免令牌进入诊断日志。
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credential {
    #[serde(default)]
    pub(crate) generation: String,
    pub(crate) access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) api_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) device_id: Option<String>,
}

impl Credential {
    pub(crate) fn api_key(access_token: String) -> Self {
        Self {
            generation: String::new(),
            access_token,
            refresh_token: None,
            expires_at: None,
            account_id: None,
            api_endpoint: None,
            device_id: None,
        }
    }

    pub(crate) fn validate(&self, provider: Provider) -> Result<()> {
        ensure!(
            !self.access_token.trim().is_empty(),
            "auth credential is empty; sign in again"
        );
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.access_token))
            .map_err(|_| anyhow::anyhow!("invalid auth credential format; sign in again"))?;
        if matches!(provider, Provider::OpenaiCodex | Provider::KimiCode) {
            ensure!(
                self.refresh_token.as_ref().is_some_and(|v| !v.is_empty())
                    && self.expires_at.is_some(),
                "auth credential incomplete; sign in again"
            );
        }
        if provider == Provider::OpenaiCodex {
            ensure!(
                self.account_id.as_ref().is_some_and(|id| !id.is_empty()),
                "Codex credential is missing the account identifier; sign in again"
            );
        }
        if let Some(endpoint) = &self.api_endpoint {
            ensure!(
                provider == Provider::GithubCopilot && copilot_endpoint(endpoint),
                "invalid auth service endpoint; sign in again"
            );
        }
        Ok(())
    }
}

pub(crate) fn copilot_endpoint(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path().trim_matches('/').is_empty()
            && url.host_str().is_some_and(|host| {
                host == "api.githubcopilot.com" || host.ends_with(".githubcopilot.com")
            })
    })
}

// 授权持久化在 agent.db 的 auth_credentials 表；load/save 的 path 即 agent.db。
#[derive(Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Store {
    #[serde(default)]
    pub(crate) credentials: BTreeMap<Provider, Credential>,
}

impl Store {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        crate::agent_store::AgentStore::open(path)?.load_auth()
    }

    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        let mut agent = crate::agent_store::AgentStore::open(path)?;
        agent.save_auth(self)
    }
}

/// 仅供一次性迁移读取旧 auth.yml；运行期授权一律走 agent.db，绝不回落到此文件。
pub(crate) fn load_legacy_store(path: &Path) -> Result<Store> {
    let store: Store = read_yaml(path, "auth")?.unwrap_or_default();
    for (provider, credential) in &store.credentials {
        credential.validate(*provider)?;
    }
    Ok(store)
}

pub(super) fn read_yaml<T: serde::de::DeserializeOwned>(
    path: &Path,
    kind: &str,
) -> Result<Option<T>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(_) => anyhow::bail!("failed to read the auth file"),
    };
    let metadata = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("failed to read auth file metadata"))?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
        "auth file must be a regular file readable/writable only by the current user (chmod 600)"
    );
    let mut bytes = Vec::new();
    (&mut file)
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("failed to read the auth file"))?;
    ensure!(bytes.len() <= 1024 * 1024, "auth file is too large");
    let text =
        std::str::from_utf8(&bytes).map_err(|_| anyhow::anyhow!("invalid auth file format"))?;
    crate::yaml::parse(text, kind).map(Some)
}

pub(super) fn save_yaml(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "auth file must be a regular file"
        );
    }
    let parent = path.parent().context("invalid auth file directory")?;
    // tempfile 默认以 0600 创建；原子替换前同步，失败时保留旧凭证。
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|_| anyhow::anyhow!("failed to create auth temp file"))?;
    serde_yaml::to_writer(&mut temp, value)
        .map_err(|_| anyhow::anyhow!("failed to serialize auth file"))?;
    temp.flush()
        .and_then(|_| temp.as_file().sync_all())
        .map_err(|_| anyhow::anyhow!("failed to sync auth file"))?;
    temp.persist(path)
        .map_err(|_| anyhow::anyhow!("failed to save auth file"))?;
    Ok(())
}

// 所有进程共享同一个锁；刷新期间持锁，避免轮换 refresh token 被旧进程覆盖。
pub(crate) struct StoreLock(File);

impl StoreLock {
    pub(crate) fn acquire(path: &Path, cancellation: &CancellationToken) -> Result<Self> {
        let parent = path.parent().context("invalid auth file directory")?;
        fs::create_dir_all(parent)
            .map_err(|_| anyhow::anyhow!("failed to create auth directory"))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(parent.join("auth.lock"))
            .map_err(|_| anyhow::anyhow!("failed to open auth lock"))?;
        ensure!(
            file.metadata()?.is_file(),
            "auth lock must be a regular file"
        );
        let started = Instant::now();
        loop {
            if cancellation.is_cancelled() {
                return Err(Cancelled.into());
            }
            match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(Self(file)),
                Err(rustix::io::Errno::INTR | rustix::io::Errno::WOULDBLOCK) => {}
                Err(_) => anyhow::bail!("failed to acquire the auth lock"),
            }
            ensure!(
                started.elapsed() < Duration::from_secs(30),
                "timed out waiting for the auth lock"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        while let Err(rustix::io::Errno::INTR) =
            rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock)
        {}
    }
}

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(super) fn generation() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("failed to generate an auth generation"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests;
