use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IconMode {
    #[default]
    Nerd,
    Ascii,
}

impl IconMode {
    pub const ALL: [Self; 2] = [Self::Nerd, Self::Ascii];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nerd => "nerd",
            Self::Ascii => "ascii",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Nerd => "Nerd Font",
            Self::Ascii => "ASCII",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Nerd => "Nerd Font glyphs and icons (default)",
            Self::Ascii => "Pure ASCII characters for terminals without Nerd Fonts",
        }
    }

    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "nerd" | "nerdfont" | "nerd-font" | "nerd_font" => Some(Self::Nerd),
            "ascii" => Some(Self::Ascii),
            _ => None,
        }
    }

    pub fn save(self, path: &Path) -> Result<()> {
        super::update_settings(path, |mut settings| {
            settings.tui.icons = self;
            Ok((
                "tui",
                serde_yaml::to_value(settings.tui).context("Failed to serialize tui settings")?,
            ))
        })
    }
}

impl Serialize for IconMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for IconMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown icon mode '{raw}'; expected 'nerd' or 'ascii'"
            ))
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Default,
    TokyoNight,
    Catppuccin,
    Dracula,
    Nord,
    Gruvbox,
    SolarizedLight,
}

impl Theme {
    pub const ALL: [Self; 7] = [
        Self::Default,
        Self::TokyoNight,
        Self::Catppuccin,
        Self::Dracula,
        Self::Nord,
        Self::Gruvbox,
        Self::SolarizedLight,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::TokyoNight => "tokyonight",
            Self::Catppuccin => "catppuccin",
            Self::Dracula => "dracula",
            Self::Nord => "nord",
            Self::Gruvbox => "gruvbox",
            Self::SolarizedLight => "solarized-light",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::TokyoNight => "Tokyo Night",
            Self::Catppuccin => "Catppuccin Mocha",
            Self::Dracula => "Dracula",
            Self::Nord => "Nord",
            Self::Gruvbox => "Gruvbox Dark",
            Self::SolarizedLight => "Solarized Light",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Default => "Original pp dark palette",
            Self::TokyoNight => "Clean dark blue inspired by Tokyo city lights",
            Self::Catppuccin => "Soothing pastel dark theme",
            Self::Dracula => "Classic high-contrast purple dark theme",
            Self::Nord => "Arctic, north-bluish clean dark palette",
            Self::Gruvbox => "Retro groove warm dark palette",
            Self::SolarizedLight => "Precision warm daylight palette",
        }
    }

    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "default" => Some(Self::Default),
            "tokyonight" | "tokyo-night" | "tokyo_night" => Some(Self::TokyoNight),
            "catppuccin" | "catppuccin-mocha" | "catppuccin_mocha" | "mocha" => {
                Some(Self::Catppuccin)
            }
            "dracula" => Some(Self::Dracula),
            "nord" => Some(Self::Nord),
            "gruvbox" | "gruvbox-dark" | "gruvbox_dark" => Some(Self::Gruvbox),
            "solarized-light" | "solarized_light" | "solarized" | "light" => {
                Some(Self::SolarizedLight)
            }
            _ => None,
        }
    }

    pub fn save(self, path: &Path) -> Result<()> {
        super::update_settings(path, |mut settings| {
            settings.tui.theme = self;
            Ok((
                "tui",
                serde_yaml::to_value(settings.tui).context("Failed to serialize tui settings")?,
            ))
        })
    }
}

impl Serialize for Theme {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Theme {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown theme '{raw}'; expected one of: default, tokyonight, catppuccin, dracula, nord, gruvbox, solarized-light"
            ))
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiSettings {
    pub max_fps: u16,
    pub theme: Theme,
    #[serde(alias = "icon_mode", alias = "iconMode")]
    pub icons: IconMode,
}

impl Default for TuiSettings {
    fn default() -> Self {
        Self {
            max_fps: 60,
            theme: Theme::Default,
            icons: IconMode::Nerd,
        }
    }
}

impl TuiSettings {
    /// 复用配置的严格校验与原子更新，同时保存 TUI 主题与图标模式偏好。
    pub fn save_appearance(path: &Path, theme: Theme, icons: IconMode) -> Result<()> {
        super::update_settings(path, |mut settings| {
            settings.tui.theme = theme;
            settings.tui.icons = icons;
            Ok((
                "tui",
                serde_yaml::to_value(settings.tui).context("Failed to serialize tui settings")?,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::read_settings;

    #[test]
    fn tui_theme_and_icons_default_and_persist_without_models() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent/settings.yml");
        let initial = read_settings(&path).unwrap().tui;
        assert_eq!(initial.theme, Theme::Default);
        assert_eq!(initial.icons, IconMode::Nerd);
        assert_eq!(initial.max_fps, 60);
        assert!(!path.exists());

        TuiSettings::save_appearance(&path, Theme::TokyoNight, IconMode::Ascii).unwrap();
        let saved = read_settings(&path).unwrap().tui;
        assert_eq!(saved.theme, Theme::TokyoNight);
        assert_eq!(saved.icons, IconMode::Ascii);
        assert_eq!(saved.max_fps, 60);

        std::fs::write(
            &path,
            "# 保留说明\ndebug: true # 调试\ntui:\n  max_fps: 90\n  theme: catppuccin-mocha\n  icons: nerd-font\n",
        )
        .unwrap();
        let loaded = read_settings(&path).unwrap().tui;
        assert_eq!(loaded.max_fps, 90);
        assert_eq!(loaded.theme, Theme::Catppuccin);
        assert_eq!(loaded.icons, IconMode::Nerd);

        Theme::SolarizedLight.save(&path).unwrap();
        let updated = read_settings(&path).unwrap().tui;
        assert_eq!(updated.max_fps, 90);
        assert_eq!(updated.theme, Theme::SolarizedLight);
        assert_eq!(updated.icons, IconMode::Nerd);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 保留说明"));
        assert!(text.contains("debug: true # 调试"));
    }

    #[test]
    fn theme_and_icon_mode_aliases_and_rejections() {
        for (input, expected) in [
            ("default", Some(Theme::Default)),
            ("TokyoNight", Some(Theme::TokyoNight)),
            ("tokyo-night", Some(Theme::TokyoNight)),
            ("catppuccin", Some(Theme::Catppuccin)),
            ("catppuccin-mocha", Some(Theme::Catppuccin)),
            ("dracula", Some(Theme::Dracula)),
            ("nord", Some(Theme::Nord)),
            ("gruvbox", Some(Theme::Gruvbox)),
            ("gruvbox-dark", Some(Theme::Gruvbox)),
            ("solarized-light", Some(Theme::SolarizedLight)),
            ("solarized", Some(Theme::SolarizedLight)),
            ("unknown", None),
        ] {
            assert_eq!(Theme::parse(input), expected, "input: {input}");
        }
        for (input, expected) in [
            ("nerd", Some(IconMode::Nerd)),
            ("Nerd-Font", Some(IconMode::Nerd)),
            ("ascii", Some(IconMode::Ascii)),
            ("ASCII", Some(IconMode::Ascii)),
            ("emoji", None),
        ] {
            assert_eq!(IconMode::parse(input), expected, "input: {input}");
        }
    }
}
