//! 用户级的持久全局授权与跨进程文件同步。

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

const MAX_BYTES: u64 = 1024 * 1024;
const PERMISSION_FILE: &str = "permission.toml";
const PERMISSION_LOCK: &str = "permission.lock";

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxGrant {
    pub command: String,
    #[serde(default)]
    pub writable: Vec<PathBuf>,
    #[serde(default)]
    pub network: bool,
}

impl SandboxGrant {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.writable.iter().all(|path| {
                path.is_absolute()
                    && !path
                        .components()
                        .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
            }),
            "invalid permission rule path"
        );
        ensure!(!self.command.trim().is_empty(), "empty permission command");
        ensure!(
            self.network || !self.writable.is_empty(),
            "empty permission scope"
        );
        ensure!(
            self.writable.windows(2).all(|pair| pair[0] < pair[1]),
            "invalid permission scope order"
        );
        Ok(())
    }

    pub fn summary(&self, cwd: Option<&Path>) -> String {
        let writes = self
            .writable
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let command = pp_core::logging::diagnostic_text(&self.command);
        let mut lines = Vec::new();
        if let Some(cwd) = cwd {
            lines.push(format!("Directory: {}", cwd.display()));
        }
        if !writes.is_empty() {
            lines.push(format!("Extra writable directories: {writes}"));
        }
        if self.network {
            lines.push("Allow outbound network for this command and its subprocesses".to_owned());
        }
        lines.push(format!("Command: {command}"));
        pp_core::text::display_text(&lines.join("\n")).into_owned()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Document {
    #[serde(default)]
    pub(crate) tools: Vec<String>,
    #[serde(default)]
    pub(crate) sandbox: Vec<SandboxGrant>,
}

impl Document {
    fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.sandbox.is_empty()
    }

    fn validate(&self) -> Result<()> {
        let mut seen_tools = HashSet::with_capacity(self.tools.len());
        for tool in &self.tools {
            validate_tool_name(tool)?;
            ensure!(seen_tools.insert(tool), "duplicate permission tool");
        }
        let mut seen_sandbox = HashSet::with_capacity(self.sandbox.len());
        for grant in &self.sandbox {
            grant.validate()?;
            ensure!(
                seen_sandbox.insert(grant),
                "duplicate permission sandbox rule"
            );
        }
        Ok(())
    }
}

fn validate_tool_name(tool: &str) -> Result<()> {
    ensure!(
        !tool.trim().is_empty() && tool == tool.trim(),
        "empty permission tool"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fingerprint {
    dev: u64,
    ino: u64,
    len: u64,
    mtime_sec: i64,
    mtime_nsec: i64,
    ctime_sec: i64,
    ctime_nsec: i64,
}

impl Fingerprint {
    fn from_meta(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime_sec: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
            ctime_sec: meta.ctime(),
            ctime_nsec: meta.ctime_nsec(),
        }
    }
}

struct CachedRules {
    fingerprint: Fingerprint,
    document: Arc<Document>,
}

#[derive(Clone)]
pub(crate) struct Store {
    path: PathBuf,
    cache: Arc<Mutex<Option<CachedRules>>>,
}

impl Store {
    pub(crate) fn new(agent_dir: &Path) -> Self {
        Self {
            path: agent_dir.join(PERMISSION_FILE),
            cache: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn protects_path(&self, path: &Path) -> bool {
        let Some(dir) = self.path.parent() else {
            return false;
        };
        let Ok(dir) = dir.canonicalize() else {
            return false;
        };
        let is_rule = path
            .file_name()
            .is_some_and(|file| file == PERMISSION_FILE || file == PERMISSION_LOCK);
        is_rule
            && path
                .parent()
                .and_then(|parent| parent.canonicalize().ok())
                .as_ref()
                == Some(&dir)
    }

    /// 按文件指纹（dev/ino/size/mtime/ctime）按需重载持久规则；文件未变直接复用内存快照。
    pub(crate) fn load(&self) -> Result<Arc<Document>> {
        let meta = match fs::symlink_metadata(&self.path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
                return Ok(Arc::default());
            }
            Err(error) => {
                return Err(anyhow::Error::from(error).context(format!(
                    "loading permission rules from {}",
                    self.path.display()
                )));
            }
        };
        check_private_meta(&meta)
            .with_context(|| format!("loading permission rules from {}", self.path.display()))?;
        let fingerprint = Fingerprint::from_meta(&meta);
        {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = cache.as_ref()
                && cached.fingerprint == fingerprint
            {
                return Ok(Arc::clone(&cached.document));
            }
        }
        let (document, actual_fp) = self
            .read_uncached()
            .with_context(|| format!("loading permission rules from {}", self.path.display()))?;
        let document = Arc::new(document);
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        *cache = actual_fp.map(|fingerprint| CachedRules {
            fingerprint,
            document: Arc::clone(&document),
        });
        Ok(document)
    }

    fn read_uncached(&self) -> Result<(Document, Option<Fingerprint>)> {
        let file = match private_file(&self.path, false) {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == ErrorKind::NotFound) =>
            {
                return Ok((Document::default(), None));
            }
            Err(error) => return Err(error),
        };
        let meta = file.metadata()?;
        check_private_meta(&meta)?;
        ensure!(meta.len() <= MAX_BYTES, "permission file is too large");
        let fingerprint = Fingerprint::from_meta(&meta);
        let mut text = String::new();
        file.take(MAX_BYTES + 1).read_to_string(&mut text)?;
        ensure!(
            text.len() as u64 <= MAX_BYTES,
            "permission file is too large"
        );
        let document: Document = toml::from_str(&text).map_err(|_| {
            anyhow!(
                "invalid TOML permission rules config: check syntax, field types, required fields, and unknown fields"
            )
        })?;
        document.validate()?;
        Ok((document, Some(fingerprint)))
    }

    pub(crate) fn list(&self) -> Result<String> {
        let doc = self.load()?;
        if doc.is_empty() {
            return Ok("No saved permission rules.".into());
        }
        let mut entries = Vec::with_capacity(doc.tools.len() + doc.sandbox.len());
        for (index, tool) in doc.tools.iter().enumerate() {
            let number = index + 1;
            let tool = pp_core::text::display_text(tool);
            entries.push(format!("{number}. Tool: {tool}"));
        }
        for (offset, grant) in doc.sandbox.iter().enumerate() {
            let number = doc.tools.len() + offset + 1;
            let summary = grant.summary(None);
            let mut lines = summary.lines();
            let first = lines.next().unwrap_or("");
            let mut item = format!("{number}. {first}");
            for line in lines {
                item.push_str("\n   ");
                item.push_str(line);
            }
            entries.push(item);
        }
        Ok(entries.join("\n"))
    }

    pub(crate) fn save(&self, grant: SandboxGrant) -> Result<()> {
        grant.validate()?;
        self.update(|doc| {
            if doc.sandbox.iter().any(|existing| existing == &grant) {
                return Ok(false);
            }
            doc.sandbox.push(grant);
            Ok(true)
        })
    }

    pub(crate) fn save_tool(&self, tool: &str) -> Result<()> {
        validate_tool_name(tool)?;
        self.update(|doc| {
            if doc.tools.iter().any(|existing| existing == tool) {
                return Ok(false);
            }
            doc.tools.push(tool.to_owned());
            Ok(true)
        })
    }

    pub(crate) fn revoke(&self, target: &str) -> Result<()> {
        let target = target.trim();
        ensure!(!target.is_empty(), "unknown permission rule: {target}");
        self.update(|doc| {
            if let Some(pos) = doc.tools.iter().position(|tool| tool == target) {
                doc.tools.remove(pos);
                return Ok(true);
            }
            if target.bytes().all(|byte| byte.is_ascii_digit())
                && let Ok(number) = target.parse::<usize>()
                && number >= 1
            {
                let index = number - 1;
                if index < doc.tools.len() {
                    doc.tools.remove(index);
                    return Ok(true);
                }
                let sandbox_index = index - doc.tools.len();
                if sandbox_index < doc.sandbox.len() {
                    doc.sandbox.remove(sandbox_index);
                    return Ok(true);
                }
            }
            bail!("unknown permission rule: {target}");
        })
    }

    fn update(&self, update: impl FnOnce(&mut Document) -> Result<bool>) -> Result<()> {
        let dir = self
            .path
            .parent()
            .context("permission directory unavailable")?;
        fs::create_dir_all(dir)?;
        let lock =
            private_file(&dir.join(PERMISSION_LOCK), true).context("opening permission lock")?;
        loop {
            match rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive) {
                Err(rustix::io::Errno::INTR) => continue,
                result => break result.context("locking permission rules")?,
            }
        }
        let (mut document, disk_fp) = self
            .read_uncached()
            .with_context(|| format!("loading permission rules from {}", self.path.display()))?;
        let dirty = update(&mut document)?;
        if !dirty {
            let document = Arc::new(document);
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            *cache = disk_fp.map(|fingerprint| CachedRules {
                fingerprint,
                document,
            });
            return Ok(());
        }
        let text = encode_document(&document)?;
        ensure!(
            text.len() as u64 <= MAX_BYTES,
            "permission file is too large"
        );
        let mut temporary = tempfile::NamedTempFile::new_in(dir)?;
        temporary.write_all(text.as_bytes())?;
        temporary.as_file().sync_all()?;
        let persisted = temporary
            .persist(&self.path)
            .map_err(|error| error.error)
            .with_context(|| format!("committing permission rules to {}", self.path.display()))?;
        let new_fp = persisted
            .metadata()
            .ok()
            .map(|meta| Fingerprint::from_meta(&meta));
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        *cache = new_fp.map(|fingerprint| CachedRules {
            fingerprint,
            document: Arc::new(document),
        });
        Ok(())
    }
}

fn encode_document(document: &Document) -> Result<String> {
    let mut out = String::new();
    if document.is_empty() {
        writeln!(out, "tools = []")?;
    } else {
        if !document.tools.is_empty() {
            writeln!(out, "tools = [")?;
            for tool in &document.tools {
                out.push_str("    ");
                write_toml_string(&mut out, tool)?;
                writeln!(out, ",")?;
            }
            writeln!(out, "]")?;
        }
        for grant in &document.sandbox {
            if !out.is_empty() {
                writeln!(out)?;
            }
            writeln!(out, "[[sandbox]]")?;
            write!(out, "command = ")?;
            write_toml_string(&mut out, &grant.command)?;
            writeln!(out)?;
            if !grant.writable.is_empty() {
                write!(out, "writable = [")?;
                for (index, path) in grant.writable.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    let path = path
                        .to_str()
                        .context("permission rule path must be valid UTF-8")?;
                    write_toml_string(&mut out, path)?;
                }
                writeln!(out, "]")?;
            }
            if grant.network {
                writeln!(out, "network = true")?;
            }
        }
    }
    let decoded: Document =
        toml::from_str(&out).map_err(|_| anyhow!("cannot encode permission rules"))?;
    ensure!(&decoded == document, "cannot encode permission rules");
    Ok(out)
}

fn write_toml_string(out: &mut String, value: &str) -> Result<()> {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0C}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            ch if ch.is_control() => write!(out, "\\u{:04X}", ch as u32)?,
            ch => out.push(ch),
        }
    }
    out.push('"');
    Ok(())
}

fn check_private_meta(meta: &fs::Metadata) -> Result<()> {
    ensure!(
        meta.file_type().is_file()
            && meta.nlink() == 1
            && meta.mode() & 0o077 == 0
            && meta.uid() == rustix::process::geteuid().as_raw(),
        "permission file must be a private regular file owned by the current user"
    );
    Ok(())
}

fn private_file(path: &Path, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    check_private_meta(&file.metadata()?)?;
    Ok(file)
}
