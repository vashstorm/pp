//! 列表只读取最初的消息摘要和末尾状态，不重建会话树。
use super::{Header, SessionSummary, compact_summary, parse_timestamp};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub(super) fn read(path: &Path, id: &str) -> Result<Option<SessionSummary>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32 | rustix::fs::OFlags::NONBLOCK.bits() as i32,
        )
        .open(path)
        .context("opening session")?;
    let metadata = file.metadata().context("reading session metadata")?;
    ensure!(metadata.is_file(), "session is not a regular file");
    let updated_at = metadata
        .modified()
        .context("reading session modification time")?;
    let mut reader = BufReader::new(&file);
    let mut line = Vec::new();
    reader
        .read_until(b'\n', &mut line)
        .context("reading session header")?;
    ensure!(line.ends_with(b"\n"), "incomplete session header");
    let header: Header =
        serde_json::from_slice(&line).map_err(|_| anyhow::anyhow!("invalid session header"))?;
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
    let Some((summary, preview)) = messages(&mut reader)? else {
        return Ok(None);
    };
    let done = done(&mut reader, metadata.len())?;
    Ok(Some(SessionSummary {
        id: id.to_owned(),
        cwd: header.cwd,
        updated_at,
        summary,
        preview,
        bytes: metadata.len(),
        done,
    }))
}

#[derive(Deserialize)]
struct SummaryEntry {
    #[serde(rename = "type")]
    kind: String,
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
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct SummaryContent {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

fn messages(reader: &mut impl BufRead) -> Result<Option<(String, String)>> {
    let mut line = Vec::new();
    let mut summary = None;
    let mut preview = String::new();
    loop {
        line.clear();
        reader
            .read_until(b'\n', &mut line)
            .context("reading session record")?;
        if !line.ends_with(b"\n") {
            break;
        }
        let entry: SummaryEntry = match serde_json::from_slice(&line) {
            Ok(entry) => entry,
            // 列表保留已有摘要；完整日志校验留给实际恢复。
            Err(_) if summary.is_some() => break,
            Err(_) => bail!("invalid session record before first user message"),
        };
        if entry.kind == "message" {
            match entry.message {
                Some(SummaryMessage::User { content }) => {
                    summary.get_or_insert_with(|| compact_summary(&content, 80));
                }
                Some(SummaryMessage::Assistant { content }) if preview.is_empty() => {
                    preview = compact_summary(
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
        if summary.is_some() && !preview.is_empty() {
            break;
        }
    }
    Ok(summary.map(|summary| (summary, preview)))
}

#[derive(Deserialize)]
struct StatusEntry {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "customType")]
    custom_type: String,
    #[serde(default)]
    message: Option<StatusMessage>,
}

#[derive(Deserialize)]
#[serde(tag = "role")]
enum StatusMessage {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(default, rename = "stopReason")]
        stop_reason: String,
    },
    #[serde(other)]
    Other,
}

fn status(line: &[u8]) -> Option<bool> {
    let Ok(entry) = serde_json::from_slice::<StatusEntry>(line) else {
        return Some(false);
    };
    if matches!(entry.custom_type.as_str(), "pp.error" | "pp.cancelled") {
        return Some(false);
    }
    if entry.kind == "message" {
        match entry.message {
            Some(StatusMessage::User) => return Some(false),
            Some(StatusMessage::Assistant { stop_reason }) => return Some(stop_reason == "stop"),
            _ => {}
        }
    }
    None
}

fn done(reader: &mut (impl Read + Seek), mut end: u64) -> Result<bool> {
    let mut buffer = [0; 8192];
    let mut line = Vec::new();
    let mut first = true;
    while end > 0 {
        let count = end.min(buffer.len() as u64) as usize;
        end -= count as u64;
        reader
            .seek(SeekFrom::Start(end))
            .context("seeking session tail")?;
        reader
            .read_exact(&mut buffer[..count])
            .context("reading session tail")?;
        let mut bytes = &buffer[..count];
        if first {
            // 未提交完整换行的尾部仍显示为中断。
            if bytes.last() != Some(&b'\n') {
                return Ok(false);
            }
            bytes = &bytes[..bytes.len() - 1];
            first = false;
        }
        for &byte in bytes.iter().rev() {
            if byte == b'\n' {
                line.reverse();
                if let Some(done) = status(&line) {
                    return Ok(done);
                }
                line.clear();
            } else {
                line.push(byte);
            }
        }
    }
    line.reverse();
    Ok(status(&line).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct MeasuredReader {
        inner: Cursor<Vec<u8>>,
        bytes: usize,
    }

    impl Read for MeasuredReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let count = self.inner.read(buffer)?;
            self.bytes += count;
            Ok(count)
        }
    }

    impl Seek for MeasuredReader {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(position)
        }
    }

    #[test]
    fn listing_reads_only_initial_messages_and_status_tail() {
        let head = concat!(
            "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"最初的问题\"}}\n",
            "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"隐藏思考\"}]}}\n",
            "{\"type\":\"message\",\"message\":{\"role\":\"toolResult\",\"content\":[]}}\n",
            "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"最初的回答\"}]}}\n",
        );
        let middle =
            "{\"type\":\"custom\",\"customType\":\"pp.event\",\"data\":{\"text\":\"流式正文\"}}\n"
                .repeat(20_000);
        let tail = format!(
            "{}\n{}\n",
            serde_json::json!({"type":"message", "message":{"role":"assistant", "content":[], "stopReason":"stop"}}),
            serde_json::json!({"type":"custom", "customType":"pp.native_append", "data":{"padding":"尾部记录".repeat(10_000)}}),
        );
        let data = format!("{head}{middle}{tail}").into_bytes();
        let length = data.len() as u64;
        let mut reader = BufReader::new(MeasuredReader {
            inner: Cursor::new(data),
            bytes: 0,
        });
        assert_eq!(
            messages(&mut reader).unwrap(),
            Some(("最初的问题".into(), "最初的回答".into()))
        );
        assert!(reader.get_ref().bytes <= 8192);
        assert!(done(&mut reader, length).unwrap());
        // 长尾记录可以跨多个读取块；中间的大量事件仍不应被读取。
        assert!(reader.get_ref().bytes <= head.len() + tail.len() + 16_384);
    }

    #[test]
    fn listing_tail_handles_status_changes_corruption_and_block_boundaries() {
        let completed =
            "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"stopReason\":\"stop\"}}\n";
        for (last, expected) in [
            ("", true),
            (
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"next\"}}\n",
                false,
            ),
            (
                "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"stopReason\":\"toolUse\"}}\n",
                false,
            ),
            (
                "{\"type\":\"custom\",\"customType\":\"pp.error\",\"data\":{}}\n",
                false,
            ),
            (
                "{\"type\":\"custom\",\"customType\":\"pp.cancelled\",\"data\":{}}\n",
                false,
            ),
            (
                "{\"type\":\"message\",\"message\":{\"role\":\"toolResult\"}}\n",
                true,
            ),
            ("broken record\n", false),
        ] {
            for padding in [0, 8191, 8192, 8193, 24_576] {
                let data = format!(
                    "{completed}{last}{}\n",
                    serde_json::json!({"type":"custom", "customType":"pp.event", "data":{"text":"x".repeat(padding)}})
                );
                let length = data.len() as u64;
                assert_eq!(
                    done(&mut Cursor::new(data), length).unwrap(),
                    expected,
                    "尾部 {last:?}, 填充 {padding}"
                );
            }
        }
        for suffix in ["{partial", "\n", "broken\n", "{\"type\":\"message\"}"] {
            let data = format!("{completed}{suffix}");
            let length = data.len() as u64;
            assert!(!done(&mut Cursor::new(data), length).unwrap());
        }
        assert!(!done(&mut Cursor::new(Vec::new()), 0).unwrap());
        let mut reader = Cursor::new(b"{\"type\":\"session\"}\n");
        let length = reader.get_ref().len() as u64;
        assert!(!done(&mut reader, length).unwrap());
    }
}
