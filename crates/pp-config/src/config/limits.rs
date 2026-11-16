use serde::{Deserialize, Deserializer};

use super::{OutputSettings, RgSettings, ToolLimits, default_bash_max_timeout_seconds};

impl<'de> Deserialize<'de> for ToolLimits {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct SavedLimits {
            #[serde(default)]
            output: OutputSettings,
            read_max_bytes: usize,
            read_max_lines: usize,
            bash_timeout_seconds: u64,
            #[serde(default, deserialize_with = "read_maximum")]
            bash_max_timeout_seconds: Option<u64>,
            #[serde(default)]
            rg: RgSettings,
        }

        let saved = SavedLimits::deserialize(deserializer)?;
        // 旧 checkpoint 没有上限字段，沿用其已保存的默认时限，避免历史会话无法恢复。
        // 新配置与带显式上限的 checkpoint 仍严格校验默认值不得超过上限。
        let maximum = saved
            .bash_max_timeout_seconds
            .unwrap_or_else(|| default_bash_max_timeout_seconds().max(saved.bash_timeout_seconds));
        Ok(Self {
            output: saved.output,
            read_max_bytes: saved.read_max_bytes,
            read_max_lines: saved.read_max_lines,
            bash_timeout_seconds: saved.bash_timeout_seconds,
            bash_max_timeout_seconds: maximum,
            rg: saved.rg,
        })
    }
}

fn read_maximum<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    u64::deserialize(deserializer).map(Some)
}
