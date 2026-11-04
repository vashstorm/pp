use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThinkLevel {
    #[default]
    #[serde(alias = "default")]
    None,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkLevel {
    pub const ALL: [Self; 6] = [
        Self::None,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];
    pub const OPTIONS: &str = "none, low, medium, high, xhigh, max";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn label(self) -> &'static str {
        if self == Self::None {
            "no think"
        } else {
            self.as_str()
        }
    }

    pub fn effort(self) -> async_openai::types::responses::ReasoningEffort {
        use async_openai::types::responses::ReasoningEffort;
        match self {
            Self::None => ReasoningEffort::None,
            Self::Low => ReasoningEffort::Low,
            Self::Medium => ReasoningEffort::Medium,
            Self::High => ReasoningEffort::High,
            Self::Xhigh => ReasoningEffort::Xhigh,
            Self::Max => ReasoningEffort::Max,
        }
    }
}

impl std::str::FromStr for ThinkLevel {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "none" | "default" => Ok(Self::None),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => anyhow::bail!("thinking level must be one of: {}", Self::OPTIONS),
        }
    }
}
