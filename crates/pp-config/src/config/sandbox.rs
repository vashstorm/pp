use std::{
    path::{Component, Path},
    str::FromStr,
};

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

/// 沙盒运行模式；默认 `Auto`（在 macOS 与 Linux 平台可用时开启 `workspace-write`，其余关闭）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    #[default]
    Auto,
    WorkspaceWrite,
    ReadOnly,
    Off,
}

impl SandboxMode {
    pub const ALL: [Self; 4] = [Self::Auto, Self::WorkspaceWrite, Self::ReadOnly, Self::Off];
    pub const OPTIONS: &str = "auto, workspace-write, read-only, off";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::WorkspaceWrite => "workspace-write",
            Self::ReadOnly => "read-only",
            Self::Off => "off",
        }
    }
}

impl FromStr for SandboxMode {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "workspace-write" => Ok(Self::WorkspaceWrite),
            "read-only" => Ok(Self::ReadOnly),
            "off" => Ok(Self::Off),
            _ => bail!("invalid sandbox mode \"{raw}\"; options: {}", Self::OPTIONS),
        }
    }
}

fn default_network() -> bool {
    false
}

/// settings.yml `tools.sandbox` 配置。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct SandboxSettings {
    pub mode: SandboxMode,
    #[serde(default = "default_network")]
    pub network: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub writable: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deny_read: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow_read: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub protect: Vec<String>,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            mode: SandboxMode::default(),
            network: default_network(),
            writable: Vec::new(),
            deny_read: Vec::new(),
            allow_read: Vec::new(),
            protect: Vec::new(),
        }
    }
}

impl SandboxSettings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub fn validate(&self) -> Result<()> {
        for (field, paths) in [
            ("tools.sandbox.writable", &self.writable),
            ("tools.sandbox.denyRead", &self.deny_read),
            ("tools.sandbox.allowRead", &self.allow_read),
        ] {
            for path in paths {
                validate_host_path(field, path)?;
            }
        }
        for path in &self.protect {
            validate_protect_path(path)?;
        }
        Ok(())
    }
}

fn validate_host_path(field: &str, raw: &str) -> Result<()> {
    let trimmed = raw.trim();
    ensure!(
        !trimmed.is_empty() && trimmed == raw,
        "{field} entries must not be blank or have surrounding whitespace"
    );
    ensure!(!raw.contains('\0'), "{field} entries must not contain NUL");
    if let Some(rest) = raw.strip_prefix("~/") {
        ensure!(
            !rest.trim_matches('/').is_empty(),
            "{field} home-relative path \"{raw}\" must name a subpath under ~/"
        );
        ensure!(
            !Path::new(rest)
                .components()
                .any(|c| matches!(c, Component::ParentDir)),
            "{field} path \"{raw}\" must not contain '..'"
        );
        return Ok(());
    }
    ensure!(
        Path::new(raw).is_absolute(),
        "{field} path \"{raw}\" must be an absolute path or start with ~/"
    );
    Ok(())
}

fn validate_protect_path(raw: &str) -> Result<()> {
    let trimmed = raw.trim();
    ensure!(
        !trimmed.is_empty() && trimmed == raw,
        "tools.sandbox.protect entries must not be blank or have surrounding whitespace"
    );
    ensure!(
        !raw.contains('\0'),
        "tools.sandbox.protect entries must not contain NUL"
    );
    ensure!(
        !raw.starts_with('~') && !Path::new(raw).is_absolute(),
        "tools.sandbox.protect path \"{raw}\" must be relative to the working directory"
    );
    let mut has_normal = false;
    for component in Path::new(raw).components() {
        match component {
            Component::Normal(_) => has_normal = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("tools.sandbox.protect path \"{raw}\" must not escape the working directory");
            }
        }
    }
    ensure!(
        has_normal,
        "tools.sandbox.protect path \"{raw}\" must name a subpath within the working directory"
    );
    Ok(())
}
