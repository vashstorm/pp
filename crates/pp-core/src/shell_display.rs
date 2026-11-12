//! Shell 展示快照；不携带执行器或终端布局。

use serde::{Deserialize, Serialize};

pub const VERSION: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellPresentation {
    pub version: u8,
    pub revision: u64,
    pub running: bool,
    pub rows: Vec<ShellRow>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellRow {
    pub depth: u16,
    pub segments: Vec<ShellSegment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<u32>,
    #[serde(default)]
    pub state: ShellState,
    #[serde(default)]
    pub block: bool,
    #[serde(default)]
    pub meta: String,
    #[serde(default)]
    pub effects: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSegment {
    pub text: String,
    pub style: ShellStyle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<u32>,
    #[serde(default)]
    pub state: ShellState,
    /// 嵌入程序的语言标签，仅用于语法着色。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellState {
    #[default]
    Pending,
    Running,
    Ok,
    Fail,
    Skipped,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellStyle {
    #[default]
    Text,
    Command,
    Keyword,
    Variable,
    String,
    Number,
    Operator,
    Badge,
    Comment,
}
