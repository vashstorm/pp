//! Codex-style user-level global permission rule store (`~/.pp/agent/permission.toml`).
//!
//! The standalone config file omits the `permissions.` prefix and supports:
//! - `[<profile>.filesystem]`, `[<profile>.filesystem.":workspace"]`
//! - `[<profile>.network]`
//! - `[<profile>.tools]`
//! - `[[rules]]` fine-grained command rules (`exact` / `prefix`)
//! - `[[sandbox]]` sandbox expansion rules (extra directories / protected path exemptions / outbound network)

mod codec;
mod store;

pub(crate) use store::Store;

use anyhow::{Result, ensure};
use pp_config::config::PermissionPolicy;
use pp_sandbox::SandboxOverlay;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Component, Path, PathBuf},
};

pub(super) const MAX_BYTES: u64 = 1024 * 1024;
pub(super) const PERMISSION_FILE: &str = "permission.toml";
pub(super) const PERMISSION_LOCK: &str = "permission.lock";
pub(super) const DEFAULT_PROFILE: &str = "default";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsAccess {
    #[default]
    Read,
    Write,
    #[serde(alias = "deny")]
    None,
}

impl FsAccess {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::None => "none",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkDomainDecision {
    Allow,
    Deny,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    #[default]
    Exact,
    Prefix,
}

impl MatchMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Prefix => "prefix",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FilesystemConfig {
    /// Absolute path or `~/...` path rules.
    pub(crate) host: BTreeMap<String, FsAccess>,
    /// Workspace-relative path rules (`[profile.filesystem.":workspace"]`).
    pub(crate) workspace: BTreeMap<String, FsAccess>,
}

impl FilesystemConfig {
    pub(super) fn is_empty(&self) -> bool {
        self.host.is_empty() && self.workspace.is_empty()
    }

    fn validate(&self) -> Result<()> {
        for raw in self.host.keys() {
            validate_host_fs_path(raw)?;
        }
        for raw in self.workspace.keys() {
            validate_workspace_fs_path(raw)?;
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for FilesystemConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = toml::map::Map::<String, toml::Value>::deserialize(deserializer)?;
        let mut config = Self::default();
        for (key, value) in raw {
            if key == ":workspace" || key == ":workspace_roots" {
                let table = value.as_table().ok_or_else(|| {
                    serde::de::Error::custom(format!("{key} must be a TOML table"))
                })?;
                for (rel, access_val) in table {
                    let access: FsAccess = access_val
                        .clone()
                        .try_into()
                        .map_err(serde::de::Error::custom)?;
                    if config.workspace.insert(rel.clone(), access).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate workspace filesystem rule: {rel}"
                        )));
                    }
                }
            } else {
                let access: FsAccess = value.try_into().map_err(serde::de::Error::custom)?;
                config.host.insert(key, access);
            }
        }
        Ok(config)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct NetworkConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) domains: BTreeMap<String, NetworkDomainDecision>,
}

impl NetworkConfig {
    pub(super) fn is_empty(&self) -> bool {
        self.enabled.is_none() && self.domains.is_empty()
    }

    fn validate(&self) -> Result<()> {
        for domain in self.domains.keys() {
            ensure!(
                !domain.trim().is_empty()
                    && domain == domain.trim()
                    && !domain.chars().any(char::is_control),
                "invalid network domain pattern"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub(crate) struct Profile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) extends: Option<String>,
    #[serde(default)]
    pub(crate) filesystem: FilesystemConfig,
    #[serde(default)]
    pub(crate) network: NetworkConfig,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) tools: BTreeMap<String, PermissionPolicy>,
}

impl Profile {
    pub(super) fn is_empty(&self) -> bool {
        self.description.is_none()
            && self.extends.is_none()
            && self.filesystem.is_empty()
            && self.network.is_empty()
            && self.tools.is_empty()
    }

    fn validate(&self, defined_profiles: &BTreeMap<String, Profile>) -> Result<()> {
        if let Some(extends) = &self.extends {
            ensure!(
                matches!(extends.as_str(), ":workspace" | ":read-only")
                    || defined_profiles.contains_key(extends),
                "unknown permission profile in extends: {extends}"
            );
        }
        self.filesystem.validate()?;
        self.network.validate()?;
        for tool in self.tools.keys() {
            validate_tool_name(tool)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SandboxGrant {
    pub command: String,
    #[serde(default)]
    pub writable: Vec<PathBuf>,
    #[serde(default)]
    pub allow_protected: Vec<PathBuf>,
    #[serde(default)]
    pub network: bool,
}

impl SandboxGrant {
    pub(super) fn validate(&self) -> Result<()> {
        for paths in [&self.writable, &self.allow_protected] {
            ensure!(
                paths.iter().all(|path| {
                    path.is_absolute()
                        && !path
                            .components()
                            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
                }),
                "invalid permission rule path"
            );
            ensure!(
                paths.windows(2).all(|pair| pair[0] < pair[1]),
                "invalid permission scope order"
            );
        }
        ensure!(!self.command.trim().is_empty(), "empty permission command");
        ensure!(
            self.network || !self.writable.is_empty() || !self.allow_protected.is_empty(),
            "empty permission scope"
        );
        Ok(())
    }

    pub fn summary(&self, cwd: Option<&Path>) -> String {
        let writes = self
            .writable
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let protected = self
            .allow_protected
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let command = pp_core::logging::diagnostic_text(&self.command);
        let mut lines = Vec::new();
        if let Some(cwd) = cwd {
            lines.push(format!("Directory: {}", cwd.display()));
        }
        if !writes.is_empty() {
            lines.push(format!("Extra writable directories: {writes}"));
        }
        if !protected.is_empty() {
            lines.push(format!("Protected paths allowed: {protected}"));
        }
        if self.network {
            lines.push("Allow outbound network for this command and its subprocesses".to_owned());
        }
        lines.push(format!("Command: {command}"));
        pp_core::text::display_text(&lines.join("\n")).into_owned()
    }
}

fn default_allow() -> PermissionPolicy {
    PermissionPolicy::Allow
}

fn deserialize_decision<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<PermissionPolicy, D::Error> {
    let raw = String::deserialize(deserializer)?;
    match raw.as_str() {
        "allow" => Ok(PermissionPolicy::Allow),
        "prompt" => Ok(PermissionPolicy::Prompt),
        "deny" | "forbidden" => Ok(PermissionPolicy::Deny),
        other => Err(serde::de::Error::custom(format!(
            "unknown rule decision \"{other}\"; expected allow, prompt, or deny"
        ))),
    }
}

/// Codex `[[rules]]` fine-grained command rule; defaults to `match = "exact"`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BashRule {
    pub pattern: Vec<String>,
    #[serde(default, rename = "match")]
    pub match_mode: MatchMode,
    #[serde(
        default = "default_allow",
        deserialize_with = "deserialize_decision",
        rename = "decision"
    )]
    pub policy: PermissionPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
}

impl BashRule {
    pub fn exact(pattern: Vec<String>, policy: PermissionPolicy) -> Self {
        Self {
            pattern,
            match_mode: MatchMode::Exact,
            policy,
            justification: None,
        }
    }

    pub fn prefix(pattern: Vec<String>, policy: PermissionPolicy) -> Self {
        Self {
            pattern,
            match_mode: MatchMode::Prefix,
            policy,
            justification: None,
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(!self.pattern.is_empty(), "empty bash permission pattern");
        for token in &self.pattern {
            ensure!(
                !token.trim().is_empty()
                    && token == token.trim()
                    && !token.chars().any(char::is_control),
                "invalid bash permission token"
            );
        }
        if let Some(justification) = &self.justification {
            ensure!(
                !justification.chars().any(char::is_control),
                "invalid rule justification"
            );
        }
        Ok(())
    }

    pub fn display_pattern(&self) -> String {
        pp_core::text::display_text(&self.pattern.join(" ")).into_owned()
    }

    pub(crate) fn summary(&self) -> String {
        let policy = codec::policy_str(self.policy);
        format!(
            "Rule ({}, {policy}): {}",
            self.match_mode.as_str(),
            self.display_pattern()
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Document {
    pub(crate) default_profile: String,
    pub(crate) profiles: BTreeMap<String, Profile>,
    pub(crate) rules: Vec<BashRule>,
    pub(crate) sandbox: Vec<SandboxGrant>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            default_profile: DEFAULT_PROFILE.to_owned(),
            profiles: BTreeMap::new(),
            rules: Vec::new(),
            sandbox: Vec::new(),
        }
    }
}

impl Document {
    pub(super) fn is_empty(&self) -> bool {
        self.default_profile == DEFAULT_PROFILE
            && self.profiles.values().all(Profile::is_empty)
            && self.rules.is_empty()
            && self.sandbox.is_empty()
    }

    pub(crate) fn active_profile_name(&self) -> &str {
        if self.default_profile.is_empty() {
            DEFAULT_PROFILE
        } else {
            &self.default_profile
        }
    }

    pub(super) fn active_profile_mut(&mut self) -> &mut Profile {
        let name = self.active_profile_name().to_owned();
        self.profiles.entry(name).or_default()
    }

    /// Merges the `extends` inheritance chain to resolve the aggregate view of the currently active profile.
    pub(crate) fn resolve_active_profile(&self) -> Result<ResolvedProfile> {
        let mut chain = Vec::new();
        let mut visited = HashSet::new();
        let mut cursor = Some(self.active_profile_name().to_owned());
        while let Some(name) = cursor {
            if matches!(name.as_str(), ":workspace" | ":read-only") {
                break;
            }
            ensure!(
                visited.insert(name.clone()),
                "cyclic permission profile inheritance detected at {name}"
            );
            if let Some(profile) = self.profiles.get(&name) {
                chain.push(profile);
                cursor = profile.extends.clone();
            } else {
                ensure!(
                    name == DEFAULT_PROFILE,
                    "unknown default_profile \"{name}\" in permission.toml"
                );
                break;
            }
        }
        let mut resolved = ResolvedProfile::default();
        for profile in chain.into_iter().rev() {
            for (path, access) in &profile.filesystem.host {
                resolved.filesystem.host.insert(path.clone(), *access);
            }
            for (path, access) in &profile.filesystem.workspace {
                resolved.filesystem.workspace.insert(path.clone(), *access);
            }
            if profile.network.enabled.is_some() {
                resolved.network.enabled = profile.network.enabled;
            }
            for (domain, decision) in &profile.network.domains {
                resolved.network.domains.insert(domain.clone(), *decision);
            }
            for (tool, policy) in &profile.tools {
                resolved.tools.insert(tool.clone(), *policy);
            }
        }
        Ok(resolved)
    }

    /// Filesystem and network overlay exported to `pp-sandbox`.
    pub(crate) fn sandbox_overlay(
        &self,
        cwd: &Path,
        home: Option<&Path>,
    ) -> Result<SandboxOverlay> {
        let active = self.resolve_active_profile()?;
        let mut overlay = SandboxOverlay {
            network: active.network.enabled,
            ..Default::default()
        };
        for (raw, access) in &active.filesystem.host {
            if let Some(abs) = expand_host_fs_path(raw, home) {
                match access {
                    FsAccess::Write => overlay.writable.push(abs),
                    FsAccess::Read => overlay.read_only.push(abs),
                    FsAccess::None => overlay.deny_read.push(abs),
                }
            }
        }
        for (rel, access) in &active.filesystem.workspace {
            let abs = cwd.join(rel);
            match access {
                FsAccess::Write => overlay.writable.push(abs),
                FsAccess::Read => overlay.read_only.push(abs),
                FsAccess::None => overlay.deny_read.push(abs),
            }
        }
        Ok(overlay)
    }

    pub(super) fn validate(&self) -> Result<()> {
        validate_profile_name(self.active_profile_name())?;
        for (name, profile) in &self.profiles {
            validate_profile_name(name)?;
            profile.validate(&self.profiles)?;
        }
        self.resolve_active_profile()?;
        let mut seen_rules = HashSet::with_capacity(self.rules.len());
        for rule in &self.rules {
            rule.validate()?;
            ensure!(
                seen_rules.insert((&rule.pattern, rule.match_mode)),
                "duplicate permission command rule"
            );
        }
        let mut seen_sandbox = HashSet::with_capacity(self.sandbox.len());
        for grant in &self.sandbox {
            grant.validate()?;
            ensure!(
                seen_sandbox.insert(grant),
                "duplicate permission sandbox rule"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResolvedProfile {
    pub(crate) filesystem: FilesystemConfig,
    pub(crate) network: NetworkConfig,
    pub(crate) tools: BTreeMap<String, PermissionPolicy>,
}

pub(super) fn validate_profile_name(name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty()
            && name == name.trim()
            && !name.starts_with(':')
            && !name.chars().any(|c| c.is_whitespace() || c.is_control()),
        "invalid permission profile name"
    );
    Ok(())
}

pub(super) fn validate_tool_name(tool: &str) -> Result<()> {
    ensure!(
        !tool.trim().is_empty() && tool == tool.trim() && !tool.chars().any(char::is_control),
        "invalid permission tool name"
    );
    Ok(())
}

pub(super) fn validate_host_fs_path(raw: &str) -> Result<()> {
    ensure!(
        !raw.trim().is_empty() && raw == raw.trim() && !raw.contains('\0'),
        "invalid permission filesystem path"
    );
    if let Some(rest) = raw.strip_prefix("~/") {
        ensure!(
            !rest.trim_matches('/').is_empty()
                && !Path::new(rest)
                    .components()
                    .any(|c| matches!(c, Component::ParentDir)),
            "invalid ~/ permission filesystem path"
        );
        return Ok(());
    }
    ensure!(
        raw == "~" || Path::new(raw).is_absolute(),
        "permission filesystem path must be absolute or start with ~/"
    );
    Ok(())
}

pub(super) fn validate_workspace_fs_path(raw: &str) -> Result<()> {
    ensure!(
        !raw.trim().is_empty() && raw == raw.trim() && !raw.contains('\0'),
        "invalid workspace permission filesystem path"
    );
    ensure!(
        !raw.starts_with('~') && !Path::new(raw).is_absolute(),
        "workspace permission filesystem path must be relative"
    );
    ensure!(
        !Path::new(raw).components().any(|c| matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )),
        "workspace permission filesystem path must not escape workspace"
    );
    Ok(())
}

pub(crate) fn expand_host_fs_path(raw: &str, home: Option<&Path>) -> Option<PathBuf> {
    if raw == "~" {
        return home.map(Path::to_path_buf);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home.map(|h| h.join(rest));
    }
    Some(PathBuf::from(raw))
}
