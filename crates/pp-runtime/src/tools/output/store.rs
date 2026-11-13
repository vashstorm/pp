//! 会话 `artifact://` 资源：绑定、分配、快照与删除。
use anyhow::{Context, Result, ensure};
use pp_core::cancellation::CancellationToken;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use super::lock;
use super::window::Window;

#[derive(Default, Clone)]
pub(crate) struct Store(Arc<Mutex<StoreState>>);

#[derive(Default)]
struct StoreState {
    directory: Option<PathBuf>,
    next: u64,
    live: BTreeMap<u64, Arc<Mutex<Capture>>>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Metadata {
    version: u64,
    total_bytes: u64,
    total_lines: u64,
    elided_bytes: u64,
    complete: bool,
}

pub(super) struct Capture {
    id: u64,
    path: PathBuf,
    file: File,
    window: Option<Window>,
    metadata: Metadata,
    error: bool,
    sealed: bool,
}

pub(crate) struct Snapshot {
    pub path: PathBuf,
    pub metadata: Value,
    _temporary: Option<tempfile::NamedTempFile>,
}

fn regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)
        .context("opening artifact")?;
    ensure!(
        file.metadata()?.is_file(),
        "artifact must be a regular file"
    );
    Ok(file)
}

fn directory(path: &Path) -> Result<()> {
    // 检查所有现有祖先，避免资源目录或祖先被替换为链接。
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "invalid artifact directory"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    Ok(())
}

fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut temporary =
        tempfile::NamedTempFile::new_in(path.parent().context("artifact directory missing")?)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_data()?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)?;
    Ok(())
}

impl Store {
    pub fn bind(&self, sessions: &Path, id: &str) -> Result<()> {
        let path = sessions.canonicalize()?.join(id);
        let mut state = lock(&self.0);
        if state.directory.as_ref() == Some(&path) {
            return Ok(());
        }
        ensure!(state.directory.is_none(), "artifact store already bound");
        let mut next = 0;
        match fs::read_dir(&path) {
            Ok(entries) => {
                directory(&path)?;
                for entry in entries {
                    let entry = entry?;
                    if let Some(id) = entry
                        .file_name()
                        .to_str()
                        .and_then(|v| v.split('.').next())
                        .and_then(|v| v.parse::<u64>().ok())
                    {
                        next = next.max(id.checked_add(1).context("artifact IDs exhausted")?);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        state.directory = Some(path);
        state.next = next;
        Ok(())
    }

    pub(super) fn allocate(&self, max: usize) -> Result<Arc<Mutex<Capture>>> {
        let mut state = lock(&self.0);
        let dir = state
            .directory
            .as_ref()
            .context("artifact store is not bound")?
            .clone();
        directory(&dir)?;
        let id = state.next;
        state.next = id.checked_add(1).context("artifact IDs exhausted")?;
        let path = dir.join(format!("{id}.bash.log"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&path)?;
        let capture = Arc::new(Mutex::new(Capture {
            id,
            path,
            file,
            window: (max != 0).then(|| Window::new(max, 3 * 1024 * 1024)),
            metadata: Metadata::default(),
            error: false,
            sealed: false,
        }));
        state.live.insert(id, capture.clone());
        Ok(capture)
    }

    pub fn parse_id(uri: &str) -> Result<u64> {
        let id = uri
            .strip_prefix("artifact://")
            .context("invalid artifact URI")?;
        ensure!(
            !id.is_empty() && id.bytes().all(|v| v.is_ascii_digit()),
            "artifact URI requires a numeric ID without a path or query"
        );
        id.parse().context("invalid artifact ID")
    }

    pub fn snapshot(&self, uri: &str, token: &CancellationToken) -> Result<Snapshot> {
        pp_core::cancellation::check(token)?;
        let id = Self::parse_id(uri)?;
        let (dir, live) = {
            let state = lock(&self.0);
            (
                state
                    .directory
                    .clone()
                    .context("no session artifacts available")?,
                state.live.get(&id).cloned(),
            )
        };
        ensure!(
            fs::symlink_metadata(&dir)?.is_dir(),
            "artifact directory unavailable"
        );
        for ancestor in dir.ancestors() {
            ensure!(
                !fs::symlink_metadata(ancestor)?.file_type().is_symlink(),
                "invalid artifact directory"
            );
        }
        if let Some(live) = live {
            let (window, mut metadata, file, error) = {
                let capture = lock(&live);
                if capture.sealed {
                    drop(capture);
                    return self.snapshot(uri, token);
                }
                (
                    capture.window.clone(),
                    capture.metadata.clone(),
                    if capture.window.is_none() {
                        Some(regular(&capture.path)?)
                    } else {
                        None
                    },
                    capture.error,
                )
            };
            let mut temporary = tempfile::NamedTempFile::new()?;
            if let Some(window) = window {
                let (text, omitted) = window.render(true);
                metadata.elided_bytes = omitted;
                temporary.write_all(text.as_bytes())?;
            } else {
                let mut reader = file
                    .context("artifact snapshot file unavailable")?
                    .take(metadata.total_bytes);
                copy(&mut reader, &mut temporary, token)?;
            }
            let path = temporary.path().canonicalize()?;
            return Ok(Snapshot {
                path,
                metadata: json!({"uri":uri,"capturing":true,"snapshot_version":metadata.version,"complete":false,"elided_bytes":metadata.elided_bytes,"capture_error":error}),
                _temporary: Some(temporary),
            });
        }
        let path = dir.join(format!("{id}.bash.log"));
        drop(regular(&path).context("artifact not found or unavailable")?);
        let metadata_path = dir.join(format!("{id}.meta.json"));
        let (metadata, interrupted): (Metadata, bool) = match regular(&metadata_path) {
            Ok(file) => (
                serde_json::from_reader(file.take(4096)).context("invalid artifact metadata")?,
                false,
            ),
            Err(_) => (Metadata::default(), true),
        };
        Ok(Snapshot {
            path,
            metadata: json!({"uri":uri,"capturing":false,"snapshot_version":metadata.version,"complete":metadata.complete,"elided_bytes":metadata.elided_bytes,"interrupted":interrupted}),
            _temporary: None,
        })
    }

    pub fn remove(sessions: &Path, id: &str) -> Result<()> {
        let path = sessions.canonicalize()?.join(id);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "invalid artifact directory"
            ),
        }
        directory(&path)?;
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            ensure!(entry.file_type()?.is_file(), "invalid artifact entry");
            fs::remove_file(entry.path())?;
        }
        fs::remove_dir(path)?;
        Ok(())
    }

    /// 完成收尾：摘除 live 句柄并冻结窗口。保持既有的「先持 capture 锁、
    /// 再取 state 锁」顺序，与 `snapshot` 的读取边界一致。
    pub(super) fn release(&self, capture: &mut Capture) {
        lock(&self.0).live.remove(&capture.id);
        capture.sealed = true;
        capture.window = None;
    }

    #[cfg(test)]
    pub(super) fn live_is_empty_for_test(&self) -> bool {
        lock(&self.0).live.is_empty()
    }
}

fn copy(reader: &mut impl Read, writer: &mut impl Write, token: &CancellationToken) -> Result<()> {
    let mut buffer = [0; 8192];
    loop {
        pp_core::cancellation::check(token)?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        writer.write_all(&buffer[..count])?;
    }
}

impl Capture {
    pub(super) fn append(&mut self, bytes: &[u8]) {
        if self.error {
            return;
        }
        let retained = self.metadata.total_bytes;
        let keep = self.window.as_ref().map_or(bytes.len(), |w| {
            bytes
                .len()
                .min((w.head_limit + w.tail_limit).saturating_sub(retained as usize))
        });
        if self.file.write_all(&bytes[..keep]).is_err() {
            self.error = true;
        }
        if let Some(window) = &mut self.window {
            window.append(bytes);
            self.metadata.elided_bytes = window
                .total
                .saturating_sub((window.head.len() + window.tail.len()) as u64);
        }
        self.metadata.total_bytes = self.metadata.total_bytes.saturating_add(bytes.len() as u64);
        self.metadata.version = self.metadata.version.saturating_add(1);
    }

    pub(super) fn seal(&mut self) -> Result<()> {
        ensure!(!self.error, "artifact capture failed");
        let dir = self.path.parent().context("artifact directory missing")?;
        if let Some(window) = &self.window
            && self.metadata.elided_bytes > 0
        {
            let (content, omitted) = window.render(true);
            self.metadata.elided_bytes = omitted;
            // 超出保存容量时在原文件写回头尾窗口；完整落盘后才发布完成元数据。
            self.file.seek(SeekFrom::Start(0))?;
            self.file.write_all(content.as_bytes())?;
            self.file.set_len(content.len() as u64)?;
        }
        self.file.sync_data()?;
        self.metadata.complete = self.metadata.elided_bytes == 0;
        publish(
            &dir.join(format!("{}.meta.json", self.id)),
            &serde_json::to_vec(&self.metadata)?,
        )?;
        Ok(())
    }

    pub(super) fn set_total_lines(&mut self, lines: u64) {
        self.metadata.total_lines = lines;
    }

    pub(super) fn mark_failed(&mut self) {
        self.error = true;
    }

    pub(super) fn id(&self) -> u64 {
        self.id
    }

    pub(super) fn failed(&self) -> bool {
        self.error
    }

    pub(super) fn version(&self) -> u64 {
        self.metadata.version
    }

    pub(super) fn complete(&self) -> bool {
        self.metadata.complete
    }

    /// 已省略字节数：活动捕获以窗口的当前渲染为准。
    pub(super) fn elided_bytes(&self) -> u64 {
        self.window
            .as_ref()
            .map_or(self.metadata.elided_bytes, |window| window.render(true).1)
    }

    #[cfg(test)]
    pub(super) fn window_is_none_for_test(&self) -> bool {
        self.window.is_none()
    }

    #[cfg(test)]
    pub(super) fn reopen_read_only_for_test(&mut self) -> std::io::Result<()> {
        self.file = File::open(&self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{fixture, read};
    use super::*;
    use crate::tools::output::OutputSink;
    use pp_config::config::OutputSettings;
    use pp_core::events::OutputStream;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn artifact_cap_live_snapshots_and_resume_continue_numbering() {
        let settings = OutputSettings {
            artifact_spill_threshold: 8,
            artifact_head_bytes: 4,
            output_max_columns: 0,
            artifact_max_bytes: 128,
        };
        let (dir, store, mut sink) = fixture(settings);
        sink.append(OutputStream::Stdout, b"0123456789abcdef-INITIAL");
        let uri = sink.value()["artifact"]["uri"].as_str().unwrap().to_owned();
        let before = store.snapshot(&uri, &CancellationToken::new()).unwrap();
        sink.append(
            OutputStream::Stderr,
            &format!("{}FINAL", "middle-".repeat(40)).into_bytes(),
        );
        assert_eq!(read(before), "0123456789abcdef-INITIAL");
        let notice = sink.value()["notice"].as_str().unwrap().to_owned();
        assert!(notice.contains("snapshot"));
        assert!(notice.contains("head/tail sample"));
        assert!(notice.contains("cannot be recovered"));
        assert!(!notice.contains("Full saved output"));
        let active = store.snapshot(&uri, &CancellationToken::new()).unwrap();
        assert_eq!(active.metadata["capturing"], true);
        let active = read(active);
        assert!(active.starts_with("0123456789abcdef"));
        assert!(active.ends_with("FINAL"));
        assert!(active.contains("ARTIFACT TRUNCATED"));
        let log = dir.path().join("session/0.bash.log");
        let inode = fs::metadata(&log).unwrap().ino();
        sink.finish();
        assert_eq!(fs::metadata(&log).unwrap().ino(), inode);
        assert_eq!(sink.value()["artifact"]["uri"], uri);
        assert_eq!(sink.value()["artifact"]["complete"], false);
        let notice = sink.value()["notice"].as_str().unwrap().to_owned();
        assert!(notice.contains("incomplete"));
        assert!(notice.contains("head/tail sample"));
        assert!(!notice.contains("still growing"));
        assert!(
            fs::metadata(dir.path().join("session/0.bash.log"))
                .unwrap()
                .len()
                <= 128
        );
        let restored = Store::default();
        restored.bind(dir.path(), "session").unwrap();
        assert_eq!(
            read(restored.snapshot(&uri, &CancellationToken::new()).unwrap()),
            active
        );
        let mut next = OutputSink::new(settings, restored.clone());
        next.append(OutputStream::Stdout, b"next-next-next");
        assert_eq!(next.value()["artifact"]["uri"], "artifact://1");
        drop(next);
        drop(restored);
        let resumed = Store::default();
        resumed.bind(dir.path(), "session").unwrap();
        let interrupted = resumed
            .snapshot("artifact://1", &CancellationToken::new())
            .unwrap();
        assert_eq!(interrupted.metadata["interrupted"], true);
        assert_eq!(interrupted.metadata["complete"], false);
        assert_eq!(read(interrupted), "next-next-next");
    }

    #[test]
    fn artifact_invalid_uri_symlink_and_capture_failure_are_explicit() {
        let (dir, store, mut sink) = fixture(OutputSettings {
            artifact_spill_threshold: 8,
            ..Default::default()
        });
        for uri in [
            "artifact://",
            "artifact://../0",
            "artifact://0/file",
            "artifact://0?x",
            "artifact://-1",
            "artifact://18446744073709551616",
        ] {
            assert!(store.snapshot(uri, &CancellationToken::new()).is_err());
        }
        let root = dir.path().join("session");
        fs::write(&root, "blocked").unwrap();
        sink.append(OutputStream::Stdout, b"large-output");
        sink.finish();
        assert!(sink.value()["capture_error"].is_string());
        assert!(sink.value().get("artifact").is_none());
        let notice = sink.value()["notice"].as_str().unwrap().to_owned();
        assert!(notice.contains("unavailable"));
        assert!(!notice.contains("read("));
        assert!(!notice.contains("artifact://"));
        fs::remove_file(&root).unwrap();
        fs::create_dir(&root).unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, "secret").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("0.bash.log")).unwrap();
        assert!(
            store
                .snapshot("artifact://0", &CancellationToken::new())
                .is_err()
        );
        assert!(Store::remove(dir.path(), "session").is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "secret");
    }

    #[test]
    fn artifact_concurrent_shared_ids_are_unique() {
        let (dir, store, _) = fixture(Default::default());
        std::thread::scope(|scope| {
            for i in 0..12 {
                let store = store.clone();
                scope.spawn(move || {
                    let mut sink = OutputSink::new(
                        OutputSettings {
                            artifact_spill_threshold: 1,
                            ..Default::default()
                        },
                        store,
                    );
                    sink.append(OutputStream::Stdout, format!("child-{i}").as_bytes());
                    sink.finish();
                    assert!(sink.value()["artifact"]["uri"].is_string());
                });
            }
        });
        let logs = fs::read_dir(dir.path().join("session"))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|e| e == "log")
            })
            .count();
        assert_eq!(logs, 12);
        Store::remove(dir.path(), "session").unwrap();
        assert!(!dir.path().join("session").exists());
    }
}
