use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::ToolStyle;

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
    OneDark,
    SolarizedDark,
    Monokai,
    RosePine,
    RosePineDawn,
    Kanagawa,
    Everforest,
    GruvboxLight,
    CatppuccinLatte,
    GitHubDark,
    GitHubLight,
    AyuDark,
    NightOwl,
}

impl Theme {
    pub const ALL: [Self; 20] = [
        Self::Default,
        Self::TokyoNight,
        Self::Catppuccin,
        Self::Dracula,
        Self::Nord,
        Self::Gruvbox,
        Self::SolarizedLight,
        Self::OneDark,
        Self::SolarizedDark,
        Self::Monokai,
        Self::RosePine,
        Self::RosePineDawn,
        Self::Kanagawa,
        Self::Everforest,
        Self::GruvboxLight,
        Self::CatppuccinLatte,
        Self::GitHubDark,
        Self::GitHubLight,
        Self::AyuDark,
        Self::NightOwl,
    ];

    pub const ALL_NAMES: &'static str = "default, tokyonight, catppuccin, dracula, nord, gruvbox, solarized-light, onedark, solarized-dark, monokai, rose-pine, rose-pine-dawn, kanagawa, everforest, gruvbox-light, catppuccin-latte, github-dark, github-light, ayu-dark, night-owl";

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::TokyoNight => "tokyonight",
            Self::Catppuccin => "catppuccin",
            Self::Dracula => "dracula",
            Self::Nord => "nord",
            Self::Gruvbox => "gruvbox",
            Self::SolarizedLight => "solarized-light",
            Self::OneDark => "onedark",
            Self::SolarizedDark => "solarized-dark",
            Self::Monokai => "monokai",
            Self::RosePine => "rose-pine",
            Self::RosePineDawn => "rose-pine-dawn",
            Self::Kanagawa => "kanagawa",
            Self::Everforest => "everforest",
            Self::GruvboxLight => "gruvbox-light",
            Self::CatppuccinLatte => "catppuccin-latte",
            Self::GitHubDark => "github-dark",
            Self::GitHubLight => "github-light",
            Self::AyuDark => "ayu-dark",
            Self::NightOwl => "night-owl",
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
            Self::OneDark => "One Dark",
            Self::SolarizedDark => "Solarized Dark",
            Self::Monokai => "Monokai",
            Self::RosePine => "Rose Pine",
            Self::RosePineDawn => "Rose Pine Dawn",
            Self::Kanagawa => "Kanagawa",
            Self::Everforest => "Everforest Dark",
            Self::GruvboxLight => "Gruvbox Light",
            Self::CatppuccinLatte => "Catppuccin Latte",
            Self::GitHubDark => "GitHub Dark",
            Self::GitHubLight => "GitHub Light",
            Self::AyuDark => "Ayu Dark",
            Self::NightOwl => "Night Owl",
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
            Self::OneDark => "Iconic balanced dark slate palette",
            Self::SolarizedDark => "Precision deep teal low-glare dark palette",
            Self::Monokai => "Vibrant warm charcoal high-contrast theme",
            Self::RosePine => "All natural pine, faux fur and Soho vibes",
            Self::RosePineDawn => "Warm muted rose daylight palette",
            Self::Kanagawa => "Deep ink blue inspired by ukiyo-e waves",
            Self::Everforest => "Comfortable soft green forest dark palette",
            Self::GruvboxLight => "Retro warm parchment daylight palette",
            Self::CatppuccinLatte => "Soothing pastel light theme",
            Self::GitHubDark => "Crisp neutral dark high-contrast palette",
            Self::GitHubLight => "Clean bright daylight editor palette",
            Self::AyuDark => "Deep obsidian dark with warm amber accents",
            Self::NightOwl => "Deep navy dark tuned for late-night coding",
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
            "onedark" | "one-dark" | "one_dark" => Some(Self::OneDark),
            "solarized-dark" | "solarized_dark" | "solarizeddark" => Some(Self::SolarizedDark),
            "monokai" | "monokai-pro" | "monokai_pro" => Some(Self::Monokai),
            "rose-pine" | "rose_pine" | "rosepine" | "rosé-pine" => Some(Self::RosePine),
            "rose-pine-dawn" | "rose_pine_dawn" | "rosepinedawn" => Some(Self::RosePineDawn),
            "kanagawa" | "kanagawa-wave" | "kanagawa_wave" => Some(Self::Kanagawa),
            "everforest" | "everforest-dark" | "everforest_dark" => Some(Self::Everforest),
            "gruvbox-light" | "gruvbox_light" | "gruvboxlight" => Some(Self::GruvboxLight),
            "catppuccin-latte" | "catppuccin_latte" | "latte" => Some(Self::CatppuccinLatte),
            "github-dark" | "github_dark" | "githubdark" => Some(Self::GitHubDark),
            "github-light" | "github_light" | "githublight" => Some(Self::GitHubLight),
            "ayu-dark" | "ayu_dark" | "ayudark" | "ayu" => Some(Self::AyuDark),
            "night-owl" | "night_owl" | "nightowl" => Some(Self::NightOwl),
            _ => None,
        }
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
                "unknown theme '{raw}'; expected one of: {}",
                Self::ALL_NAMES
            ))
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopNotifications {
    #[default]
    Background,
    Always,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiSettings {
    pub max_fps: u16,
    pub theme: Theme,
    #[serde(alias = "icon_mode", alias = "iconMode")]
    pub icons: IconMode,
    #[serde(alias = "toolStyle", alias = "tools")]
    pub tool_style: ToolStyle,
    pub desktop_notifications: DesktopNotifications,
}

impl Default for TuiSettings {
    fn default() -> Self {
        Self {
            max_fps: 60,
            theme: Theme::Default,
            icons: IconMode::Nerd,
            tool_style: ToolStyle::Classic,
            desktop_notifications: DesktopNotifications::Background,
        }
    }
}

impl TuiSettings {
    /// Reuses the configuration's strict validation and atomic update, saving the TUI theme, icon mode and tool card style preferences together.
    pub fn save_appearance(
        path: &Path,
        theme: Theme,
        icons: IconMode,
        tool_style: ToolStyle,
    ) -> Result<()> {
        super::update_settings(path, |mut settings| {
            settings.tui.theme = theme;
            settings.tui.icons = icons;
            settings.tui.tool_style = tool_style;
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
    fn desktop_notification_modes_default_and_survive_appearance_saves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.yml");
        for (field, expected) in [
            ("", DesktopNotifications::Background),
            (
                "  desktop_notifications: background\n",
                DesktopNotifications::Background,
            ),
            (
                "  desktop_notifications: always\n",
                DesktopNotifications::Always,
            ),
            ("  desktop_notifications: off\n", DesktopNotifications::Off),
        ] {
            std::fs::write(&path, format!("tui:\n  max_fps: 60\n{field}")).unwrap();
            assert_eq!(
                read_settings(&path).unwrap().tui.desktop_notifications,
                expected
            );
            TuiSettings::save_appearance(&path, Theme::Nord, IconMode::Ascii, ToolStyle::Rail)
                .unwrap();
            assert_eq!(
                read_settings(&path).unwrap().tui.desktop_notifications,
                expected
            );
        }
        std::fs::write(&path, "tui: {desktop_notifications: 'sometimes'}\n").unwrap();
        assert!(read_settings(&path).is_err());
    }

    #[test]
    fn tui_theme_and_icons_default_and_persist_without_models() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent/settings.yml");
        let initial = read_settings(&path).unwrap().tui;
        assert_eq!(initial.theme, Theme::Default);
        assert_eq!(initial.icons, IconMode::Nerd);
        assert_eq!(initial.tool_style, ToolStyle::Classic);
        assert_eq!(initial.max_fps, 60);
        assert!(!path.exists());

        TuiSettings::save_appearance(&path, Theme::TokyoNight, IconMode::Ascii, ToolStyle::Rail)
            .unwrap();
        let saved = read_settings(&path).unwrap().tui;
        assert_eq!(saved.theme, Theme::TokyoNight);
        assert_eq!(saved.icons, IconMode::Ascii);
        assert_eq!(saved.tool_style, ToolStyle::Rail);
        assert_eq!(saved.max_fps, 60);

        std::fs::write(
            &path,
            "# preserved note\ndebug: true # debug\ntui:\n  max_fps: 90\n  theme: catppuccin-mocha\n  icons: nerd-font\n  tool_style: badge\n",
        )
        .unwrap();
        let loaded = read_settings(&path).unwrap().tui;
        assert_eq!(loaded.max_fps, 90);
        assert_eq!(loaded.theme, Theme::Catppuccin);
        assert_eq!(loaded.icons, IconMode::Nerd);
        assert_eq!(loaded.tool_style, ToolStyle::Badge);

        TuiSettings::save_appearance(
            &path,
            Theme::SolarizedLight,
            IconMode::Nerd,
            ToolStyle::Ledger,
        )
        .unwrap();
        let updated = read_settings(&path).unwrap().tui;
        assert_eq!(updated.max_fps, 90);
        assert_eq!(updated.theme, Theme::SolarizedLight);
        assert_eq!(updated.icons, IconMode::Nerd);
        assert_eq!(updated.tool_style, ToolStyle::Ledger);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# preserved note"));
        assert!(text.contains("debug: true # debug"));
    }

    #[test]
    fn tool_style_aliases_persistence_and_rejections() {
        assert_eq!(ToolStyle::ALL.len(), 6);
        let listed: Vec<_> = ToolStyle::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(listed.join(", "), ToolStyle::ALL_NAMES);
        let unique_slugs: std::collections::HashSet<_> = listed.iter().copied().collect();
        assert_eq!(unique_slugs.len(), ToolStyle::ALL.len());
        let unique_labels: std::collections::HashSet<_> =
            ToolStyle::ALL.iter().map(|s| s.label()).collect();
        assert_eq!(unique_labels.len(), ToolStyle::ALL.len());
        for style in ToolStyle::ALL {
            assert_eq!(ToolStyle::parse(style.as_str()), Some(style));
            assert!(!style.label().is_empty());
            assert!(!style.description().is_empty());
        }
        for (input, expected) in [
            ("classic", Some(ToolStyle::Classic)),
            ("RAIL", Some(ToolStyle::Rail)),
            ("timeline", Some(ToolStyle::Rail)),
            ("slab", Some(ToolStyle::Slab)),
            ("badge", Some(ToolStyle::Badge)),
            ("pill", Some(ToolStyle::Badge)),
            ("hanging", Some(ToolStyle::Hanging)),
            ("ledger", Some(ToolStyle::Ledger)),
            ("unknown", None),
        ] {
            assert_eq!(ToolStyle::parse(input), expected, "input: {input}");
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.yml");
        std::fs::write(&path, "tui:\n  tool_style: bogus\n").unwrap();
        let err = read_settings(&path).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid") || format!("{err:#}").contains("settings"),
            "{err:#}"
        );
    }

    #[test]
    fn theme_and_icon_mode_aliases_and_rejections() {
        assert_eq!(Theme::ALL.len(), 20);
        let listed: Vec<_> = Theme::ALL.iter().map(|theme| theme.as_str()).collect();
        assert_eq!(listed.join(", "), Theme::ALL_NAMES);
        let unique_slugs: std::collections::HashSet<_> = listed.iter().copied().collect();
        assert_eq!(unique_slugs.len(), Theme::ALL.len());
        let unique_labels: std::collections::HashSet<_> =
            Theme::ALL.iter().map(|theme| theme.label()).collect();
        assert_eq!(unique_labels.len(), Theme::ALL.len());
        for theme in Theme::ALL {
            assert_eq!(Theme::parse(theme.as_str()), Some(theme));
            assert!(!theme.label().is_empty());
            assert!(!theme.description().is_empty());
        }

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
            ("one-dark", Some(Theme::OneDark)),
            ("solarized_dark", Some(Theme::SolarizedDark)),
            ("monokai-pro", Some(Theme::Monokai)),
            ("rosepine", Some(Theme::RosePine)),
            ("rose_pine_dawn", Some(Theme::RosePineDawn)),
            ("kanagawa-wave", Some(Theme::Kanagawa)),
            ("everforest-dark", Some(Theme::Everforest)),
            ("gruvbox_light", Some(Theme::GruvboxLight)),
            ("latte", Some(Theme::CatppuccinLatte)),
            ("github_dark", Some(Theme::GitHubDark)),
            ("github-light", Some(Theme::GitHubLight)),
            ("ayu", Some(Theme::AyuDark)),
            ("night_owl", Some(Theme::NightOwl)),
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
