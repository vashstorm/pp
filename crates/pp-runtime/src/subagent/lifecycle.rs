//! 运行取消、回收等待与关闭。

use anyhow::{Result, ensure};
use std::time::{Duration, Instant};

use super::{Binding, lock};

impl Binding {
    /// 只取消当前 owner 明确选中的未完成 Run，空集合不代表全部。
    pub(crate) fn cancel_runs(&self, ids: &[String]) -> Result<()> {
        let state = lock(&self.inner.state);
        for id in ids {
            ensure!(
                state
                    .disk
                    .runs
                    .get(id)
                    .is_some_and(|run| run.owner == self.owner),
                "run does not belong to this agent"
            );
        }
        for run in state.disk.runs.values().filter(|run| {
            ids.contains(&run.id) && matches!(run.status.as_str(), "queued" | "running")
        }) {
            if let Some(live) = state.live.get(&run.agent_id) {
                live.cancellation.cancel();
            }
        }
        self.inner.changed.notify_all();
        Ok(())
    }

    /// 终态由真实执行路径发布；不得靠改 registry 状态假装已经回收。
    pub(crate) fn reap_runs(&self, ids: &[String]) -> Result<()> {
        let started = Instant::now();
        loop {
            self.check_lsp_cleanup()?;
            if ids.is_empty()
                || !self
                    .wait_jobs(ids)?
                    .iter()
                    .any(crate::tools::wait::Job::pending)
            {
                return Ok(());
            }
            if started.elapsed() >= Duration::from_secs(5) {
                return Err(pp_core::cancellation::CleanupError(anyhow::anyhow!(
                    "subagent stop cleanup timed out"
                ))
                .into());
            }
            self.sleep();
        }
    }

    pub(crate) fn cancel_owned(&self) -> Result<()> {
        let state = lock(&self.inner.state);
        for run in
            state.disk.runs.values().filter(|r| {
                r.owner == self.owner && matches!(r.status.as_str(), "queued" | "running")
            })
        {
            if let Some(live) = state.live.get(&run.agent_id) {
                live.cancellation.cancel();
            }
        }
        self.inner.changed.notify_all();
        Ok(())
    }
    pub(crate) fn reap_owned(&self) -> Result<()> {
        self.cancel_owned()?;
        let start = Instant::now();
        loop {
            let running = lock(&self.inner.state).disk.runs.values().any(|r| {
                r.owner == self.owner && matches!(r.status.as_str(), "queued" | "running")
            });
            self.check_lsp_cleanup()?;
            if !running {
                return Ok(());
            }
            if start.elapsed() > Duration::from_secs(5) {
                return Err(pp_core::cancellation::CleanupError(anyhow::anyhow!(
                    "subagent stop cleanup timed out"
                ))
                .into());
            }
            self.sleep();
        }
    }
    pub fn shutdown(&self) -> Result<()> {
        {
            let mut state = lock(&self.inner.state);
            state.stopping = true;
            for live in state.live.values() {
                live.cancellation.cancel();
            }
            state.live.clear();
        }
        let start = Instant::now();
        while lock(&self.inner.state).workers > 0 {
            ensure!(
                start.elapsed() < Duration::from_secs(5),
                "subagent cleanup timed out"
            );
            self.sleep();
        }
        self.check_lsp_cleanup()?;
        lock(&self.inner.factory).take();
        Ok(())
    }
    pub(crate) fn check_lsp_cleanup(&self) -> Result<()> {
        let handles = std::mem::take(&mut *lock(&self.inner.lsp_cleanup));
        let mut errors = std::mem::take(&mut *lock(&self.inner.cleanup_errors));
        let mut pending = Vec::new();
        for mut handle in handles {
            if let Err(error) = handle.stop() {
                pending.push(handle);
                errors.push(error);
            }
        }
        lock(&self.inner.lsp_cleanup).extend(pending);
        let mut errors = errors.into_iter();
        if let Some(mut error) = errors.next() {
            for additional in errors {
                error = error.context(format!(
                    "Additional subagent cleanup failure: {additional:#}"
                ));
            }
            return Err(error);
        }
        Ok(())
    }
}
