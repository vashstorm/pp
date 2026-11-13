use super::format::{HeaderRef, Message, StateRef, WorkflowRef};
use super::*;
use anyhow::ensure;
use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;

impl SessionJournal {
    pub(in crate::agent) fn checkpoint(
        &mut self,
        metadata: &Metadata<'_>,
        session: SessionRef<'_>,
        workflow: &WorkflowState,
    ) -> Result<()> {
        self.check()?;
        let Some(active) = &self.active else {
            return Ok(());
        };
        let first = !active.header_written;
        if first {
            let result = self.write_header(metadata);
            self.latch(result)?;
        } else {
            self.record(
                PayloadRef::ModelChange {
                    model: metadata.model_reference,
                    role: "default",
                },
                false,
            )?;
        }
        self.write_state(metadata, session, workflow)?;
        if first {
            self.write_first_records(metadata)?;
        }
        Ok(())
    }

    /// Write the session header exactly once; callers decide how to latch a
    /// failure (`checkpoint` latches, plan execution rolls the group back).
    fn write_header(&mut self, metadata: &Metadata<'_>) -> Result<()> {
        let active = self.active.as_ref().expect("active journal checked");
        let timestamp = timestamp(active.created_at_ms)?;
        self.buffer.clear();
        let serialized = serde_json::to_writer(
            &mut self.buffer,
            &HeaderRef {
                kind: "session",
                id: &active.id,
                timestamp: &timestamp,
                cwd: metadata.cwd,
            },
        );
        self.append_serialized(serialized, false)?;
        self.active
            .as_mut()
            .expect("active journal checked")
            .header_written = true;
        Ok(())
    }

    /// The first-record audit pair that follows a freshly written header.
    fn write_first_records(&mut self, metadata: &Metadata<'_>) -> Result<()> {
        self.record(
            PayloadRef::ModelChange {
                model: metadata.model_reference,
                role: "default",
            },
            false,
        )?;
        self.think(metadata.think_level)
    }
    /// Commit a complete native history without inventing a model-switch audit.
    /// The new state record is the logical commit point; old records remain.
    pub(in crate::agent) fn replace_history(
        &mut self,
        metadata: &Metadata<'_>,
        session: SessionRef<'_>,
        workflow: &WorkflowState,
    ) -> Result<()> {
        self.check()?;
        ensure!(
            self.active
                .as_ref()
                .is_some_and(|active| active.header_written),
            "persisting session: missing session checkpoint"
        );
        self.write_state(metadata, session, workflow)
    }

    fn write_state(
        &mut self,
        metadata: &Metadata<'_>,
        session: SessionRef<'_>,
        workflow: &WorkflowState,
    ) -> Result<()> {
        self.write_tree_state(metadata, session, workflow, None)
    }

    pub(in crate::agent) fn write_tree_state(
        &mut self,
        metadata: &Metadata<'_>,
        session: SessionRef<'_>,
        workflow: &WorkflowState,
        tree: Option<&tree::Selection>,
    ) -> Result<()> {
        let mut state = StateRef::new(metadata, session, workflow);
        state.tree = tree;
        // 导航失败时保留此前完整提交，避免半条或未同步的 state 在恢复时生效。
        self.synced_custom(CustomRef::State(state))?;
        self.committed = session.len();
        self.model_reference.clear();
        self.model_reference.push_str(metadata.model_reference);
        self.wire_model = match session {
            SessionRef::Completion(request) => request.model.clone(),
            SessionRef::Response(request) => request.model.clone().unwrap_or_default(),
        };
        self.capture_thinking = matches!(session, SessionRef::Completion(_));
        self.thinking.clear();
        self.pending_checkpoint = false;
        self.pending_tools = false;
        Ok(())
    }

    /// Offset to truncate back to if a synced write group fails mid-way.
    fn rollback_offset(&self) -> Result<Option<u64>> {
        self.active
            .as_ref()
            .map(|active| {
                active
                    .file
                    .metadata()
                    .map(|m| active.truncate_to.unwrap_or(m.len()))
            })
            .transpose()
            .context("reading session rollback boundary")
    }

    fn rollback_group(&mut self, offset: Option<u64>, error: anyhow::Error) -> anyhow::Error {
        if let Some(offset) = offset
            && let Some(active) = &mut self.active
            && let Err(rollback) = active
                .file
                .set_len(offset)
                .and_then(|()| active.file.sync_data())
        {
            // 回滚失败不改变已登记的持久化错误：重复失败必须返回同一信息。
            eprintln!("Failed to roll back failed session state write: {rollback}");
        }
        error
    }

    /// Synced custom record commit: on failure truncate back to the previous
    /// length and sync so no half-written or unsynced state can restore.
    pub(super) fn synced_custom(&mut self, custom: CustomRef<'_>) -> Result<()> {
        let rollback = self.rollback_offset()?;
        if let Err(error) = self.custom(custom, true) {
            return Err(self.rollback_group(rollback, error));
        }
        Ok(())
    }

    /// Persist a workflow-only update (mode changes, approvals, revisions).
    /// This is the transactional boundary for every permission grant.
    pub(in crate::agent) fn write_workflow(&mut self, workflow: &WorkflowState) -> Result<()> {
        self.check()?;
        if self.active.is_none() {
            return Ok(());
        }
        self.synced_custom(CustomRef::Workflow(WorkflowRef { workflow }))
    }

    /// 退出 Plan 时一次提交模式和模型；空会话仍等首条请求才落盘。
    pub(in crate::agent) fn commit_plan_exit(
        &mut self,
        metadata: &Metadata<'_>,
        session: SessionRef<'_>,
        workflow: &WorkflowState,
    ) -> Result<()> {
        self.check()?;
        if self.active.is_none() {
            return Ok(());
        }
        self.write_state(metadata, session, workflow)
    }

    /// Commit the approved plan execution handoff: the user projection and the
    /// native state (which already contains the user message) form one atomic
    /// group. Failure rolls both back and never reports a running task.
    pub(in crate::agent) fn commit_plan_handoff(
        &mut self,
        metadata: &Metadata<'_>,
        session: SessionRef<'_>,
        workflow: &WorkflowState,
        prompt: &str,
    ) -> Result<()> {
        self.check()?;
        self.ensure_active()?;
        let rollback = self.rollback_offset()?;
        let result = (|| -> Result<()> {
            let first = !self
                .active
                .as_ref()
                .expect("active journal after ensure_active")
                .header_written;
            if first {
                self.write_header(metadata)?;
            }
            let timestamp = now_ms()?;
            self.record(
                PayloadRef::Message {
                    message: &Message::User {
                        content: prompt,
                        timestamp,
                    },
                },
                true,
            )?;
            self.write_state(metadata, session, workflow)?;
            if first {
                self.write_first_records(metadata)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            return Err(self.rollback_group(rollback, error));
        }
        Ok(())
    }

    pub(super) fn ensure_active(&mut self) -> Result<()> {
        if self.active.is_some() {
            return Ok(());
        }
        let created = (|| {
            let created_at_ms = now_ms()?;
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&self.dir)
                .context("creating session directory")?;
            create(&self.dir, &self.lock_dir, created_at_ms, || {
                let mut random = [0; 16];
                getrandom::fill(&mut random)
                    .map_err(|_| anyhow::anyhow!("session OS random source failed"))?;
                Ok(random)
            })
        })();
        self.active = Some(match created {
            Ok(active) => active,
            Err(error) => {
                let message = format!("persisting session: {error:#}");
                self.first_error = Some(message.clone());
                bail!("{message}");
            }
        });
        self.notice = self.id().map(str::to_owned);
        Ok(())
    }
}
