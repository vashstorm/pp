//! Shared rules for the session list projection: header validation, record classification
//! and terminal status determination.
//!
//! `persistence::index` reuses these rules when streaming the derived projection per file,
//! so there is no second summary/status implementation.

use super::{Header, compact_summary, parse_timestamp};
use anyhow::{Result, ensure};
use serde::Deserialize;

/// Validate and return the session header from the first journal line (with its complete
/// trailing newline): kind must be session, the version must be supported, id must match the
/// file name, cwd must be absolute and the timestamp must parse.
pub(super) fn header(line: &[u8], id: &str) -> Result<Header> {
    ensure!(line.ends_with(b"\n"), "incomplete session header");
    let header: Header =
        serde_json::from_slice(line).map_err(|_| anyhow::anyhow!("invalid session header"))?;
    ensure!(
        header.kind == "session",
        "first record must be a session header"
    );
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
        #[serde(default, rename = "ppReplay")]
        replay: Option<SummaryReplay>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct SummaryReplay {
    #[serde(default)]
    order: Vec<SummaryStep>,
}
#[derive(Deserialize)]
#[serde(tag = "kind")]
enum SummaryStep {
    #[serde(rename = "finish")]
    Finish { text: Option<String> },
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

/// Classification of one complete journal record; a JSON parse failure makes it a bad
/// record, which the caller handles with the "before/after the first user" rule.
pub(super) enum Classified {
    /// The actual body of a `type=message`, `role=user` record; only these records enter the
    /// prompt index.
    User(String),
    /// The content parts of a `type=message`, `role=assistant` record; used only for the
    /// first preview.
    Assistant(Vec<SummaryContent>, bool),
    /// Remaining records (native state, events, tool results, ...) do not take part in the
    /// projection.
    Other(Option<bool>),
}

/// Classify one complete record line according to the listing summary rules.
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
        Some(SummaryMessage::Assistant { stop_reason, .. })
            if matches!(stop_reason.as_str(), "aborted" | "error") =>
        {
            Classified::Other(Some(false))
        }
        Some(SummaryMessage::Assistant {
            mut content,
            stop_reason,
            replay,
        }) => {
            if let Some(replay) = replay
                && let Some(text) = replay.order.into_iter().rev().find_map(|step| match step {
                    SummaryStep::Finish { text } => Some(text.unwrap_or_else(|| {
                        content
                            .iter()
                            .filter(|part| part.kind == "text")
                            .map(|part| part.text.as_str())
                            .collect()
                    })),
                    SummaryStep::Other => None,
                })
            {
                content = vec![SummaryContent {
                    kind: "text".into(),
                    text,
                }];
            }
            Classified::Assistant(content, stop_reason == "stop")
        }
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

/// Accumulator for the first user summary (80 characters) and the first assistant text
/// preview (200 characters), following the same rules as the legacy listing entry point.
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

    /// Whether the first user message has been seen; this is the boundary for the bad-record
    /// "whole file invalid / keep prefix" decision.
    pub(super) fn has_summary(&self) -> bool {
        self.summary.is_some()
    }

    /// A file without a user message returns None and is not listed.
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
            serde_json::json!({"type":"session","id":id,"timestamp":"2026-10-17T01:02:03Z","cwd":"/project"})
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

        let user = "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"  Fix\\ntests \"}}\n";
        let assistant = "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"hidden\"},{\"type\":\"text\",\"text\":\"Already\\nfixed\"}]}}\n";
        let other = "{\"type\":\"custom\",\"customType\":\"pp.native_append\",\"data\":{\"messages\":[]}}\n";
        assert!(classify(other.as_bytes()).is_ok());
        let mut head = Head::default();
        assert!(!head.has_summary());
        for line in [other, user, assistant] {
            head.feed(classify(line.as_bytes()).unwrap());
        }
        assert_eq!(
            head.finish().unwrap(),
            ("Fix tests".into(), "Already fixed".into())
        );
        // Non-string user content is treated as a bad record (consistent with the listing
        // rules).
        assert!(
            classify(
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[]}}\n"
                    .as_bytes()
            )
            .is_err()
        );
        assert!(Head::default().finish().is_none());
    }
    #[test]
    fn message_snapshot_summary_uses_final_text_without_fragment_spaces() {
        for (finish, expected) in [
            (serde_json::json!({"kind":"finish"}), "prefixsuffix"),
            (
                serde_json::json!({"kind":"finish","text":"final replacement"}),
                "final replacement",
            ),
        ] {
            let message = serde_json::json!({"type":"message","message":{"role":"assistant","stopReason":"stop",
                "content":[{"type":"text","text":"prefix"},{"type":"thinking","thinking":"thought"},{"type":"text","text":"suffix"}],
                "ppReplay":{"version":1,"attempt":"fixture","order":[finish]}}});
            let mut head = Head::default();
            head.feed(Classified::User("question".into()));
            head.feed(classify(message.to_string().as_bytes()).unwrap());
            assert_eq!(head.finish().unwrap().1, expected);
            assert_eq!(
                super::super::replay::completed_text(&message["message"]).as_deref(),
                Some(expected)
            );
        }
    }
}
