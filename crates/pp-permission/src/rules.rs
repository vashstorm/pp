//! 用户级的持久全局授权与跨进程文件同步。

use anyhow::{Context, Result, anyhow, ensure};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub command: String,
    #[serde(default)]
    pub writable: Vec<PathBuf>,
    #[serde(default)]
    pub network: bool,
}

impl SandboxGrant {
    pub fn into_global(mut self) -> Self {
        self.workspace = None;
        self.cwd = None;
        self
    }

    pub fn is_global(&self) -> bool {
        self.workspace.is_none() && self.cwd.is_none()
    }

    pub fn matches(&self, request: &Self) -> bool {
        self.same_scope(request)
            && (self.is_global()
                || (self.workspace == request.workspace && self.cwd == request.cwd))
    }

    fn same_scope(&self, other: &Self) -> bool {
        self.command == other.command
            && self.writable == other.writable
            && self.network == other.network
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.workspace.is_some() == self.cwd.is_some(),
            "invalid permission rule scope"
        );
        let paths = [self.workspace.as_deref(), self.cwd.as_deref()]
            .into_iter()
            .flatten()
            .chain(self.writable.iter().map(PathBuf::as_path));
        ensure!(
            paths.into_iter().all(|path| {
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

    pub fn summary(&self) -> String {
        let writes = self
            .writable
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let command = pp_core::logging::diagnostic_text(&self.command);
        let mut lines = Vec::new();
        if let Some(cwd) = &self.cwd {
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

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolGrant {
    pub(crate) tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) server: Option<String>,
}

impl ToolGrant {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.tool.trim().is_empty() && self.tool == self.tool.trim(),
            "empty permission tool"
        );
        if let Some(server) = &self.server {
            ensure!(
                !server.is_empty()
                    && server.len() <= 128
                    && server
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric()
                            || matches!(byte, b'-' | b'_' | b':')),
                "invalid permission server identity"
            );
        }
        Ok(())
    }

    fn summary(&self) -> String {
        let tool = pp_core::text::display_text(&self.tool);
        match &self.server {
            Some(server) => {
                let short = &server[..server.len().min(12)];
                format!("Tool: {tool} (server {short})")
            }
            None => format!("Tool: {tool}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Grant {
    Sandbox(SandboxGrant),
    Tool(ToolGrant),
}

impl Grant {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Sandbox(grant) => grant.validate(),
            Self::Tool(grant) => grant.validate(),
        }
    }

    fn scope_label(&self) -> String {
        match self {
            Self::Sandbox(grant) => match &grant.workspace {
                Some(workspace) => {
                    pp_core::text::display_text(&workspace.to_string_lossy()).into_owned()
                }
                None => "global".to_owned(),
            },
            Self::Tool(_) => "global".to_owned(),
        }
    }

    fn summary(&self) -> String {
        match self {
            Self::Sandbox(grant) => grant.summary(),
            Self::Tool(grant) => grant.summary(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rule {
    pub(crate) id: String,
    pub(crate) grant: Grant,
}

impl Rule {
    pub(crate) fn matches_tool(&self, name: &str, server: Option<&str>) -> bool {
        matches!(&self.grant, Grant::Tool(grant) if grant.tool == name
        && if name.starts_with("mcp__") {
            server.is_some() && grant.server.as_deref() == server
        } else {
            grant.server.is_none() && server.is_none()
        })
    }

    pub(crate) fn matches_sandbox(&self, request: &SandboxGrant) -> bool {
        matches!(&self.grant, Grant::Sandbox(grant) if grant.matches(request))
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    #[serde(default)]
    rules: Vec<Rule>,
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
    rules: Arc<[Rule]>,
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
    pub(crate) fn load(&self) -> Result<Arc<[Rule]>> {
        let meta = match fs::symlink_metadata(&self.path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
                return Ok(Arc::from([]));
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
                return Ok(Arc::clone(&cached.rules));
            }
        }
        let (rules, actual_fp) = self
            .read_uncached()
            .with_context(|| format!("loading permission rules from {}", self.path.display()))?;
        let rules: Arc<[Rule]> = Arc::from(rules);
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        *cache = actual_fp.map(|fingerprint| CachedRules {
            fingerprint,
            rules: Arc::clone(&rules),
        });
        Ok(rules)
    }

    fn read_uncached(&self) -> Result<(Vec<Rule>, Option<Fingerprint>)> {
        let file = match private_file(&self.path, false) {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == ErrorKind::NotFound) =>
            {
                return Ok((Vec::new(), None));
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
        ensure!(document.version == 1, "unsupported permission file version");
        let mut ids = HashSet::new();
        for rule in &document.rules {
            ensure!(
                rule.id.len() == 32
                    && rule.id.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && ids.insert(&rule.id),
                "invalid or duplicate permission rule id"
            );
            rule.grant.validate()?;
        }
        Ok((document.rules, Some(fingerprint)))
    }

    pub(crate) fn list(&self) -> Result<String> {
        let rules = self.load()?;
        if rules.is_empty() {
            return Ok("No saved permission rules.".into());
        }
        Ok(rules
            .iter()
            .map(|rule| {
                format!(
                    "{} · {}\n{}",
                    rule.id,
                    rule.grant.scope_label(),
                    rule.grant.summary()
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n"))
    }

    pub(crate) fn save(&self, grant: SandboxGrant) -> Result<()> {
        let grant = grant.into_global();
        grant.validate()?;
        self.update(|rules| {
            let has_exact = rules
                .iter()
                .any(|rule| matches!(&rule.grant, Grant::Sandbox(existing) if existing == &grant));
            let previous = rules.len();
            rules.retain(|rule| {
                !matches!(
                    &rule.grant,
                    Grant::Sandbox(existing) if existing != &grant && existing.same_scope(&grant)
                )
            });
            let subsumed = rules.len() != previous;
            if has_exact {
                return Ok(subsumed);
            }
            rules.push(Rule {
                id: generate_rule_id()?,
                grant: Grant::Sandbox(grant),
            });
            Ok(true)
        })
    }

    pub(crate) fn save_tool(&self, tool: &str, server: Option<&str>) -> Result<()> {
        ensure!(
            !tool.starts_with("mcp__") || server.is_some(),
            "MCP persistent permission requires server identity"
        );
        let grant = ToolGrant {
            tool: tool.to_owned(),
            server: server.map(str::to_owned),
        };
        grant.validate()?;
        self.update(|rules| {
            if rules
                .iter()
                .any(|rule| matches!(&rule.grant, Grant::Tool(existing) if existing == &grant))
            {
                return Ok(false);
            }
            rules.push(Rule {
                id: generate_rule_id()?,
                grant: Grant::Tool(grant),
            });
            Ok(true)
        })
    }

    pub(crate) fn revoke(&self, id: &str) -> Result<()> {
        self.update(|rules| {
            let previous = rules.len();
            rules.retain(|rule| rule.id != id);
            ensure!(previous != rules.len(), "unknown permission rule id");
            Ok(true)
        })
    }

    fn update(&self, update: impl FnOnce(&mut Vec<Rule>) -> Result<bool>) -> Result<()> {
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
        let (mut rules, disk_fp) = self
            .read_uncached()
            .with_context(|| format!("loading permission rules from {}", self.path.display()))?;
        let dirty = update(&mut rules)?;
        if !dirty {
            let rules: Arc<[Rule]> = Arc::from(rules);
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            *cache = disk_fp.map(|fingerprint| CachedRules { fingerprint, rules });
            return Ok(());
        }
        let text = encode_document(&Document {
            version: 1,
            rules: rules.clone(),
        })?;
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
            rules: Arc::from(rules),
        });
        Ok(())
    }
}

fn generate_rule_id() -> Result<String> {
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|_| anyhow!("cannot generate permission rule id"))?;
    Ok(format!("{:032x}", u128::from_be_bytes(random)))
}

fn encode_document(document: &Document) -> Result<String> {
    let mut out = String::new();
    writeln!(out, "version = {}", document.version)?;
    if document.rules.is_empty() {
        writeln!(out, "rules = []")?;
    } else {
        for rule in &document.rules {
            writeln!(out)?;
            writeln!(out, "[[rules]]")?;
            write!(out, "id = ")?;
            write_toml_string(&mut out, &rule.id)?;
            writeln!(out)?;
            writeln!(out)?;
            writeln!(out, "[rules.grant]")?;
            match &rule.grant {
                Grant::Sandbox(grant) => {
                    if let Some(workspace) = &grant.workspace {
                        let workspace = workspace
                            .to_str()
                            .context("permission rule path must be valid UTF-8")?;
                        write!(out, "workspace = ")?;
                        write_toml_string(&mut out, workspace)?;
                        writeln!(out)?;
                    }
                    if let Some(cwd) = &grant.cwd {
                        let cwd = cwd
                            .to_str()
                            .context("permission rule path must be valid UTF-8")?;
                        write!(out, "cwd = ")?;
                        write_toml_string(&mut out, cwd)?;
                        writeln!(out)?;
                    }
                    write!(out, "command = ")?;
                    write_toml_string(&mut out, &grant.command)?;
                    writeln!(out)?;
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
                    writeln!(out, "network = {}", grant.network)?;
                }
                Grant::Tool(grant) => {
                    write!(out, "tool = ")?;
                    write_toml_string(&mut out, &grant.tool)?;
                    writeln!(out)?;
                    if let Some(server) = &grant.server {
                        write!(out, "server = ")?;
                        write_toml_string(&mut out, server)?;
                        writeln!(out)?;
                    }
                }
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
