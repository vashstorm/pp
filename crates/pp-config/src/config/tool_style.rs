use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 工具消息框（read / bash / eval / search / edit / write / mcp / wait / ask / lsp）的版式风格。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolStyle {
    #[default]
    Classic,
    Rail,
    Slab,
    Badge,
    Hanging,
    Ledger,
}

impl ToolStyle {
    pub const ALL: [Self; 6] = [
        Self::Classic,
        Self::Rail,
        Self::Slab,
        Self::Badge,
        Self::Hanging,
        Self::Ledger,
    ];

    pub const ALL_NAMES: &'static str = "classic, rail, slab, badge, hanging, ledger";

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Rail => "rail",
            Self::Slab => "slab",
            Self::Badge => "badge",
            Self::Hanging => "hanging",
            Self::Ledger => "ledger",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::Rail => "Rail",
            Self::Slab => "Slab",
            Self::Badge => "Badge",
            Self::Hanging => "Hanging",
            Self::Ledger => "Ledger",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Classic => "Original bordered cards and compact read rows (default)",
            Self::Rail => "Timeline rail with status color and right-aligned status",
            Self::Slab => "Solid shaded slab with left accent bar and header band",
            Self::Badge => "Inverted tool pill badge with dashed section dividers",
            Self::Hanging => "Compact function-style header with hanging output",
            Self::Ledger => "Aligned tabular label column with ruled section header",
        }
    }

    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "classic" => Some(Self::Classic),
            "rail" | "timeline" => Some(Self::Rail),
            "slab" => Some(Self::Slab),
            "badge" | "pill" => Some(Self::Badge),
            "hanging" => Some(Self::Hanging),
            "ledger" => Some(Self::Ledger),
            _ => None,
        }
    }
}

impl Serialize for ToolStyle {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ToolStyle {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown tool style '{raw}'; expected one of: {}",
                Self::ALL_NAMES
            ))
        })
    }
}
