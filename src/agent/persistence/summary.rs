//! 会话列表投影的共享规则：header 校验、记录分类与末尾状态判定。
//!
//! `persistence::index` 在逐文件流式重建派生投影时复用这些规则，
//! 不维护第二套摘要/状态实现。

use super::{Header, compact_summary, parse_timestamp};
use anyhow::{Result, ensure};
use serde::Deserialize;

/// 校验并返回日志首行（完整换行结尾）的会话 header：kind 必须是 session、
/// 版本受支持、id 与文件名一致、cwd 绝对、时间可解析。
pub(super) fn header(line: &[u8], id: &str) -> Result<Header> {
    ensure!(line.ends_with(b"\n"), "incomplete session header");
    let header: Header =
        serde_json::from_slice(line).map_err(|_| anyhow::anyhow!("invalid session header"))?;
    ensure!(
        header.kind == "session",
        "first record must be a session header"
    );
    ensure!(header.version == 3, "unsupported session version");
    ensure!(header.id == id, "session ID does not match filename");
    ensure!(
        header.cwd.is_absolute(),
        "session working directory must be absolute"
    );
    parse_timestamp(&header.timestamp)?;
    Ok(header)
}

#[derive(Deserialize)]
struct SummaryEntry {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "customType")]
    custom_type: String,
    #[serde(default)]
    message: Option<SummaryMessage>,
}

#[derive(Deserialize)]
#[serde(tag = "role")]
enum SummaryMessage {
    #[serde(rename = "user")]
    User { content: String },
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(default)]
        content: Vec<SummaryContent>,
        #[serde(default, rename = "stopReason")]
        stop_reason: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
pub(super) struct SummaryContent {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

/// 单条完整日志记录的分类结果；JSON 解析失败即为坏记录，由调用方按
/// “首条 user 之前/之后”的规则处理。
pub(super) enum Classified {
    /// `type=message`、`role=user` 的实际正文；只有这类记录进入 prompt 索引。
    User(String),
    /// `type=message`、`role=assistant` 的 content 部分，仅用于首条预览。
    Assistant(Vec<SummaryContent>, bool),
    /// 其余记录（native state、事件、工具结果等）不参与投影。
    Other(Option<bool>),
}

/// 按列表摘要规则分类一条完整记录行。
pub(super) fn classify(line: &[u8]) -> Result<Classified> {
    let entry: SummaryEntry =
        serde_json::from_slice(line).map_err(|_| anyhow::anyhow!("invalid session record"))?;
    if matches!(entry.custom_type.as_str(), "pp.error" | "pp.cancelled") {
        return Ok(Classified::Other(Some(false)));
    }
    if entry.kind != "message" {
        return Ok(Classified::Other(None));
    }
    Ok(match entry.message {
        Some(SummaryMessage::User { content }) => Classified::User(content),
        Some(SummaryMessage::Assistant {
            content,
            stop_reason,
        }) => Classified::Assistant(content, stop_reason == "stop"),
        _ => Classified::Other(None),
    })
}

impl Classified {
    pub(super) fn status(&self) -> Option<bool> {
        match self {
            Self::User(_) => Some(false),
            Self::Assistant(_, done) => Some(*done),
            Self::Other(status) => *status,
        }
    }
}

/// 首条 user 摘要（80 字符）与首条 assistant 文本预览（200 字符）的累加器，
/// 规则与旧列表入口一致。
#[derive(Default)]
pub(super) struct Head {
    summary: Option<String>,
    preview: String,
}

impl Head {
    pub(super) fn feed(&mut self, record: Classified) {
        match record {
            Classified::User(content) => {
                self.summary
                    .get_or_insert_with(|| compact_summary(&content, 80));
            }
            Classified::Assistant(content, _) if self.preview.is_empty() => {
                self.preview = compact_summary(
                    &content
                        .iter()
                        .filter(|part| part.kind == "text")
                        .map(|part| part.text.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                    200,
                );
            }
            _ => {}
        }
    }

    /// 是否已看到首条 user 消息；这是坏记录“整文件无效/保留前缀”的分界。
    pub(super) fn has_summary(&self) -> bool {
        self.summary.is_some()
    }

    /// 没有 user 消息的文件返回 None，不列出。
    pub(super) fn finish(self) -> Option<(String, String)> {
        self.summary.map(|summary| (summary, self.preview))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_and_classify_follow_listing_rules() {
        let id = format!("2026-10-17T01-02-03-{}", "a".repeat(32));
        let valid = format!(
            "{}\n",
            serde_json::json!({"type":"session","version":3,"id":id,"timestamp":"2026-10-17T01:02:03Z","cwd":"/project"})
        );
        assert!(header(valid.as_bytes(), &id).is_ok());
        for broken in [
            "{\"type\":\"session\",\"version\":3,\"id\":\"mismatch\"}\n",
            "{\"type\":\"message\"}\n",
            "{\"type\":\"session\",\"version\":2}\n",
            "broken\n",
            "",
        ] {
            assert!(header(broken.as_bytes(), &id).is_err(), "{broken}");
        }

        let user = "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"  修复\\n测试 \"}}\n";
        let assistant = "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"隐藏\"},{\"type\":\"text\",\"text\":\"已经\\n修复\"}]}}\n";
        let other = "{\"type\":\"custom\",\"customType\":\"pp.native_append\",\"data\":{\"messages\":[]}}\n";
        assert!(classify(other.as_bytes()).is_ok());
        let mut head = Head::default();
        assert!(!head.has_summary());
        for line in [other, user, assistant] {
            head.feed(classify(line.as_bytes()).unwrap());
        }
        assert_eq!(
            head.finish().unwrap(),
            ("修复 测试".into(), "已经 修复".into())
        );
        // user content 不是字符串时按坏记录处理（与列表规则一致）。
        assert!(
            classify(
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[]}}\n"
                    .as_bytes()
            )
            .is_err()
        );
        assert!(Head::default().finish().is_none());
    }
}
