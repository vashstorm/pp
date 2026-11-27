//! `permission.toml` 原子读写、文件指纹缓存与跨进程排他锁管理。

use super::codec::{
    check_private_meta, encode_document, normalize_against_existing_ancestor, platform_path_eq,
    policy_str, private_file,
};
use super::{
    BashRule, Document, FsAccess, MAX_BYTES, PERMISSION_FILE, PERMISSION_LOCK, SandboxGrant,
    validate_host_fs_path, validate_tool_name, validate_workspace_fs_path,
};
use anyhow::{Context, Result, bail, ensure};
use pp_config::config::PermissionPolicy;
use std::{
    fs,
    io::{ErrorKind, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

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
        let Some(dir) = self
            .path
            .parent()
            .and_then(normalize_against_existing_ancestor)
        else {
            return false;
        };
        let is_rule = path
            .file_name()
            .is_some_and(|file| file == PERMISSION_FILE || file == PERMISSION_LOCK);
        is_rule
            && path
                .parent()
                .and_then(normalize_against_existing_ancestor)
                .is_some_and(|parent| platform_path_eq(&parent, &dir))
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
        let document = Document::from_toml_str(&text)?;
        Ok((document, Some(fingerprint)))
    }

    pub(crate) fn list(&self) -> Result<String> {
        let doc = self.load()?;
        let active = doc.resolve_active_profile()?;
        let mut entries = Vec::new();
        let mut push_item = |line: String| {
            let idx = entries.len() + 1;
            entries.push(format!("{idx}. {line}"));
        };
        for (tool, policy) in &active.tools {
            let tool = pp_core::text::display_text(tool);
            push_item(format!("Tool ({}): {tool}", policy_str(*policy)));
        }
        for (path, access) in &active.filesystem.workspace {
            let path = pp_core::text::display_text(path);
            push_item(format!("Workspace ({}): {path}", access.as_str()));
        }
        for (path, access) in &active.filesystem.host {
            let path = pp_core::text::display_text(path);
            push_item(format!("Filesystem ({}): {path}", access.as_str()));
        }
        if let Some(enabled) = active.network.enabled {
            push_item(format!("Network: enabled={enabled}"));
        }
        for rule in &doc.rules {
            push_item(rule.summary());
        }
        for grant in &doc.sandbox {
            let summary = grant.summary(None);
            let mut lines = summary.lines();
            let first = lines.next().unwrap_or("");
            let mut item = first.to_owned();
            for line in lines {
                item.push_str("\n   ");
                item.push_str(line);
            }
            push_item(item);
        }
        if entries.is_empty() {
            return Ok("No saved permission rules.".into());
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

    pub(crate) fn save_tool(&self, tool: &str, policy: PermissionPolicy) -> Result<()> {
        validate_tool_name(tool)?;
        self.update(|doc| {
            let profile = doc.active_profile_mut();
            if profile.tools.get(tool) == Some(&policy) {
                return Ok(false);
            }
            profile.tools.insert(tool.to_owned(), policy);
            Ok(true)
        })
    }

    pub(crate) fn save_workspace_fs(&self, rel_path: &str, access: FsAccess) -> Result<()> {
        validate_workspace_fs_path(rel_path)?;
        self.update(|doc| {
            let profile = doc.active_profile_mut();
            if profile.filesystem.workspace.get(rel_path) == Some(&access) {
                return Ok(false);
            }
            profile
                .filesystem
                .workspace
                .insert(rel_path.to_owned(), access);
            Ok(true)
        })
    }

    pub(crate) fn save_host_fs(&self, path: &str, access: FsAccess) -> Result<()> {
        validate_host_fs_path(path)?;
        self.update(|doc| {
            let profile = doc.active_profile_mut();
            if profile.filesystem.host.get(path) == Some(&access) {
                return Ok(false);
            }
            profile.filesystem.host.insert(path.to_owned(), access);
            Ok(true)
        })
    }

    pub(crate) fn save_host_reads(&self, paths: &[PathBuf]) -> Result<()> {
        let paths = paths
            .iter()
            .map(|path| {
                let raw = path.to_str().context("non-UTF-8 path cannot be saved")?;
                validate_host_fs_path(raw)?;
                Ok(raw)
            })
            .collect::<Result<Vec<_>>>()?;
        self.update(|doc| {
            let host = &mut doc.active_profile_mut().filesystem.host;
            let mut changed = false;
            for path in paths {
                ensure!(
                    host.get(path) != Some(&FsAccess::None),
                    "{path} is blocked by permission.toml filesystem rule."
                );
                if !matches!(host.get(path), Some(FsAccess::Read | FsAccess::Write)) {
                    host.insert(path.to_owned(), FsAccess::Read);
                    changed = true;
                }
            }
            Ok(changed)
        })
    }

    pub(crate) fn save_network_enabled(&self, enabled: bool) -> Result<()> {
        self.update(|doc| {
            let profile = doc.active_profile_mut();
            if profile.network.enabled == Some(enabled) {
                return Ok(false);
            }
            profile.network.enabled = Some(enabled);
            Ok(true)
        })
    }

    pub(crate) fn save_bash_batch(&self, rules: Vec<BashRule>) -> Result<()> {
        for rule in &rules {
            rule.validate()?;
        }
        self.update(|doc| {
            let mut changed = false;
            for rule in rules {
                if let Some(existing) = doc
                    .rules
                    .iter_mut()
                    .find(|r| r.pattern == rule.pattern && r.match_mode == rule.match_mode)
                {
                    if existing.policy != rule.policy
                        || existing.justification != rule.justification
                    {
                        existing.policy = rule.policy;
                        existing.justification = rule.justification;
                        changed = true;
                    }
                } else {
                    doc.rules.push(rule);
                    changed = true;
                }
            }
            Ok(changed)
        })
    }

    pub(crate) fn revoke(&self, target: &str) -> Result<()> {
        let target = target.trim();
        ensure!(!target.is_empty(), "unknown permission rule: {target}");
        self.update(|doc| {
            let active_name = doc.active_profile_name().to_owned();
            if let Some(profile) = doc.profiles.get_mut(&active_name)
                && profile.tools.remove(target).is_some()
            {
                return Ok(true);
            }
            if let Some(tool) = target.strip_prefix("tool:")
                && let Some(profile) = doc.profiles.get_mut(&active_name)
                && profile.tools.remove(tool.trim()).is_some()
            {
                return Ok(true);
            }
            if let Some(fs_target) = target.strip_prefix("fs:") {
                let fs_target = fs_target.trim();
                if let Some(profile) = doc.profiles.get_mut(&active_name)
                    && (profile.filesystem.workspace.remove(fs_target).is_some()
                        || profile.filesystem.host.remove(fs_target).is_some())
                {
                    return Ok(true);
                }
            }
            if target == "network"
                && let Some(profile) = doc.profiles.get_mut(&active_name)
                && profile.network.enabled.take().is_some()
            {
                return Ok(true);
            }
            if let Some(raw_pattern) = target
                .strip_prefix("rule:")
                .or_else(|| target.strip_prefix("bash:"))
            {
                let normalized = raw_pattern.split_whitespace().collect::<Vec<_>>();
                if !normalized.is_empty()
                    && let Some(pos) = doc.rules.iter().position(|rule| {
                        rule.pattern.len() == normalized.len()
                            && rule
                                .pattern
                                .iter()
                                .zip(normalized.iter())
                                .all(|(a, b)| a == b)
                    })
                {
                    doc.rules.remove(pos);
                    return Ok(true);
                }
            }
            if target.bytes().all(|byte| byte.is_ascii_digit())
                && let Ok(number) = target.parse::<usize>()
                && number >= 1
            {
                let mut cursor = number - 1;
                let active = doc.resolve_active_profile()?;
                if cursor < active.tools.len() {
                    let key = active.tools.keys().nth(cursor).cloned();
                    if let Some(key) = key
                        && let Some(profile) = doc.profiles.get_mut(&active_name)
                        && profile.tools.remove(&key).is_some()
                    {
                        return Ok(true);
                    }
                    bail!("permission rule #{number} is inherited and cannot be revoked directly");
                }
                cursor -= active.tools.len();
                if cursor < active.filesystem.workspace.len() {
                    let key = active.filesystem.workspace.keys().nth(cursor).cloned();
                    if let Some(key) = key
                        && let Some(profile) = doc.profiles.get_mut(&active_name)
                        && profile.filesystem.workspace.remove(&key).is_some()
                    {
                        return Ok(true);
                    }
                    bail!("permission rule #{number} is inherited and cannot be revoked directly");
                }
                cursor -= active.filesystem.workspace.len();
                if cursor < active.filesystem.host.len() {
                    let key = active.filesystem.host.keys().nth(cursor).cloned();
                    if let Some(key) = key
                        && let Some(profile) = doc.profiles.get_mut(&active_name)
                        && profile.filesystem.host.remove(&key).is_some()
                    {
                        return Ok(true);
                    }
                    bail!("permission rule #{number} is inherited and cannot be revoked directly");
                }
                cursor -= active.filesystem.host.len();
                if active.network.enabled.is_some() {
                    if cursor == 0 {
                        if let Some(profile) = doc.profiles.get_mut(&active_name)
                            && profile.network.enabled.take().is_some()
                        {
                            return Ok(true);
                        }
                        bail!(
                            "permission rule #{number} is inherited and cannot be revoked directly"
                        );
                    }
                    cursor -= 1;
                }
                if cursor < doc.rules.len() {
                    doc.rules.remove(cursor);
                    return Ok(true);
                }
                cursor -= doc.rules.len();
                if cursor < doc.sandbox.len() {
                    doc.sandbox.remove(cursor);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protects_path_handles_missing_agent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("missing-pp").join("agent");
        let store = Store::new(&agent_dir);
        assert!(store.protects_path(&agent_dir.join(PERMISSION_FILE)));
        assert!(store.protects_path(&agent_dir.join(PERMISSION_LOCK)));
        assert!(!store.protects_path(&agent_dir.join("other.txt")));
        assert!(!store.protects_path(&dir.path().join("workspace.txt")));
    }
}
