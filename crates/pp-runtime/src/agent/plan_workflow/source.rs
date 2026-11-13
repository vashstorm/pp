use super::super::Agent;
use super::super::plan_sync::{import_plan_content, reconcile_plan_state, sync_plan_todos};
use super::{NO_ORIGIN, NO_PLAN};
use anyhow::{Result, anyhow, bail};
use pp_core::plan::PlanReview;
use pp_core::plan::ReviewToken;

impl Agent {
    /// 恢复协调入口：计划读写、审阅、审批、进度与恢复访问前按 ID 同步
    /// 实际路径。revision-0/未绑定直接通过；已绑定文档不 clone 整篇正文。
    pub(crate) fn reconcile_plan_location(&mut self) -> Result<()> {
        reconcile_plan_state(
            self.tools.plan_workspace(),
            &mut self.journal,
            &mut self.workflow,
        )
    }

    /// Register on-disk plan bytes as a new revision. Returns `false` when the
    /// file matches the registered snapshot. Draft/Ready documents become
    /// Ready only when the imported content has a genuine Markdown task list;
    /// Executed/Abandoned documents remain read-only after importing progress.
    pub(super) fn import_plan_revision(
        &mut self,
        content: String,
        new_title: &str,
    ) -> Result<bool> {
        import_plan_content(&mut self.journal, &mut self.workflow, content, new_title)
    }

    /// 通过 NOFOLLOW 工作区句柄读取计划，导入外部修订并签发绑定当前
    /// 会话、计划、路径及精确正文的审批 token。文件缺失或不可读时报错，
    /// 不回退到日志快照。Executed/Abandoned 可刷新但不恢复执行授权；
    /// Plan 仅同步正文，Execute 才刷新任务投影，已脱离的普通清单不被替换。
    pub fn read_plan(&mut self) -> Result<PlanReview> {
        self.journal.check()?;
        let document = self
            .workflow
            .plan
            .clone()
            .ok_or_else(|| anyhow!("{NO_PLAN}"))?;
        let report = self.workflow.review.clone();
        if document.revision == 0 {
            // revision-0 绑定尚未首存：没有可解析的文件路径，从不从
            // 历史快照或猜测路径导入。
            bail!("{NO_PLAN}");
        }
        // 按 ID 协调登记路径并导入外部修订；Plan 不生成 TODO。
        // 墓碑、搬迁与首次登记在此暴露。
        sync_plan_todos(
            &self.tools,
            &mut self.journal,
            &mut self.workflow,
            &mut |_| {},
            true,
        )?;
        let document = self.workflow.plan.as_ref().expect("plan bound").clone();
        let session_id = self
            .journal
            .id()
            .ok_or_else(|| anyhow!("{NO_ORIGIN}"))?
            .to_owned();
        Ok(PlanReview {
            token: ReviewToken {
                session_id,
                path: document.path().ok_or_else(|| anyhow!("{NO_PLAN}"))?,
                plan_id: document.id.clone(),
                revision: document.revision,
                content: document.content.clone(),
            },
            document,
            report,
        })
    }
}
