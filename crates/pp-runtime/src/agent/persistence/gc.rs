use super::{
    locks::{self, SessionLock},
    validate_id,
};
use crate::history;
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashSet,
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
    let mut history = None;
    let mut total = 0;
    let mut removed = HashSet::new();
    let mut prompts_removed = 0;
    if directory_exists(&dir)? {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some((id, staged)) = name.to_str().and_then(session_name) else {
                continue;
            };
            let path = entry.path();
            let Some(metadata) = metadata(&path)?.filter(Metadata::is_file) else {
                continue;
            };
            total += 1;
            if !staged && metadata.modified()? >= cutoff {
                continue;
            }
            let Some(_lock) = SessionLock::try_open(&lock_dir, id)? else {
                continue;
            };
            let Some(current) = self::metadata(&path)? else {
                continue;
            };
            if !current.is_file()
                || current.dev() != metadata.dev()
                || current.ino() != metadata.ino()
                || (!staged && current.modified()? >= cutoff)
            {
                continue;
            }
            let journal = dir.join(format!("{id}.jsonl"));
            let pending = dir.join(format!("{id}.jsonl.gc"));
            if history.is_none() && self::metadata(&history_path)?.is_some() {
                history = Some(history::Store::open(&history_path)?);
            }
            if staged {
                ensure!(
                    self::metadata(&journal)?.is_none(),
                    "session cleanup conflicts with an existing journal"
                );
            } else {
                ensure!(
                    self::metadata(&pending)?.is_none(),
                    "session cleanup file already exists"
                );
                // 先从恢复和索引入口移除，再用暂存文件记录清理进度，供中断后重试。
                fs::rename(&journal, &pending).context("staging expired session")?;
            }
            stats
                .remove_session(&journal)
                .context("cleaning session statistics")?;
            if let Some(history) = &mut history {
                history
                    .remove_session(id)
                    .context("cleaning session search index")?;
            }
            pp_core::logging::remove_session_prompt(id)
                .map(|removed| prompts_removed += u64::from(removed))
                .context("cleaning session prompt log")?;
            crate::subagent::remove_session_data(&journal.with_extension("subagents"))
                .context("cleaning subagent session data")?;
            pp_workspace::output::Store::remove(&dir, id).context("cleaning session artifacts")?;
            fs::remove_file(pending).context("removing expired session")?;
            removed.insert(id.to_owned());
        }
    }
    let locks = locks::gc(&lock_dir, cutoff, &removed).context("cleaning session lock files")?;
    Ok(GarbageCollection {
        sessions: (total, removed.len() as u64),
        locks,
        prompts_removed,
    })
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
