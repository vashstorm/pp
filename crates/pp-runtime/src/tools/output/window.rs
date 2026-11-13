//! 有界输出窗口：头部保留 + 尾部环形缓冲。
use std::collections::VecDeque;

#[derive(Clone)]
pub(super) struct Window {
    pub(super) head: Vec<u8>,
    pub(super) tail: VecDeque<u8>,
    pub(super) head_limit: usize,
    pub(super) tail_limit: usize,
    pub(super) total: u64,
}

impl Window {
    pub(super) fn new(limit: usize, head: usize) -> Self {
        let head_limit = head.min(limit / 2);
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            head_limit,
            tail_limit: limit - head_limit,
            total: 0,
        }
    }

    pub(super) fn append(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len() as u64);
        let take = bytes.len().min(self.head_limit - self.head.len());
        self.head.extend_from_slice(&bytes[..take]);
        let bytes = &bytes[take..];
        if bytes.len() >= self.tail_limit {
            self.tail.clear();
            self.tail.extend(&bytes[bytes.len() - self.tail_limit..]);
        } else {
            let drop = self
                .tail
                .len()
                .saturating_add(bytes.len())
                .saturating_sub(self.tail_limit);
            self.tail.drain(..drop);
            self.tail.extend(bytes);
        }
    }

    fn parts(&self) -> (String, String, u64) {
        let mut head = self.head.as_slice();
        let tail = self.tail.iter().copied().collect::<Vec<_>>();
        if self.total <= (self.head.len() + tail.len()) as u64 {
            let mut all = head.to_vec();
            all.extend_from_slice(&tail);
            return (String::from_utf8_lossy(&all).into_owned(), String::new(), 0);
        }
        while std::str::from_utf8(head).is_err() && !head.is_empty() {
            head = &head[..head.len() - 1];
        }
        let mut tail_slice = tail.as_slice();
        while std::str::from_utf8(tail_slice).is_err() && !tail_slice.is_empty() {
            tail_slice = &tail_slice[1..];
        }
        let omitted = self.total - (head.len() + tail_slice.len()) as u64;
        (
            String::from_utf8_lossy(head).into_owned(),
            String::from_utf8_lossy(tail_slice).into_owned(),
            omitted,
        )
    }

    pub(super) fn render(&self, artifact: bool) -> (String, u64) {
        let (mut head, mut tail, mut omitted) = self.parts();
        if omitted == 0 {
            return (head, 0);
        }
        let label = if artifact {
            "ARTIFACT TRUNCATED"
        } else {
            "OUTPUT TRUNCATED"
        };
        if artifact {
            let limit = self.head_limit + self.tail_limit;
            let reserve = format!("\n[{label}: {} bytes omitted]\n", self.total).len();
            if limit < reserve {
                let marker = "[TRUNCATED]";
                return (marker[..limit.min(marker.len())].to_owned(), self.total);
            }
            let budget = limit - reserve;
            let mut end = head.len().min(budget);
            while !head.is_char_boundary(end) {
                end -= 1;
            }
            head.truncate(end);
            let mut start = tail.len().saturating_sub(budget - head.len());
            while !tail.is_char_boundary(start) {
                start += 1;
            }
            tail.drain(..start);
            omitted = self.total - (head.len() + tail.len()) as u64;
        }
        (
            format!("{head}\n[{label}: {omitted} bytes omitted]\n{tail}"),
            omitted,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{fixture, read};
    use pp_config::config::OutputSettings;
    use pp_core::cancellation::CancellationToken;
    use pp_core::events::OutputStream;
    use std::fs;

    #[test]
    fn artifact_head_tail_boundaries_and_full_capture() {
        let settings = OutputSettings {
            artifact_spill_threshold: 12,
            artifact_head_bytes: 4,
            output_max_columns: 0,
            ..Default::default()
        };
        for count in [0, 11, 12, 13, 4097] {
            let (dir, store, mut sink) = fixture(settings);
            let text = if count == 0 {
                String::new()
            } else {
                format!("HEAD{}TAIL", "x".repeat(count - 8))
            };
            for chunk in text.as_bytes().chunks(3) {
                sink.append(OutputStream::Stdout, chunk);
            }
            if count <= 12 {
                assert!(!dir.path().join("session").exists());
            }
            sink.finish();
            let result = sink.value();
            assert_eq!(result["total_bytes"], count);
            if count <= 12 {
                assert_eq!(result["output"], text);
                assert!(result.get("artifact").is_none());
                assert!(!dir.path().join("session").exists());
            } else {
                let output = result["output"].as_str().unwrap();
                assert!(output.starts_with("HEAD"));
                assert!(output.ends_with("TAIL"));
                assert!(output.len() < 100);
                assert_eq!(result["artifact"]["complete"], true);
                assert!(
                    result["notice"]
                        .as_str()
                        .unwrap()
                        .contains("Full saved output")
                );
                assert_eq!(
                    read(
                        store
                            .snapshot(
                                result["artifact"]["uri"].as_str().unwrap(),
                                &CancellationToken::new()
                            )
                            .unwrap()
                    ),
                    text
                );
            }
        }
    }

    #[test]
    fn artifact_utf8_windows_and_missing_final_metadata_remain_honest() {
        let settings = OutputSettings {
            artifact_spill_threshold: 17,
            artifact_head_bytes: 6,
            output_max_columns: 0,
            artifact_max_bytes: 128,
        };
        let (dir, store, mut sink) = fixture(settings);
        let text = "中😀尾部".repeat(100);
        for chunk in text.as_bytes().chunks(7) {
            sink.append(OutputStream::Stdout, chunk);
        }
        sink.finish();
        assert!(!sink.preview().contains('�'));
        let uri = sink.value()["artifact"]["uri"].as_str().unwrap().to_owned();
        let saved = store.snapshot(&uri, &CancellationToken::new()).unwrap();
        assert!(!read(saved).contains('�'));
        fs::remove_file(dir.path().join("session/0.meta.json")).unwrap();
        let interrupted = store.snapshot(&uri, &CancellationToken::new()).unwrap();
        assert_eq!(interrupted.metadata["complete"], false);
        assert_eq!(interrupted.metadata["interrupted"], true);
    }
}
