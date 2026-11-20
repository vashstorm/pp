//! 用户级的持久沙盒授权。

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
};

const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxGrant {
    pub workspace: PathBuf,
    pub cwd: PathBuf,
    pub command: String,
    pub writable: Vec<PathBuf>,
    pub network: bool,
}

impl SandboxGrant {
    fn validate(&self) -> Result<()> {
        let paths = [&self.workspace, &self.cwd]
            .into_iter()
            .chain(&self.writable);
        ensure!(
            paths.into_iter().all(|path| {
                path.is_absolute()
                    && !path
                        .components()
                        .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
            }),
            "invalid approval rule path"
        );
        ensure!(!self.command.trim().is_empty(), "empty approval command");
        ensure!(
            self.network || !self.writable.is_empty(),
            "empty approval scope"
        );
        ensure!(
            self.writable.windows(2).all(|pair| pair[0] < pair[1]),
            "invalid approval scope order"
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
        let mut summary = format!("Directory: {}", self.cwd.display());
        if !writes.is_empty() {
            summary.push_str(&format!("\nExtra writable directories: {writes}"));
        }
        if self.network {
            summary.push_str("\nAllow outbound network for this command and its subprocesses");
        }
        summary.push_str(&format!("\nCommand: {command}"));
        pp_core::text::display_text(&summary).into_owned()
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rule {
    id: String,
    pub(crate) grant: SandboxGrant,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    rules: Vec<Rule>,
}

#[derive(Clone)]
pub(crate) struct Store {
    path: PathBuf,
}

impl Store {
    pub(crate) fn new(agent_dir: &Path) -> Self {
        Self {
            path: agent_dir.join("approval.yml"),
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
            .is_some_and(|file| file == "approval.yml" || file == "approval.lock");
        is_rule
            && path
                .parent()
                .and_then(|parent| parent.canonicalize().ok())
                .as_ref()
                == Some(&dir)
    }

    pub(crate) fn load(&self) -> Result<Vec<Rule>> {
        self.read()
            .with_context(|| format!("loading approval rules from {}", self.path.display()))
    }

    fn read(&self) -> Result<Vec<Rule>> {
        let file = match private_file(&self.path, false) {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == ErrorKind::NotFound) =>
            {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        let mut text = String::new();
        file.take(MAX_BYTES + 1).read_to_string(&mut text)?;
        ensure!(text.len() as u64 <= MAX_BYTES, "approval file is too large");
        let document: Document = pp_config::yaml::parse(&text, "approval rules")?;
        ensure!(document.version == 1, "unsupported approval file version");
        let mut ids = HashSet::new();
        for rule in &document.rules {
            ensure!(
                rule.id.len() == 32
                    && rule.id.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && ids.insert(&rule.id),
                "invalid or duplicate approval rule id"
            );
            rule.grant.validate()?;
        }
        Ok(document.rules)
    }

    pub(crate) fn list(&self) -> Result<String> {
        let rules = self.load()?;
        if rules.is_empty() {
            return Ok("No saved permission rules.".into());
        }
        Ok(rules
            .iter()
            .map(|rule| {
                let project = pp_core::text::display_text(&rule.grant.workspace.to_string_lossy())
                    .into_owned();
                format!("{} · {}\n{}", rule.id, project, rule.grant.summary())
            })
            .collect::<Vec<_>>()
            .join("\n\n"))
    }

    pub(crate) fn save(&self, grant: SandboxGrant) -> Result<()> {
        grant.validate()?;
        self.update(|rules| {
            if rules.iter().any(|rule| rule.grant == grant) {
                return Ok(());
            }
            let mut random = [0; 16];
            getrandom::fill(&mut random)
                .map_err(|_| anyhow::anyhow!("cannot generate approval rule id"))?;
            rules.push(Rule {
                id: format!("{:032x}", u128::from_be_bytes(random)),
                grant,
            });
            Ok(())
        })
    }

    pub(crate) fn revoke(&self, id: &str) -> Result<()> {
        self.update(|rules| {
            let previous = rules.len();
            rules.retain(|rule| rule.id != id);
            ensure!(previous != rules.len(), "unknown permission rule id");
            Ok(())
        })
    }

    fn update(&self, update: impl FnOnce(&mut Vec<Rule>) -> Result<()>) -> Result<()> {
        let dir = self
            .path
            .parent()
            .context("approval directory unavailable")?;
        fs::create_dir_all(dir)?;
        let lock =
            private_file(&dir.join("approval.lock"), true).context("opening approval lock")?;
        let guard = lock;
        loop {
            match rustix::fs::flock(&guard, rustix::fs::FlockOperation::LockExclusive) {
                Err(rustix::io::Errno::INTR) => continue,
                result => break result.context("locking approval rules")?,
            }
        }
        let mut rules = self.load()?;
        update(&mut rules)?;
        let text = serde_yaml::to_string(&Document { version: 1, rules })
            .map_err(|_| anyhow::anyhow!("cannot encode approval rules"))?;
        ensure!(text.len() as u64 <= MAX_BYTES, "approval file is too large");
        let mut temporary = tempfile::NamedTempFile::new_in(dir)?;
        temporary.write_all(text.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&self.path)
            .map_err(|error| error.error)
            .with_context(|| format!("committing approval rules to {}", self.path.display()))?;
        Ok(())
    }
}

fn private_file(path: &Path, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.nlink() == 1
            && meta.mode() & 0o077 == 0
            && meta.uid() == rustix::process::geteuid().as_raw(),
        "approval file must be a private regular file owned by the current user"
    );
    Ok(file)
}
