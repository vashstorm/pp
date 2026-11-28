use anyhow::{Result, ensure};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

use super::sections::{default_eval_max_timeout_seconds, default_eval_timeout_seconds};
use super::{
    OutputSettings, PermissionPolicy, SandboxSettings, default_bash_max_timeout_seconds,
    default_bash_timeout_seconds, default_read_max_bytes, default_read_max_lines,
};

#[derive(Clone, Copy, Serialize)]
pub struct ToolLimits {
    pub output: crate::config::OutputSettings,
    pub read_max_bytes: usize,
    pub read_max_lines: usize,
    pub bash_timeout_seconds: u64,
    pub bash_max_timeout_seconds: u64,
    pub eval_timeout_seconds: u64,
    pub eval_max_timeout_seconds: u64,
    pub rg: RgSettings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RgSettings {
    #[serde(rename = "contextBefore")]
    pub context_before: usize,
    #[serde(rename = "contextAfter")]
    pub context_after: usize,
}

impl Default for RgSettings {
    fn default() -> Self {
        Self {
            context_before: 1,
            context_after: 3,
        }
    }
}

/// The `tools:` section of settings.yml; field semantics are interpreted by `pp-permission` and `pp-sandbox`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ToolsSettingsFields")]
pub struct ToolsSettings {
    #[serde(flatten)]
    pub output: OutputSettings,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub permission: BTreeMap<String, PermissionPolicy>,
    #[serde(skip_serializing_if = "SandboxSettings::is_default")]
    pub sandbox: SandboxSettings,
}

// Read known keys directly so unknown values carrying YAML tags can still be skipped.
// flatten deserialization caches unknown values first, and the cache format does not support YAML tags.
#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ToolsSettingsFields {
    artifact_spill_threshold: usize,
    artifact_head_bytes: usize,
    output_max_columns: usize,
    artifact_max_bytes: usize,
    permission: BTreeMap<String, PermissionPolicy>,
    sandbox: SandboxSettings,
}

impl Default for ToolsSettingsFields {
    fn default() -> Self {
        let output = OutputSettings::default();
        Self {
            artifact_spill_threshold: output.artifact_spill_threshold,
            artifact_head_bytes: output.artifact_head_bytes,
            output_max_columns: output.output_max_columns,
            artifact_max_bytes: output.artifact_max_bytes,
            permission: BTreeMap::new(),
            sandbox: SandboxSettings::default(),
        }
    }
}

impl From<ToolsSettingsFields> for ToolsSettings {
    fn from(fields: ToolsSettingsFields) -> Self {
        Self {
            output: OutputSettings {
                artifact_spill_threshold: fields.artifact_spill_threshold,
                artifact_head_bytes: fields.artifact_head_bytes,
                output_max_columns: fields.output_max_columns,
                artifact_max_bytes: fields.artifact_max_bytes,
            },
            permission: fields.permission,
            sandbox: fields.sandbox,
        }
    }
}

impl ToolLimits {
    pub fn validate(&self) -> Result<()> {
        self.output.validate()?;
        ensure!(
            self.read_max_bytes > 0,
            "read_max_bytes must be greater than zero"
        );
        ensure!(
            self.read_max_lines > 0,
            "read_max_lines must be greater than zero"
        );
        ensure!(
            self.bash_timeout_seconds > 0,
            "bash_timeout_seconds must be greater than zero"
        );
        ensure!(
            self.bash_max_timeout_seconds > 0,
            "bash_max_timeout_seconds must be greater than zero"
        );
        ensure!(
            self.bash_timeout_seconds <= self.bash_max_timeout_seconds,
            "bash_timeout_seconds must not exceed bash_max_timeout_seconds"
        );
        ensure!(
            std::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(
                    self.bash_max_timeout_seconds
                ))
                .is_some(),
            "bash_max_timeout_seconds is too large"
        );
        ensure!(
            self.eval_timeout_seconds > 0,
            "eval.timeout_seconds must be greater than zero"
        );
        ensure!(
            self.eval_max_timeout_seconds > 0,
            "eval.max_timeout_seconds must be greater than zero"
        );
        ensure!(
            self.eval_timeout_seconds <= self.eval_max_timeout_seconds,
            "eval.timeout_seconds must not exceed eval.max_timeout_seconds"
        );
        ensure!(
            std::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(
                    self.eval_max_timeout_seconds
                ))
                .is_some(),
            "eval.max_timeout_seconds is too large"
        );
        Ok(())
    }
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            read_max_bytes: default_read_max_bytes(),
            read_max_lines: default_read_max_lines(),
            bash_timeout_seconds: default_bash_timeout_seconds(),
            bash_max_timeout_seconds: default_bash_max_timeout_seconds(),
            eval_timeout_seconds: default_eval_timeout_seconds(),
            eval_max_timeout_seconds: default_eval_max_timeout_seconds(),
            rg: RgSettings::default(),
            output: Default::default(),
        }
    }
}

impl<'de> Deserialize<'de> for ToolLimits {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct SavedLimits {
            #[serde(default)]
            output: super::OutputSettings,
            read_max_bytes: usize,
            read_max_lines: usize,
            bash_timeout_seconds: u64,
            #[serde(default, deserialize_with = "read_maximum")]
            bash_max_timeout_seconds: Option<u64>,
            #[serde(default = "default_eval_timeout_seconds")]
            eval_timeout_seconds: u64,
            #[serde(default, deserialize_with = "read_maximum")]
            eval_max_timeout_seconds: Option<u64>,
            #[serde(default)]
            rg: RgSettings,
        }

        let saved = SavedLimits::deserialize(deserializer)?;
        // Old checkpoints have no limit field, so keep their saved timeout to avoid making history sessions unrecoverable.
        // New configurations and checkpoints with an explicit limit still strictly require the default not to exceed the limit.
        let bash_max = saved
            .bash_max_timeout_seconds
            .unwrap_or_else(|| default_bash_max_timeout_seconds().max(saved.bash_timeout_seconds));
        let eval_max = saved
            .eval_max_timeout_seconds
            .unwrap_or_else(|| default_eval_max_timeout_seconds().max(saved.eval_timeout_seconds));
        Ok(Self {
            output: saved.output,
            read_max_bytes: saved.read_max_bytes,
            read_max_lines: saved.read_max_lines,
            bash_timeout_seconds: saved.bash_timeout_seconds,
            bash_max_timeout_seconds: bash_max,
            eval_timeout_seconds: saved.eval_timeout_seconds,
            eval_max_timeout_seconds: eval_max,
            rg: saved.rg,
        })
    }
}

fn read_maximum<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    u64::deserialize(deserializer).map(Some)
}
