use anyhow::{Result, ensure};
use serde::{Deserialize, Deserializer, Serialize};

use super::sections::{default_eval_max_timeout_seconds, default_eval_timeout_seconds};
use super::{
    default_bash_max_timeout_seconds, default_bash_timeout_seconds, default_read_max_bytes,
    default_read_max_lines,
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
#[serde(default, deny_unknown_fields)]
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

/// settings.yml 的 `tools:` 小节；字段语义由 `pp-permission` 与 `pp-sandbox` 解释。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsSettings {
    #[serde(flatten)]
    pub output: crate::config::OutputSettings,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub permission: std::collections::BTreeMap<String, crate::config::PermissionPolicy>,
    #[serde(
        default,
        skip_serializing_if = "crate::config::SandboxSettings::is_default"
    )]
    pub sandbox: crate::config::SandboxSettings,
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
        // 旧 checkpoint 没有上限字段，沿用其已保存的默认时限，避免历史会话无法恢复。
        // 新配置与带显式上限的 checkpoint 仍严格校验默认值不得超过上限。
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
