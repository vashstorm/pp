use super::{
    locks::{self, SessionLock},
    validate_id,
};
use crate::history;
use anyhow::{Context, Error, Result, bail, ensure};
use std::{
    collections::{BTreeSet, HashSet},
    fs::{self, Metadata},
    io,
    os::unix::fs::MetadataExt,
    path::Path,
    time::SystemTime,
};

pub struct GarbageCollection {
    pub sessions: (u64, u64),
    pub locks: (u64, u64),
    pub prompts_removed: u64,
    pub orphans: u64,
}

enum StatsPurge<'a> {
    Batch(&'a mut pp_stats::GarbageCollection),
    Immediate(&'a pp_stats::Database),
}

impl StatsPurge<'_> {
    fn remove_session(&mut self, journal: &Path) -> Result<()> {
        match self {
            Self::Batch(stats) => stats.remove_session(journal),
            Self::Immediate(database) => {
                pp_stats::GarbageCollection::delete_session(database, journal)
            }
        }
    }

    fn orphan_session_ids(&self, sessions_dir: &Path) -> Result<Vec<String>> {
        match self {
            Self::Batch(stats) => stats.orphan_session_ids(sessions_dir),
            Self::Immediate(_) => Ok(Vec::new()),
        }
    }
}

struct PurgeContext<'a> {
    dir: &'a Path,
    dir_exists: bool,
    stats: StatsPurge<'a>,
    history_path: &'a Path,
    history: Option<history::Store>,
    history_changed: bool,
    prompts_removed: u64,
}

impl<'a> PurgeContext<'a> {
    fn new(dir: &'a Path, dir_exists: bool, stats: StatsPurge<'a>, history_path: &'a Path) -> Self {
        Self {
            dir,
            dir_exists,
            stats,
            history_path,
            history: None,
            history_changed: false,
            prompts_removed: 0,
        }
    }

    fn ensure_history(&mut self) -> Result<Option<&mut history::Store>> {
        if self.history.is_none() && metadata(self.history_path)?.is_some() {
            self.history = Some(history::Store::open(self.history_path)?);
        }
        Ok(self.history.as_mut())
    }

    /// 唯一权威的会话关联数据清理序列（不含 `.jsonl` 主文件与锁文件本身）。
    fn purge_session_data(&mut self, id: &str, journal: &Path) -> Result<()> {
        self.stats
            .remove_session(journal)
            .context("cleaning session statistics")?;
        if let Some(history) = self.ensure_history()? {
            self.history_changed |= history
                .remove_session(id)
                .context("cleaning session search index")?;
        }
        pp_core::logging::remove_session_prompt(id)
            .map(|removed| self.prompts_removed += u64::from(removed))
            .context("cleaning session prompt log")?;
        if self.dir_exists {
            crate::subagent::remove_session_data(&journal.with_extension("subagents"))
                .context("cleaning subagent session data")?;
            pp_workspace::output::Store::remove(self.dir, id)
                .context("cleaning session artifacts")?;
        }
        Ok(())
    }
}

pub(super) fn purge_session_data_for_delete(
    dir: &Path,
    stats: &pp_stats::Database,
    history_path: &Path,
    id: &str,
    journal: &Path,
) -> Result<()> {
    let mut ctx = PurgeContext::new(dir, true, StatsPurge::Immediate(stats), history_path);
    ctx.purge_session_data(id, journal)
}

pub fn gc(
    cutoff: SystemTime,
    stats: &mut pp_stats::GarbageCollection,
) -> Result<GarbageCollection> {
    let agent = pp_config::config::global_dir()?;
    let root = agent.parent().context("missing pp data directory")?;
    let lock_dir = locks::directory()?;
    for dir in [root, &agent, &root.join("run")] {
        directory_exists(dir)?;
    }
    let dir = agent.join("sessions");
    let history_path = agent.join("history.db");
    let dir_exists = directory_exists(&dir)?;
    let mut ctx = PurgeContext::new(&dir, dir_exists, StatsPurge::Batch(stats), &history_path);
    let mut total = 0;
    let mut expired_removed = 0;
    let mut present = HashSet::new();
    let mut removed = HashSet::new();
    let mut errors: Vec<(String, Error)> = Vec::new();

    if dir_exists {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some((id, staged)) = name.to_str().and_then(session_name) else {
                continue;
            };
            present.insert(id.to_owned());
            let path = entry.path();
            let Some(metadata) = metadata(&path)?.filter(Metadata::is_file) else {
                continue;
            };
            total += 1;
            if !staged && metadata.modified()? >= cutoff {
                continue;
            }
            match clean_expired_session(&mut ctx, &lock_dir, id, staged, &path, &metadata, cutoff) {
                Ok(true) => {
                    expired_removed += 1;
                    removed.insert(id.to_owned());
                }
                Ok(false) => {}
                Err(error) => errors.push((id.to_owned(), error)),
            }
        }
    }

    let orphans = sweep_orphans(&mut ctx, &lock_dir, &present, &mut removed, &mut errors)?;
    if let Some(history) = &mut ctx.history
        && let Err(error) = history.compact(ctx.history_changed)
    {
        errors.push(("history.db".to_owned(), error));
    }
    let locks = locks::gc(&lock_dir, cutoff, &removed).context("cleaning session lock files")?;
    finish_errors(errors)?;
    Ok(GarbageCollection {
        sessions: (total, expired_removed),
        locks,
        prompts_removed: ctx.prompts_removed,
        orphans,
    })
}

fn clean_expired_session(
    ctx: &mut PurgeContext<'_>,
    lock_dir: &Path,
    id: &str,
    staged: bool,
    path: &Path,
    expected: &Metadata,
    cutoff: SystemTime,
) -> Result<bool> {
    let Some(_lock) = SessionLock::try_open(lock_dir, id)? else {
        return Ok(false);
    };
    let Some(current) = metadata(path)? else {
        return Ok(false);
    };
    if !current.is_file()
        || current.dev() != expected.dev()
        || current.ino() != expected.ino()
        || (!staged && current.modified()? >= cutoff)
    {
        return Ok(false);
    }
    let journal = ctx.dir.join(format!("{id}.jsonl"));
    let pending = ctx.dir.join(format!("{id}.jsonl.gc"));
    if staged {
        ensure!(
            metadata(&journal)?.is_none(),
            "session cleanup conflicts with an existing journal"
        );
    } else {
        ensure!(
            metadata(&pending)?.is_none(),
            "session cleanup file already exists"
        );
        // 先从恢复和索引入口移除，再用暂存文件记录清理进度，供中断后重试。
        fs::rename(&journal, &pending).context("staging expired session")?;
    }
    ctx.purge_session_data(id, &journal)?;
    fs::remove_file(pending).context("removing expired session")?;
    Ok(true)
}

fn sweep_orphans(
    ctx: &mut PurgeContext<'_>,
    lock_dir: &Path,
    present: &HashSet<String>,
    removed: &mut HashSet<String>,
    errors: &mut Vec<(String, Error)>,
) -> Result<u64> {
    let candidates = collect_orphan_candidates(ctx, present)?;
    let mut orphans = 0;
    for id in candidates {
        let Some(_lock) = SessionLock::try_open(lock_dir, &id)? else {
            continue;
        };
        let journal = ctx.dir.join(format!("{id}.jsonl"));
        let pending = ctx.dir.join(format!("{id}.jsonl.gc"));
        // 持锁后二次核对日志文件仍不存在，避免与并发新建/恢复/暂存竞争。
        if metadata(&journal)?.is_some() || metadata(&pending)?.is_some() {
            continue;
        }
        match ctx.purge_session_data(&id, &journal) {
            Ok(()) => {
                orphans += 1;
                removed.insert(id);
            }
            Err(error) => errors.push((id, error)),
        }
    }
    Ok(orphans)
}

fn collect_orphan_candidates(
    ctx: &mut PurgeContext<'_>,
    present: &HashSet<String>,
) -> Result<BTreeSet<String>> {
    let mut candidates = BTreeSet::new();
    let mut record = |id: &str| {
        if validate_id(id).is_ok() && !present.contains(id) {
            candidates.insert(id.to_owned());
        }
    };
    if ctx.dir_exists {
        for entry in fs::read_dir(ctx.dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some(id) = name.strip_suffix(".subagents") {
                if validate_id(id).is_ok()
                    && !present.contains(id)
                    && crate::subagent::has_session_data(&entry.path())?
                {
                    record(id);
                }
            } else {
                record(name);
            }
        }
    }
    for id in ctx.stats.orphan_session_ids(ctx.dir)? {
        record(&id);
    }
    if let Some(history) = ctx.ensure_history()? {
        for id in history.orphan_session_ids(present)? {
            record(&id);
        }
    }
    Ok(candidates)
}

fn finish_errors(mut errors: Vec<(String, Error)>) -> Result<()> {
    match errors.len() {
        0 => Ok(()),
        1 => Err(errors.pop().expect("single error").1),
        count => {
            let summary = errors
                .into_iter()
                .map(|(id, error)| format!("{id}: {error:#}"))
                .collect::<Vec<_>>()
                .join("; ");
            bail!("failed to clean {count} sessions: {summary}");
        }
    }
}

fn session_name(name: &str) -> Option<(&str, bool)> {
    let (id, staged) = name
        .strip_suffix(".jsonl.gc")
        .map(|id| (id, true))
        .or_else(|| name.strip_suffix(".jsonl").map(|id| (id, false)))?;
    validate_id(id).ok()?;
    Some((id, staged))
}

fn directory_exists(path: &Path) -> Result<bool> {
    let Some(metadata) = metadata(path)? else {
        return Ok(false);
    };
    ensure!(
        metadata.is_dir(),
        "session storage must be a directory, not a symlink"
    );
    Ok(true)
}

fn metadata(path: &Path) -> io::Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
