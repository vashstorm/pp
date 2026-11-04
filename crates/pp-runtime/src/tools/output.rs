//! Bash 有界输出、会话资源及运行中快照。
use pp_config::config::OutputSettings;

use super::StreamText;
use anyhow::{Context, Result, ensure};
use pp_core::cancellation::CancellationToken;
use pp_core::events::OutputStream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|error| error.into_inner())
}

#[derive(Clone)]
struct Window {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    head_limit: usize,
    tail_limit: usize,
    total: u64,
}

impl Window {
    fn new(limit: usize, head: usize) -> Self {
        let head_limit = head.min(limit / 2);
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            head_limit,
            tail_limit: limit - head_limit,
            total: 0,
        }
    }

    fn append(&mut self, bytes: &[u8]) {
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

    fn render(&self, artifact: bool) -> (String, u64) {
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

struct Capture {
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

    fn allocate(&self, max: usize) -> Result<Arc<Mutex<Capture>>> {
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
    fn append(&mut self, bytes: &[u8]) {
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

    fn seal(&mut self) -> Result<()> {
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
}

#[derive(Clone, Copy, Default)]
enum EscapeState {
    #[default]
    Text,
    Escape,
    Csi,
    Intermediate,
    ControlString {
        bell: bool,
        escaped: bool,
    },
}

#[derive(Default)]
pub struct Normalizer {
    decoder: StreamText,
    escape: EscapeState,
    cr: bool,
}

impl Normalizer {
    pub fn feed(&mut self, bytes: &[u8], finish: bool) -> String {
        let decoded = if finish {
            self.decoder.finish()
        } else {
            self.decoder.append(bytes)
        };
        self.decoder.text.clear();
        let mut text = String::new();
        for ch in decoded.chars() {
            if let EscapeState::ControlString { bell, escaped } = self.escape {
                self.escape = if ch == '\u{9c}' || bell && ch == '\u{7}' || escaped && ch == '\\' {
                    EscapeState::Text
                } else {
                    EscapeState::ControlString {
                        bell,
                        escaped: ch == '\u{1b}',
                    }
                };
                continue;
            }
            if ch == '\u{1b}' {
                self.escape = EscapeState::Escape;
                continue;
            }
            match self.escape {
                EscapeState::Escape => {
                    self.escape = match ch {
                        '[' => EscapeState::Csi,
                        ']' => EscapeState::ControlString {
                            bell: true,
                            escaped: false,
                        },
                        'P' | 'X' | '^' | '_' => EscapeState::ControlString {
                            bell: false,
                            escaped: false,
                        },
                        ' '..='/' => EscapeState::Intermediate,
                        _ => EscapeState::Text,
                    };
                    continue;
                }
                EscapeState::Csi => {
                    if ('@'..='~').contains(&ch) {
                        self.escape = EscapeState::Text;
                    }
                    continue;
                }
                EscapeState::Intermediate => {
                    if ('0'..='~').contains(&ch) {
                        self.escape = EscapeState::Text;
                    }
                    continue;
                }
                EscapeState::ControlString { .. } => unreachable!("控制字符串已在前面处理"),
                EscapeState::Text => {}
            }
            let control = match ch {
                '\u{9b}' => Some(EscapeState::Csi),
                '\u{9d}' => Some(EscapeState::ControlString {
                    bell: true,
                    escaped: false,
                }),
                '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => Some(EscapeState::ControlString {
                    bell: false,
                    escaped: false,
                }),
                _ => None,
            };
            if let Some(control) = control {
                self.escape = control;
                continue;
            }
            if ch == '\n' && self.cr {
                self.cr = false;
                continue;
            }
            self.cr = ch == '\r';
            if self.cr {
                text.push('\n');
            } else if !ch.is_control() || matches!(ch, '\n' | '\t') {
                text.push(ch);
            }
        }
        text
    }
}

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
            capture.metadata.total_lines = self.lines();
            if capture.seal().is_err() {
                capture.error = true;
                self.capture_error = true;
            }
            lock(&self.store.0).live.remove(&capture.id);
            capture.sealed = true;
            capture.window = None;
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
            if !capture.error && !self.capture_error {
                let omitted = capture
                    .window
                    .as_ref()
                    .map_or(capture.metadata.elided_bytes, |window| {
                        window.render(true).1
                    });
                value["artifact"] = json!({"uri":format!("artifact://{}",capture.id),"capturing":!self.finished,"snapshot_version":capture.metadata.version,"complete":self.finished && capture.metadata.complete,"elided_bytes":omitted});
            }
            if capture.error {
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

/// 模型结果中的补读指引；保留结构化元数据，不把终端展示文案混入日志正文。
fn recovery_notice(value: &Value) -> Option<String> {
    if value["capture_error"].is_string() {
        return Some("Artifact capture failed; full saved output is unavailable. No artifact is available for recovery.".into());
    }
    let artifact = &value["artifact"];
    let uri = artifact["uri"].as_str()?;
    let capturing = artifact["capturing"] == true;
    let elided = artifact["elided_bytes"].as_u64().unwrap_or(0);
    let mut status = if capturing {
        "Saved output is still growing; each read/grep uses one snapshot.".to_owned()
    } else if artifact["complete"] == true {
        "Full saved output is available.".to_owned()
    } else {
        "Saved output is incomplete.".to_owned()
    };
    if elided > 0 {
        status.push_str(&format!(
            " The artifact is a head/tail sample with {elided} bytes omitted; missing middle bytes cannot be recovered from it."
        ));
    }
    let read = json!({"path":format!("{uri}:-80")});
    let grep = json!({"path":uri,"pattern":"REGEX"});
    Some(format!(
        "{status} Call the built-in read tool as read({read}) for the last 80 saved lines, or the built-in grep tool as grep({grep}) for targeted searches (replace REGEX with your pattern). artifact:// URIs are tool resources, not filesystem paths. Only the built-in grep tool supports searching them; NEVER pass them to bash or shell commands such as grep, rg, cat, sed, or awk. Use read line selectors or next_path to inspect other saved ranges."
    ))
}

/// `result.pipes` 的有界阶段摘要；只读元数据，不访问 registry、没有阶段正文。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipesStatus {
    pub(crate) run_id: String,
    pub uri: String,
    pub nonzero_stages: u64,
    pub incomplete: bool,
}

impl PipesStatus {
    /// CLI 追加行：URI、非零阶段计数与观测/保留不完整提示。
    pub fn notice(&self) -> String {
        let mut status = format!("[pipes: {}", self.uri);
        if self.nonzero_stages > 0 {
            status.push_str(&format!(
                " · Non-zero stage results: {}",
                self.nonzero_stages
            ));
        }
        if self.incomplete {
            status.push_str(" · stage output incomplete");
        }
        status.push(']');
        status
    }
}

/// 从 `pipes` 元数据提取有界摘要；没有真实管道时返回 `None`，老结果保持原样。
pub fn pipes_status(pipes: &Value) -> Option<PipesStatus> {
    if !pipes.is_object() {
        return None;
    }
    let run_id = pipes["run_id"].as_str()?;
    let pipelines = pipes["pipelines"].as_array().map_or(0, Vec::len);
    if pipelines == 0 && pipes["omitted_pipelines"].as_u64().unwrap_or(0) == 0 {
        return None;
    }
    Some(PipesStatus {
        run_id: run_id.to_owned(),
        uri: pipes["uri"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("pipe://{run_id}")),
        nonzero_stages: pipes["nonzero_stages"].as_u64().unwrap_or(0),
        incomplete: pipes["incomplete"] == true || pipes["capture_error"].is_string(),
    })
}

/// 仅从已保存元数据生成展示文案，不在 UI 访问文件。
pub fn notice(value: &Value) -> String {
    let mut notices = Vec::new();
    if value["truncated"] == true {
        notices.push(
            if value["artifact"]["uri"].is_string() {
                "[Output truncated; inspect the artifact for more.]"
            } else {
                "[Output truncated; full saved output is unavailable.]"
            }
            .to_owned(),
        );
    }
    if let Some(uri) = value["artifact"]["uri"].as_str() {
        let state = if value["artifact"]["capturing"] == true {
            "capturing; reads use a snapshot"
        } else if value["artifact"]["complete"] == true {
            "complete"
        } else {
            "head/tail sample"
        };
        notices.push(format!(
            "[{uri}: {state}; {} bytes omitted from artifact]",
            value["artifact"]["elided_bytes"].as_u64().unwrap_or(0)
        ));
    }
    if let Some(error) = value["capture_error"].as_str() {
        notices.push(format!("[{error}]"));
    }
    notices.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn fixture(settings: OutputSettings) -> (tempfile::TempDir, Store, OutputSink) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::default();
        store.bind(dir.path(), "session").unwrap();
        let sink = OutputSink::new(settings, store.clone());
        (dir, store, sink)
    }

    fn read(snapshot: Snapshot) -> String {
        fs::read_to_string(snapshot.path).unwrap()
    }

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
    fn artifact_normalization_preserves_split_utf8_and_uncapped_lines() {
        let (_dir, store, mut sink) = fixture(OutputSettings {
            output_max_columns: 4,
            ..Default::default()
        });
        let text = "\u{1b}(B\u{1b}[31m中😀\u{1b}[0m\r\nnext\rprogress\tend\u{1b}]title\u{7}\u{9b}32m\u{9b}0m\u{1b}Pignored\u{7}still-ignored\u{1b}\\\n";
        for chunk in text.as_bytes().chunks(1) {
            sink.append(OutputStream::Stdout, chunk);
        }
        sink.append(OutputStream::Stderr, b"error\n");
        sink.finish();
        let result = sink.value();
        assert_eq!(result["truncated"], true);
        let saved = read(
            store
                .snapshot(
                    result["artifact"]["uri"].as_str().unwrap(),
                    &CancellationToken::new(),
                )
                .unwrap(),
        );
        assert_eq!(saved, "中😀\nnext\nprogress\tend\nerror\n");
        assert_eq!(result["total_lines"], 4);
        assert!(!result["output"].as_str().unwrap().contains('�'));
    }

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
        assert!(lock(sink.capture.as_ref().unwrap()).window.is_none());
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
                capture.file = File::open(&capture.path).unwrap();
            } else {
                fs::create_dir(dir.path().join("session/0.meta.json")).unwrap();
            }
            sink.append(OutputStream::Stderr, b"more-output");
            sink.finish();
            assert!(sink.value()["capture_error"].is_string());
            assert!(sink.value().get("artifact").is_none());
            assert!(lock(&store.0).live.is_empty());
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

    #[test]
    fn pipes_status_is_bounded_and_absent_for_old_results() {
        assert!(pipes_status(&Value::Null).is_none());
        assert!(pipes_status(&json!({"ok":true,"stdout":"plain"})).is_none());
        // 没有真实管道的 run 不占用卡片底部入口。
        assert!(pipes_status(&json!({"run_id":"abc","uri":"pipe://abc"})).is_none());

        let status = pipes_status(&json!({
            "run_id":"abc",
            "uri":"pipe://abc",
            "pipelines":[{"id":1,"stages":[]}],
            "streams":[{"id":0,"uri":"pipe://abc/0"}],
            "nonzero_stages":2,
            "incomplete":true
        }))
        .unwrap();
        assert_eq!(status.uri, "pipe://abc");
        assert_eq!(status.nonzero_stages, 2);
        assert!(status.incomplete);
        let notice = status.notice();
        assert!(notice.contains("pipe://abc"), "{notice}");
        assert!(notice.contains("Non-zero stage results: 2"), "{notice}");
        assert!(notice.contains("stage output incomplete"), "{notice}");
        assert!(!notice.contains("body-secret"), "{notice}");

        let omitted = pipes_status(&json!({
            "run_id":"def",
            "pipelines":[],
            "omitted_pipelines":3
        }))
        .unwrap();
        assert_eq!(omitted.uri, "pipe://def");
        assert!(!omitted.notice().contains("Non-zero"));
    }
}
