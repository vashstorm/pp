//! Session-level dedup cache for local text `read`: keeps the model from re-reading the same unchanged large body and wasting tokens.
//!
//! It applies only to plain local text files (skipping `:raw`, URLs, `skill://`/`artifact://`/`agent://`,
//! images and directories), and only once the body reaches `MIN_DEDUP_CONTENT_BYTES`.
//! A hit requires the metadata fingerprint `(dev, ino, len, mtime)` to match exactly; in `Hashline` mode it additionally requires
//! the matching editable snapshot to still be active, so the model never gets a stale reference it cannot edit.

use pp_config::config::{EditMode, ToolLimits};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

/// Dedup is enabled only for reads whose body reaches this threshold; smaller files return the body directly to avoid extra indirection cost.
pub(crate) const MIN_DEDUP_CONTENT_BYTES: usize = 1024;
const MAX_DEDUP_ENTRIES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ReadKey {
    canonical_path: PathBuf,
    selector: String,
    mode: EditModeKey,
    max_bytes: usize,
    max_lines: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum EditModeKey {
    Replace,
    Hashline,
}

impl From<EditMode> for EditModeKey {
    fn from(mode: EditMode) -> Self {
        match mode {
            EditMode::Replace => Self::Replace,
            EditMode::Hashline => Self::Hashline,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileFingerprint {
    dev: u64,
    ino: u64,
    len: u64,
    mtime_sec: i64,
    mtime_nsec: i64,
}

impl FileFingerprint {
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        if !meta.is_file() {
            return None;
        }
        Some(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime_sec: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        })
    }
}

pub(crate) struct ReadProbe {
    pub(crate) key: ReadKey,
    pub(crate) source: String,
    pub(crate) fingerprint: FileFingerprint,
    pub(crate) workspace_key: Option<PathBuf>,
}

struct CachedEntry {
    fingerprint: FileFingerprint,
    workspace_key: Option<PathBuf>,
    truncated: bool,
    lines_read: u64,
    next_offset: Value,
    next_path: Value,
    ranges: Value,
    touched: u64,
}

#[derive(Default)]
pub(crate) struct ReadDedupCache {
    entries: HashMap<ReadKey, CachedEntry>,
    clock: u64,
}

impl ReadDedupCache {
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(crate) fn invalidate_path(&mut self, path: &Path) {
        let canonical = fs::canonicalize(path).ok();
        self.entries.retain(|key, entry| {
            let matches_canonical = canonical
                .as_ref()
                .is_some_and(|canon| key.canonical_path == *canon);
            let matches_direct = key.canonical_path == path;
            let matches_workspace = entry
                .workspace_key
                .as_deref()
                .is_some_and(|ws_key| ws_key == path);
            !(matches_canonical || matches_direct || matches_workspace)
        });
    }

    pub(crate) fn check(
        &mut self,
        probe: &ReadProbe,
        has_active_snapshot: impl FnOnce(&Path) -> bool,
    ) -> Option<Value> {
        let valid = match self.entries.get(&probe.key) {
            Some(entry) if entry.fingerprint == probe.fingerprint => {
                if probe.key.mode == EditModeKey::Hashline {
                    entry
                        .workspace_key
                        .as_deref()
                        .is_none_or(has_active_snapshot)
                } else {
                    true
                }
            }
            _ => false,
        };
        if !valid {
            self.entries.remove(&probe.key);
            return None;
        }
        self.clock += 1;
        let entry = self.entries.get_mut(&probe.key)?;
        entry.touched = self.clock;
        Some(json!({
            "ok": true,
            "deduplicated": true,
            "source": probe.source,
            "content": "[Unchanged since earlier read in this session; refer to prior read output or pass :raw to re-read verbatim.]",
            "truncated": entry.truncated,
            "lines_read": entry.lines_read,
            "next_offset": entry.next_offset,
            "next_path": entry.next_path,
            "ranges": entry.ranges,
        }))
    }

    pub(crate) fn record(&mut self, probe: ReadProbe, value: &Value) {
        if value.get("ok").and_then(Value::as_bool) != Some(true)
            || value.get("kind").is_some()
            || value.get("warning").is_some()
            || value.get("deduplicated").and_then(Value::as_bool) == Some(true)
        {
            return;
        }
        let Some(content) = value.get("content").and_then(Value::as_str) else {
            return;
        };
        if content.len() < MIN_DEDUP_CONTENT_BYTES {
            return;
        }
        self.clock += 1;
        let entry = CachedEntry {
            fingerprint: probe.fingerprint,
            workspace_key: probe.workspace_key,
            truncated: value
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            lines_read: value.get("lines_read").and_then(Value::as_u64).unwrap_or(0),
            next_offset: value.get("next_offset").cloned().unwrap_or(Value::Null),
            next_path: value.get("next_path").cloned().unwrap_or(Value::Null),
            ranges: value
                .get("ranges")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
            touched: self.clock,
        };
        self.entries.insert(probe.key, entry);
        while self.entries.len() > MAX_DEDUP_ENTRIES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }
}

pub(super) fn build_probe(
    target: &super::selector::Target,
    cwd: &Path,
    workspace: &pp_workspace::save::Workspace,
    mode: EditMode,
    limits: ToolLimits,
) -> Option<ReadProbe> {
    if target.raw || target.is_url() {
        return None;
    }
    if ["skill://", "artifact://", "agent://"]
        .iter()
        .any(|prefix| target.source.starts_with(prefix))
    {
        return None;
    }
    let local = cwd.join(&target.source);
    let canonical_path = fs::canonicalize(&local).ok()?;
    let fingerprint = FileFingerprint::from_path(&canonical_path)?;
    let workspace_key = workspace.key(Path::new(&target.source)).ok();
    Some(ReadProbe {
        key: ReadKey {
            canonical_path,
            selector: target.dedup_selector(),
            mode: mode.into(),
            max_bytes: limits.read_max_bytes,
            max_lines: limits.read_max_lines,
        },
        source: target.source.clone(),
        fingerprint,
        workspace_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::read::selector::Target;
    use pp_workspace::save::Workspace;

    #[test]
    fn read_dedup_hits_unchanged_large_file_and_misses_on_change_or_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let file = cwd.join("large.txt");
        let body = "line of content\n".repeat(120);
        assert!(body.len() >= MIN_DEDUP_CONTENT_BYTES);
        fs::write(&file, &body).unwrap();
        let workspace = Workspace::open(&cwd).unwrap();
        let target = Target::parse("large.txt", &cwd).unwrap();

        let mut cache = ReadDedupCache::default();
        let probe = build_probe(
            &target,
            &cwd,
            &workspace,
            EditMode::Hashline,
            ToolLimits::default(),
        )
        .unwrap();
        assert!(cache.check(&probe, |_| true).is_none());

        let result = json!({
            "ok": true,
            "source": "large.txt",
            "content": body,
            "truncated": false,
            "lines_read": 120,
            "next_offset": null,
            "next_path": null,
            "ranges": [{"start": 1, "end": 120}]
        });
        cache.record(probe, &result);

        // The same file and selector hits dedup while keeping the paging metadata.
        let probe2 = build_probe(
            &target,
            &cwd,
            &workspace,
            EditMode::Hashline,
            ToolLimits::default(),
        )
        .unwrap();
        let hit = cache.check(&probe2, |_| true).unwrap();
        assert_eq!(hit["ok"], true);
        assert_eq!(hit["deduplicated"], true);
        assert_eq!(hit["lines_read"], 120);
        assert!(hit["content"].as_str().unwrap().contains("Unchanged"));

        // A stale Hashline snapshot misses, forcing a re-read that refreshes it.
        assert!(cache.check(&probe2, |_| false).is_none());

        // After recording again, a content change alters the fingerprint and misses.
        cache.record(probe2, &result);
        fs::write(&file, format!("{body}extra\n")).unwrap();
        let probe3 = build_probe(
            &target,
            &cwd,
            &workspace,
            EditMode::Hashline,
            ToolLimits::default(),
        )
        .unwrap();
        assert!(cache.check(&probe3, |_| true).is_none());

        // `:raw` does not use dedup.
        let raw_target = Target::parse("large.txt:raw", &cwd).unwrap();
        assert!(
            build_probe(
                &raw_target,
                &cwd,
                &workspace,
                EditMode::Replace,
                ToolLimits::default()
            )
            .is_none()
        );
    }

    #[test]
    fn read_dedup_skips_small_files_and_honors_path_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let small = cwd.join("small.txt");
        fs::write(&small, "short\n").unwrap();
        let workspace = Workspace::open(&cwd).unwrap();
        let target = Target::parse("small.txt", &cwd).unwrap();
        let mut cache = ReadDedupCache::default();
        let probe = build_probe(
            &target,
            &cwd,
            &workspace,
            EditMode::Replace,
            ToolLimits::default(),
        )
        .unwrap();
        cache.record(
            probe,
            &json!({"ok": true, "source": "small.txt", "content": "short\n", "truncated": false}),
        );
        let probe2 = build_probe(
            &target,
            &cwd,
            &workspace,
            EditMode::Replace,
            ToolLimits::default(),
        )
        .unwrap();
        assert!(cache.check(&probe2, |_| true).is_none());

        // A recorded large file is invalidated by path.
        let big = cwd.join("big.txt");
        let body = "x".repeat(MIN_DEDUP_CONTENT_BYTES + 16);
        fs::write(&big, &body).unwrap();
        let big_target = Target::parse("big.txt", &cwd).unwrap();
        let big_probe = build_probe(
            &big_target,
            &cwd,
            &workspace,
            EditMode::Replace,
            ToolLimits::default(),
        )
        .unwrap();
        cache.record(
            big_probe,
            &json!({"ok": true, "source": "big.txt", "content": body, "truncated": false}),
        );
        cache.invalidate_path(Path::new("big.txt"));
        let big_probe2 = build_probe(
            &big_target,
            &cwd,
            &workspace,
            EditMode::Replace,
            ToolLimits::default(),
        )
        .unwrap();
        assert!(cache.check(&big_probe2, |_| true).is_none());
    }
}
