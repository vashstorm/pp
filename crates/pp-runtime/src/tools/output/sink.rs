//! bash／工具输出的增量归一化、有界预览与资源落盘。
use pp_config::config::OutputSettings;
use pp_core::events::OutputStream;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

use super::lock;
use super::normalize::Normalizer;
use super::notice::recovery_notice;
use super::store::{Capture, Store};
use super::window::Window;

pub(crate) struct OutputSink {
    settings: OutputSettings,
    store: Store,
    inline: Window,
    pending: Vec<u8>,
    capture: Option<Arc<Mutex<Capture>>>,
    normalizers: [Normalizer; 2],
    columns: usize,
    column_dropped: bool,
    total_bytes: u64,
    newlines: u64,
    ends_newline: bool,
    capture_error: bool,
    finished: bool,
}

impl OutputSink {
    pub fn new(settings: OutputSettings, store: Store) -> Self {
        Self {
            settings,
            store,
            inline: Window::new(
                settings.artifact_spill_threshold,
                settings.artifact_head_bytes,
            ),
            pending: Vec::new(),
            capture: None,
            normalizers: Default::default(),
            columns: 0,
            column_dropped: false,
            total_bytes: 0,
            newlines: 0,
            ends_newline: false,
            capture_error: false,
            finished: false,
        }
    }

    pub fn append(&mut self, stream: OutputStream, bytes: &[u8]) {
        let index = match stream {
            OutputStream::Stdout => 0,
            OutputStream::Stderr => 1,
        };
        let text = self.normalizers[index].feed(bytes, false);
        self.text(&text);
    }

    fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.total_bytes = self.total_bytes.saturating_add(text.len() as u64);
        self.newlines = self
            .newlines
            .saturating_add(text.bytes().filter(|b| *b == b'\n').count() as u64);
        self.ends_newline = text.ends_with('\n');
        let mut inline = String::new();
        for ch in text.chars() {
            if ch == '\n' {
                inline.push(ch);
                self.columns = 0;
            } else if self.settings.output_max_columns == 0
                || self.columns.saturating_add(ch.len_utf8()) <= self.settings.output_max_columns
            {
                inline.push(ch);
                self.columns = self.columns.saturating_add(ch.len_utf8());
            } else {
                if self.columns <= self.settings.output_max_columns {
                    inline.push('…');
                }
                self.columns = self.settings.output_max_columns.saturating_add(1);
                self.column_dropped = true;
            }
        }
        self.inline.append(inline.as_bytes());
        // 未溢出时只保留内存缓冲；首次溢出才创建资源文件并补写此前正文。
        if self.capture.is_none()
            && !self.capture_error
            && (self.column_dropped
                || self.total_bytes > self.settings.artifact_spill_threshold as u64)
        {
            match self.store.allocate(self.settings.artifact_max_bytes) {
                Ok(capture) => {
                    lock(&capture).append(&self.pending);
                    self.pending.clear();
                    self.capture = Some(capture);
                }
                Err(_) => {
                    self.capture_error = true;
                    self.pending.clear();
                }
            }
        }
        if let Some(capture) = &self.capture {
            lock(capture).append(text.as_bytes());
        } else if !self.capture_error {
            self.pending.extend_from_slice(text.as_bytes());
        }
    }

    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        for i in 0..2 {
            let text = self.normalizers[i].feed(&[], true);
            self.text(&text);
        }
        if let Some(capture) = &self.capture {
            let mut capture = lock(capture);
            capture.set_total_lines(self.lines());
            if capture.seal().is_err() {
                capture.mark_failed();
                self.capture_error = true;
            }
            self.store.release(&mut capture);
        }
        self.finished = true;
        self.pending.clear();
    }

    fn lines(&self) -> u64 {
        self.newlines + u64::from(self.total_bytes > 0 && !self.ends_newline)
    }

    pub fn preview(&self) -> String {
        self.inline.render(false).0
    }

    pub fn value(&self) -> Value {
        let (output, omitted) = self.inline.render(false);
        let mut value = json!({"output":output,"truncated":omitted > 0 || self.column_dropped,"total_bytes":self.total_bytes,"total_lines":self.lines()});
        if let Some(capture) = &self.capture {
            let capture = lock(capture);
            if !capture.failed() && !self.capture_error {
                let omitted = capture.elided_bytes();
                value["artifact"] = json!({"uri":format!("artifact://{}",capture.id()),"capturing":!self.finished,"snapshot_version":capture.version(),"complete":self.finished && capture.complete(),"elided_bytes":omitted});
            }
            if capture.failed() {
                value["capture_error"] =
                    json!("Artifact capture failed; full output is unavailable.");
            }
        }
        if self.capture_error {
            value["capture_error"] = json!("Artifact capture failed; full output is unavailable.");
        }
        if let Some(notice) = recovery_notice(&value) {
            value["notice"] = notice.into();
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{fixture, read};
    use super::super::lock;
    use super::*;
    use pp_core::cancellation::CancellationToken;
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn artifact_log_is_created_only_on_first_overflow() {
        for max in [0, 128] {
            let (dir, store, mut sink) = fixture(OutputSettings {
                artifact_spill_threshold: 12,
                output_max_columns: 0,
                artifact_max_bytes: max,
                ..Default::default()
            });
            let root = dir.path().join("session");
            assert!(!root.exists());
            sink.append(OutputStream::Stdout, b"HEAD");
            assert!(!root.exists());
            sink.append(OutputStream::Stderr, b"1234TAIL");
            assert!(!root.exists());
            assert!(sink.value().get("artifact").is_none());
            assert!(sink.value().get("notice").is_none());

            sink.append(OutputStream::Stdout, b"!");
            assert_eq!(sink.value()["artifact"]["uri"], "artifact://0");
            assert!(
                sink.value()["notice"]
                    .as_str()
                    .unwrap()
                    .contains("snapshot")
            );
            let log = root.join("0.bash.log");
            let inode = fs::metadata(&log).unwrap().ino();
            assert_eq!(fs::read(&log).unwrap(), b"HEAD1234TAIL!");
            assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
            assert!(!root.join("0.meta.json").exists());
            sink.append(OutputStream::Stderr, b"END");
            assert_eq!(fs::read(&log).unwrap(), b"HEAD1234TAIL!END");
            sink.finish();
            assert_eq!(fs::metadata(&log).unwrap().ino(), inode);
            assert_eq!(fs::read(&log).unwrap(), b"HEAD1234TAIL!END");
            assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
            assert!(root.join("0.meta.json").is_file());
            assert_eq!(
                read(
                    store
                        .snapshot("artifact://0", &CancellationToken::new())
                        .unwrap()
                ),
                "HEAD1234TAIL!END"
            );
        }
    }

    #[test]
    fn artifact_unlimited_capture_has_bounded_memory_and_stable_snapshot() {
        let settings = OutputSettings {
            artifact_spill_threshold: 32,
            output_max_columns: 0,
            artifact_max_bytes: 0,
            ..Default::default()
        };
        let (_dir, store, mut sink) = fixture(settings);
        sink.append(OutputStream::Stdout, &vec![b'a'; 10000]);
        let before = store
            .snapshot("artifact://0", &CancellationToken::new())
            .unwrap();
        sink.append(OutputStream::Stdout, b"TAIL");
        assert_eq!(read(before).len(), 10000);
        assert!(lock(sink.capture.as_ref().unwrap()).window_is_none_for_test());
        assert!(sink.pending.is_empty());
        sink.finish();
        assert!(
            read(
                store
                    .snapshot("artifact://0", &CancellationToken::new())
                    .unwrap()
            )
            .ends_with("TAIL")
        );
    }

    #[test]
    fn artifact_write_and_publish_failures_do_not_advertise_complete_resources() {
        for failure in ["write", "seal", "publish"] {
            let (dir, store, mut sink) = fixture(OutputSettings {
                artifact_spill_threshold: 8,
                artifact_max_bytes: if failure == "seal" { 8 } else { 128 },
                ..Default::default()
            });
            sink.append(OutputStream::Stdout, b"initial-output");
            if matches!(failure, "write" | "seal") {
                let mut capture = lock(sink.capture.as_ref().unwrap());
                capture.reopen_read_only_for_test().unwrap();
            } else {
                fs::create_dir(dir.path().join("session/0.meta.json")).unwrap();
            }
            sink.append(OutputStream::Stderr, b"more-output");
            sink.finish();
            assert!(sink.value()["capture_error"].is_string());
            assert!(sink.value().get("artifact").is_none());
            assert!(store.live_is_empty_for_test());
        }
    }
}
