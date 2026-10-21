use super::{AskArgs, AskOption, AskQuestion};
use anyhow::{Result, bail};
use serde::Deserialize;
use std::borrow::Cow;

// 在两种协议共用的工具边界归一化 provider 参数；内部结构与持久化格式保持一致。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    questions: Vec<Question>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Question {
    id: String,
    question: String,
    options: Vec<OptionInput>,
    header: Option<String>,
    multi: Option<bool>,
    recommended: Option<Index>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OptionInput {
    Label(String),
    Detailed(OptionDetails),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OptionDetails {
    label: String,
    description: Option<String>,
    preview: Option<String>,
    recommended: Option<Recommendation>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Index {
    Number(usize),
    Text(String),
}

impl Index {
    fn resolve(self) -> Result<usize> {
        match self {
            Self::Number(index) => Ok(index),
            Self::Text(text) => {
                if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) {
                    text.parse()
                        .map_err(|_| anyhow::anyhow!("invalid ask recommended index"))
                } else {
                    bail!("invalid ask recommended index")
                }
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Recommendation {
    Flag(bool),
    Index(Index),
}

pub(super) fn parse(arguments: &str) -> Result<AskArgs> {
    let invalid = || anyhow::anyhow!("invalid ask arguments");
    // 部分 provider 把 JSON 参数再次编码为字符串；最多解包一层，不修补截断 JSON。
    let arguments = if arguments.trim_start().starts_with('"') {
        Cow::Owned(serde_json::from_str::<String>(arguments).map_err(|_| invalid())?)
    } else {
        Cow::Borrowed(arguments)
    };
    // 直接按严格结构反序列化，避免 Value 中转吞掉重复字段。
    let input: Args = serde_json::from_str(&arguments).map_err(|_| invalid())?;
    let questions = input
        .questions
        .into_iter()
        .map(Question::normalize)
        .collect::<Result<_>>()?;
    Ok(AskArgs { questions })
}

impl Question {
    fn normalize(self) -> Result<AskQuestion> {
        let mut recommended = self.recommended.map(Index::resolve).transpose()?;
        let mut options = Vec::with_capacity(self.options.len());
        for (position, option) in self.options.into_iter().enumerate() {
            let (option, hint) = match option {
                OptionInput::Label(label) => (
                    AskOption {
                        label,
                        description: None,
                        preview: None,
                    },
                    None,
                ),
                OptionInput::Detailed(option) => {
                    let hint = match option.recommended {
                        Some(Recommendation::Flag(true)) => Some(position),
                        Some(Recommendation::Index(index)) => Some(index.resolve()?),
                        Some(Recommendation::Flag(false)) | None => None,
                    };
                    (
                        AskOption {
                            label: option.label,
                            description: option.description,
                            preview: option.preview,
                        },
                        hint,
                    )
                }
            };
            if let Some(hint) = hint {
                if recommended.is_some_and(|index| index != hint) {
                    bail!("conflicting ask recommended hints");
                }
                recommended = Some(hint);
            }
            options.push(option);
        }
        Ok(AskQuestion {
            id: self.id,
            question: self.question,
            options,
            header: self.header,
            multi: self.multi.unwrap_or(false),
            recommended,
        })
    }
}
