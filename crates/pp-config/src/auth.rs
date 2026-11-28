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

use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;

pub mod http;
pub mod models;
pub mod service;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
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
    OpencodeZen,
    OpencodeGo,
}

impl Provider {
    pub const ALL: [Self; 8] = [
        Self::OpenaiCodex,
        Self::Zai,
        Self::Glm,
        Self::KimiCode,
        Self::GithubCopilot,
        Self::CommandCode,
        Self::OpencodeZen,
        Self::OpencodeGo,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::GithubCopilot => "github-copilot",
            Self::OpenaiCodex => "openai-codex",
            Self::KimiCode => "kimi-code",
            Self::Glm => "glm",
            Self::Zai => "zai",
            Self::CommandCode => "command-code",
            Self::OpencodeZen => "opencode-zen",
            Self::OpencodeGo => "opencode-go",
        }
    }

    pub fn base_url(self) -> &'static str {
        match self {
            Self::GithubCopilot => "https://api.githubcopilot.com",
            Self::OpenaiCodex => "https://chatgpt.com/backend-api/codex",
            Self::KimiCode => "https://api.kimi.com/coding/v1",
            Self::Glm => "https://open.bigmodel.cn/api/coding/paas/v4",
            Self::Zai => "https://api.z.ai/api/coding/paas/v4",
            Self::CommandCode => "https://api.commandcode.ai/provider/v1",
            Self::OpencodeZen => "https://opencode.ai/zen/v1",
            Self::OpencodeGo => "https://opencode.ai/zen/go/v1",
        }
    }

    pub fn protocol(self) -> crate::models::Protocol {
        if self == Self::OpenaiCodex {
            crate::models::Protocol::Response
        } else {
            crate::models::Protocol::Completion
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenaiCodex => "ChatGPT Plus/Pro (Codex Subscription)",
            Self::GithubCopilot => "GitHub Copilot",
            Self::KimiCode => "Kimi Code",
            Self::Glm => "GLM (Coding Plan)",
            Self::Zai => "Z.AI (GLM Coding Plan)",
            Self::CommandCode => "Command Code",
            Self::OpencodeZen => "OpenCode Zen",
            Self::OpencodeGo => "OpenCode Go",
        }
    }

    pub fn key_url(self) -> Option<&'static str> {
        match self {
            Self::Glm => Some("https://bigmodel.cn/coding-plan/personal/overview"),
            Self::Zai => Some("https://z.ai/manage-apikey/apikey-list"),
            Self::CommandCode => Some("https://commandcode.ai/studio/"),
            Self::OpencodeZen | Self::OpencodeGo => Some("https://opencode.ai/auth"),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub provider: Provider,
    pub path: PathBuf,
    pub generation: String,
}

// The credential types deliberately do not implement Debug, so tokens never reach diagnostic logs.
#[derive(Serialize, Deserialize, PartialEq, Eq)]
pub struct Credential {
    #[serde(default)]
    pub generation: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

impl Credential {
    pub fn api_key(access_token: String) -> Self {
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

    pub fn validate(&self, provider: Provider) -> Result<()> {
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
        if matches!(provider, Provider::OpencodeZen | Provider::OpencodeGo) {
            ensure!(
                self.device_id.as_deref().is_some_and(|id| {
                    !id.trim().is_empty() && reqwest::header::HeaderValue::from_str(id).is_ok()
                }),
                "auth credential incomplete; sign in again"
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

pub fn copilot_endpoint(value: &str) -> bool {
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

// Authorization persists in the agent.db auth_credentials table; the load/save path is agent.db itself.
#[derive(Default, Deserialize, PartialEq, Eq)]
pub struct Store {
    #[serde(default)]
    pub credentials: BTreeMap<Provider, Credential>,
}

impl Store {
    pub fn load(path: &Path) -> Result<Self> {
        crate::agent_store::AgentStore::open(path)?.load_auth()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut agent = crate::agent_store::AgentStore::open(path)?;
        agent.save_auth(self)
    }
}

/// Reads the legacy auth.yml for the one-time migration only; runtime authorization always goes through agent.db and never falls back to this file.
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
    // tempfile creates at 0600 by default; sync before the atomic replace, and old credentials survive any failure.
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

// All processes share the same lock; writes serialize across processes within one directory and one domain.
// The domain fixes the lock file name and the error wording: `auth.lock` for authorization refresh, `settings.lock` for settings saves;
// the two domains never block each other (each guards its own resource).
pub struct StoreLock(File);

/// Lock domain: fixes the lock file name and the fixed error text (redacted, without paths or configuration values).
#[derive(Clone, Copy, Debug)]
struct LockDomain {
    file_name: &'static str,
    label: &'static str,
    directory_error: &'static str,
}

impl LockDomain {
    const AUTH: Self = Self {
        file_name: "auth.lock",
        label: "auth",
        directory_error: "invalid auth file directory",
    };
    const SETTINGS: Self = Self {
        file_name: "settings.lock",
        label: "settings",
        directory_error: "invalid settings file directory",
    };
}

impl StoreLock {
    /// Acquires the sibling `auth.lock`, guarding agent.db authorization writes and migrations; the public signature and error text stay unchanged.
    pub fn acquire(path: &Path, cancellation: &CancellationToken) -> Result<Self> {
        Self::acquire_domain(path, cancellation, LockDomain::AUTH)
    }

    /// Acquires the sibling `settings.lock`, serializing cross-process `settings.yml` saves;
    /// settings saves have no external cancellation token, so callers pass an uncancelled token and lock waiting keeps the 30 second bound.
    pub(super) fn acquire_settings(path: &Path, cancellation: &CancellationToken) -> Result<Self> {
        Self::acquire_domain(path, cancellation, LockDomain::SETTINGS)
    }

    fn acquire_domain(
        path: &Path,
        cancellation: &CancellationToken,
        domain: LockDomain,
    ) -> Result<Self> {
        let parent = path.parent().context(domain.directory_error)?;
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        if parent != Path::new(".") {
            fs::create_dir_all(parent)
                .map_err(|_| anyhow::anyhow!("failed to create {} directory", domain.label))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(parent.join(domain.file_name))
            .map_err(|_| anyhow::anyhow!("failed to open {} lock", domain.label))?;
        ensure!(
            file.metadata()?.is_file(),
            "{} lock must be a regular file",
            domain.label
        );
        let started = Instant::now();
        loop {
            if cancellation.is_cancelled() {
                return Err(Cancelled.into());
            }
            match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(Self(file)),
                Err(rustix::io::Errno::INTR | rustix::io::Errno::WOULDBLOCK) => {}
                Err(_) => anyhow::bail!("failed to acquire the {} lock", domain.label),
            }
            ensure!(
                started.elapsed() < Duration::from_secs(30),
                "timed out waiting for the {} lock",
                domain.label
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

pub fn now() -> u64 {
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
