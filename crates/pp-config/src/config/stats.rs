use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StatsStyle {
    #[default]
    Default,
    Modern,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StatsSettings {
    #[serde(default)]
    pub style: StatsStyle,
}

impl StatsStyle {
    /// 复用配置的严格校验与原子更新，只改统计页面偏好。
    pub fn save(self, path: &Path) -> Result<()> {
        super::update_settings(path, |mut settings| {
            settings.stats.style = self;
            Ok((
                "stats",
                serde_yaml::to_value(settings.stats)
                    .context("Failed to serialize stats settings")?,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::read_settings;

    #[test]
    fn stats_style_defaults_and_persists_without_models() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent/settings.yml");
        assert_eq!(
            read_settings(&path).unwrap().stats.style,
            StatsStyle::Default
        );
        assert!(!path.exists());
        StatsStyle::Modern.save(&path).unwrap();
        assert_eq!(
            read_settings(&path).unwrap().stats.style,
            StatsStyle::Modern
        );
        std::fs::write(
            &path,
            "# 保留说明\ndebug: true # 调试\nstats:\n  style: modern\n",
        )
        .unwrap();
        StatsStyle::Default.save(&path).unwrap();
        assert_eq!(
            read_settings(&path).unwrap().stats.style,
            StatsStyle::Default
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .starts_with("# 保留说明\ndebug: true # 调试\n")
        );
    }

    #[test]
    fn stats_style_rejects_invalid_settings_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.yml");
        for text in [
            "stats: {style: unknown}",
            "stats: {style: Modern}",
            "stats: {style: null}",
            "stats: {style: 1}",
            "stats: {style: modern, extra: true}",
            "stats: null",
            "stats: []",
            "unknown: secret-marker",
        ] {
            std::fs::write(&path, text).unwrap();
            assert!(read_settings(&path).is_err());
            let error = StatsStyle::Modern.save(&path).unwrap_err();
            assert!(!format!("{error:#}").contains("secret-marker"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(StatsStyle::Modern.save(&path).is_err());
    }
}
