//! 从 `/plan-list` 打开既有计划：一律以“仅携带计划”的全新会话继续草稿
//! 或进入执行前审阅。
//!
//! 候选会话不复制原会话的任何消息、推理、摘要或 origin prompt：初始唯一
//! 用户内容是本地选择动作、计划 ID、实际路径与完整 Markdown 快照。候选
//! journal 完整提交后才替换当前会话；任何失败都保持原会话、模式、输入与
//! 文件不变（候选会话文件可能留在会话列表，与 Fresh 批准的失败残留一致）。
//! 打开动作从不调用模型：ContinueDraft 等待用户下一条输入，Execute 打开
//! 后只进入本地审阅，执行仍必须经 `approve_plan` 显式批准。

use super::{
    Agent, ExecutionPreparation, Session,
    persistence::Metadata,
    plan_management::{PlanOpenIntent, PlanSelection, plan_display_title},
};
use crate::{
    cancellation::{self, CancellationToken},
    plan::{self, PlanDocument, PlanLocation, PlanStatus, WorkMode, WorkflowState},
};
use anyhow::{Result, bail, ensure};

/// 已提交任务但 revision-0 尚未首存的草稿不可被“另开会话”静默丢弃。
const UNSAVED_DRAFT: &str = "Leave the current Plan mode before opening another plan.";

/// 初始唯一用户内容：本地选择动作 + 计划 ID + 实际路径 + 完整 Markdown
/// 快照；绝不携带原会话聊天或推理。
fn selection_prompt(intent: PlanOpenIntent, id: &str, path: &str, content: &str) -> String {
    let action = match intent {
        PlanOpenIntent::ContinueDraft => "Continue planning the selected Markdown plan below.",
        PlanOpenIntent::Execute => "Execute the selected Markdown plan below after local approval.",
    };
    format!(
        "{action}\n\nPlan id: {id}\nPath: {path}\n\n[Selected Markdown snapshot]\n{content}\n[/Selected Markdown snapshot]"
    )
}

/// 固定的 origin_prompt 文案；`<path>` 为登记表解析出的实际项目相对路径。
fn origin_prompt(intent: PlanOpenIntent, path: &str) -> String {
    match intent {
        PlanOpenIntent::ContinueDraft => {
            format!("Continue planning the selected Markdown plan at {path}.")
        }
        PlanOpenIntent::Execute => {
            format!("Execute the selected Markdown plan at {path} after local approval.")
        }
    }
}

impl Agent {
    /// 从列表打开计划：ContinueDraft 建立 Plan/Draft 新会话继续草稿；
    /// Execute 建立新会话打开六项操作菜单，有 TODO 为 Ready，否则为 Draft（执行仍需本地批准，
    /// `validate` 显式允许 Plan+Ready+Plans 的预执行审阅状态）。
    ///
    /// 顺序：当前会话 journal.check 与当前草稿的 workflow/登记同步 → 取得
    /// 计划 lease 并按 ID 重查登记、核对精确字节 → 候选会话/journal 完整
    /// 提交 → Plan 模式 MCP shutdown 门禁 → 全部成功才替换当前会话并发布
    /// `replay = Some(空)` 的 ExecutionPreparation。失败出口候选被丢弃，
    /// 原会话/模式/输入/文件保持不变。
    pub(crate) fn open_listed_plan(
        &mut self,
        selection: &PlanSelection,
        intent: PlanOpenIntent,
        cancellation: &CancellationToken,
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        self.journal.check()?;
        // 未落盘的 revision-0 草稿没有可保存的工作；先保存或放弃，避免
        // “另开会话”被当作隐式丢弃。
        if let Some(plan) = &self.workflow.plan
            && plan.revision == 0
            && !plan.origin_prompt.trim().is_empty()
        {
            bail!("{UNSAVED_DRAFT}");
        }
        // 当前已保存草稿的 workflow/登记先同步成功，才准备候选。
        self.reconcile_plan_location()?;
        // 释放当前任务持有的 lease，避免同一进程对同一锁文件的自冲突；
        // 失败出口不恢复它（worker 只在任务间隙调用，按需重取）。
        self.plan_lease = None;
        // 取得 lease 并按 ID 重查登记、核对当前路径与精确字节；此后锁顺序
        // 固定为 lease → 登记表锁。
        let verified = self.verify_selection(selection)?;
        // 意图与生命周期位置匹配：继续仅限 Drafts（c 的约束），执行审阅
        // 允许 Drafts/Plans；Done 只读，不能从列表继续或执行。
        ensure!(
            match intent {
                PlanOpenIntent::ContinueDraft => verified.location == PlanLocation::Drafts,
                PlanOpenIntent::Execute => matches!(
                    verified.location,
                    PlanLocation::Drafts | PlanLocation::Plans
                ),
            },
            "This plan cannot be opened from its current location."
        );
        let document = PlanDocument {
            id: verified.id.clone(),
            revision: 1,
            title: plan_display_title(&verified.content, &verified.file_name),
            content: verified.content.clone(),
            status: match intent {
                PlanOpenIntent::ContinueDraft => PlanStatus::Draft,
                PlanOpenIntent::Execute if plan::task_progress(&verified.content).1 > 0 => {
                    PlanStatus::Ready
                }
                PlanOpenIntent::Execute => PlanStatus::Draft,
            },
            origin_prompt: origin_prompt(intent, &verified.path),
            file_name: Some(verified.file_name.clone()),
            location: verified.location,
            approved_tasks: None,
        };
        plan::validate_content(&document.content)?;
        // 载入计划只建立规划会话；任务投影在本地批准执行后初始化。
        let candidate_workflow = WorkflowState {
            mode: WorkMode::Plan,
            plan: Some(document),
            ..Default::default()
        };
        candidate_workflow.validate()?;
        let selection = self.catalog.role_selection(
            crate::config::ModelRole::Plan,
            &self.current_model.reference,
            self.think_level,
        )?;
        let config = self.catalog.resolve(&selection.model)?;
        let client = crate::llm::OpenAiClient::new(&config)?;
        let prompt = selection_prompt(intent, &verified.id, &verified.path, &verified.content);
        // 新 Plan 使用 PLAN 的模型与 effort；工具仍是 Plan 白名单，不连接 MCP。
        let prepared = (|| -> Result<(Session, super::persistence::SessionJournal)> {
            let definitions = self.tools.all_definitions(WorkMode::Plan);
            let system = self.compose_prompt(
                &candidate_workflow.instructions(),
                "plan",
                &config.model,
                definitions.iter().map(|tool| tool.name.clone()).collect(),
            )?;
            let mut session = Session::new(
                config.model.clone(),
                config.protocol,
                system,
                self.tools.edit_mode(),
            );
            session.set_think_level(selection.effort);
            session.set_tools(definitions);
            super::turn::append_user_to(&mut session, &prompt);
            let metadata = Metadata {
                cwd: self.tools.cwd(),
                model_reference: &selection.model,
                think_level: selection.effort,
                edit_mode: self.tools.edit_mode(),
                tool_limits: self.tools.limits,
                base_system: &self.base_system,
                prompt_source: self.prompt_source.as_ref(),
            };
            // 候选 journal 独立 fork：目标会话先完整提交，提交失败时原
            // 会话的 journal 从未被替换。
            let mut journal = self.journal.fork_empty();
            journal.commit_plan_handoff(
                &metadata,
                (&session).into(),
                &candidate_workflow,
                &prompt,
            )?;
            Ok((session, journal))
        })();
        let (session, journal) = prepared?;
        // Plan 候选的原有 MCP shutdown 门禁；失败不交接新会话（候选被
        // 丢弃，原会话原样保留）。成功路径随即整体替换，此处的工具清单
        // 调整随候选会话一起被覆盖。
        self.ensure_plan_mcp_shutdown(cancellation)?;
        // 完整提交后才替换当前会话；只有真正切换才清除撤销内存资格。
        self.session = session;
        self.client = client;
        self.current_model = crate::config::ModelChoice {
            reference: selection.model,
            model: config.model,
            protocol: config.protocol,
            thinking: config.thinking,
            context_window: config.context_window,
        };
        self.think_level = selection.effort;
        self.journal = journal;
        self.workflow = candidate_workflow;
        self.tools.clear_changes();
        self.last_request = None;
        self.pending_instructions = None;
        drop(verified);
        // Plan 候选未连接任何 MCP 服务：清理句柄是空快照，Drop 无副作用。
        let cleanup = self.tools.mcp.cleanup()?;
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: Some(Vec::new()),
            cleanup,
            todo_snapshot: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::{EXPLORATION, err_message, runtime_input_agent, seed_session};
    use crate::tools::plan_catalog::{
        PlanCatalogEntry, PlanCatalogLock, load_catalog, save_catalog,
    };
    use std::os::unix::fs::PermissionsExt;

    const LISTED_CONTENT: &str =
        "# Listed plan\n\n- [ ] First canary task.\n- [ ] Second canary task.\n";
    const LISTED_FILE: &str = "continue-listed.md";
    const LISTED_ID: &str = "0123456789abcdef0123456789abcdef";

    fn listed_path() -> String {
        plan::path_for(LISTED_FILE, PlanLocation::Drafts)
    }

    fn write_plan_bytes(agent: &Agent, location: PlanLocation, file_name: &str, content: &str) {
        let path = agent.tools.cwd().join(plan::path_for(file_name, location));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn write_listed_file(agent: &Agent) {
        write_plan_bytes(agent, PlanLocation::Drafts, LISTED_FILE, LISTED_CONTENT);
    }

    fn register_entry(agent: &Agent, id: &str, file_name: &str, location: PlanLocation) {
        let workspace = agent.tools.plan_workspace();
        let _lock = PlanCatalogLock::acquire(workspace).unwrap();
        let snapshot = load_catalog(workspace).unwrap();
        let mut catalog = snapshot.catalog;
        catalog.entries.push(PlanCatalogEntry {
            id: id.to_owned(),
            file_name: file_name.to_owned(),
            location,
            deleted: false,
            deleted_file_identity: None,
        });
        save_catalog(workspace, snapshot.raw.as_deref(), &catalog).unwrap();
    }

    fn register_listed_entry(agent: &Agent) {
        register_entry(agent, LISTED_ID, LISTED_FILE, PlanLocation::Drafts);
    }

    fn selection() -> PlanSelection {
        PlanSelection {
            id: LISTED_ID.to_owned(),
            path: listed_path(),
            content: LISTED_CONTENT.to_owned(),
        }
    }

    /// 与 plan_workflow 测试相同的原生用户文本提取，用于断言候选会话内容。
    fn user_texts(agent: &Agent) -> Vec<String> {
        let native = serde_json::to_value(&agent.session).unwrap();
        let request = &native["request"];
        let entries = match request["messages"].as_array() {
            Some(messages) => messages.clone(),
            None => request["input"].as_array().unwrap().clone(),
        };
        entries
            .iter()
            .filter(|entry| entry["role"] == "user")
            .map(|entry| match &entry["content"] {
                serde_json::Value::String(text) => text.clone(),
                parts => parts
                    .as_array()
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|part| part["text"].as_str())
                            .collect::<String>()
                    })
                    .unwrap_or_default(),
            })
            .collect()
    }

    fn native_entry_count(agent: &Agent) -> usize {
        let request = &serde_json::to_value(&agent.session).unwrap()["request"];
        request["messages"]
            .as_array()
            .or_else(|| request["input"].as_array())
            .unwrap()
            .len()
    }

    #[test]
    fn open_listed_plan_starts_fresh_context_without_old_history() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, EXPLORATION);
            write_listed_file(&agent);
            register_listed_entry(&agent);
            let selection = selection();
            let preparation = agent
                .open_listed_plan(
                    &selection,
                    PlanOpenIntent::ContinueDraft,
                    &CancellationToken::new(),
                )
                .unwrap();
            // 候选 workflow：Plan/Draft，同 ID/名称，revision=1，无批准冻结。
            let document = agent.workflow.plan.as_ref().unwrap();
            assert_eq!(agent.workflow.mode, WorkMode::Plan);
            assert_eq!(document.status, PlanStatus::Draft);
            assert_eq!(document.id, LISTED_ID);
            assert_eq!(document.file_name.as_deref(), Some(LISTED_FILE));
            assert_eq!(document.location, PlanLocation::Drafts);
            assert_eq!(document.revision, 1);
            assert_eq!(document.content, LISTED_CONTENT);
            assert_eq!(document.approved_tasks, None);
            assert_eq!(document.title, "Listed plan");
            assert_eq!(
                document.origin_prompt,
                format!(
                    "Continue planning the selected Markdown plan at {}.",
                    listed_path()
                )
            );
            // 发布产物：replay 为空、session_id 指向候选会话。
            assert!(matches!(&preparation.replay, Some(replay) if replay.is_empty()));
            assert_eq!(preparation.session_id.as_deref(), agent.journal.id());
            assert_ne!(preparation.session_id.as_deref(), None);
            assert_eq!(preparation.workflow, agent.workflow);
            // 候选会话不含旧聊天：唯一用户消息携带选择动作、ID 与完整快照，
            // 原会话的探索文本绝不出现；打开动作从未产生助手回复或请求。
            let texts = user_texts(&agent);
            assert_eq!(texts.len(), 1);
            assert!(texts[0].contains("Continue planning the selected Markdown plan below."));
            assert!(texts[0].contains(LISTED_ID));
            assert!(texts[0].contains(&format!("Path: {}", listed_path())));
            assert!(texts[0].contains(LISTED_CONTENT));
            assert!(!texts[0].contains(EXPLORATION));
            assert_eq!(
                native_entry_count(&agent),
                if protocol == "completion" { 2 } else { 1 }
            );
            // 原会话文件保留：候选 fork 产生第二个会话记录。
            let sessions = std::fs::read_dir(dir.path().join("sessions"))
                .unwrap()
                .count();
            assert!(sessions >= 2, "source session must remain restorable");
        }
    }

    #[test]
    fn todo_open_listed_plan_defers_tasks_until_approval() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, EXPLORATION);
            write_listed_file(&agent);
            register_listed_entry(&agent);
            let selection = selection();
            let preparation = agent
                .open_listed_plan(
                    &selection,
                    PlanOpenIntent::Execute,
                    &CancellationToken::new(),
                )
                .unwrap();
            assert!(agent.workflow.todos.is_empty());
            assert!(agent.workflow.todo_plan_id.is_none());
            assert!(preparation.todo_snapshot.is_none());
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().content,
                selection.content
            );
        }
    }

    #[test]
    fn open_listed_plan_prepare_failure_keeps_the_original_session() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, EXPLORATION);
            write_listed_file(&agent);
            register_listed_entry(&agent);
            let selection = selection();
            let previous_session = serde_json::to_value(&agent.session).unwrap();
            let previous_workflow = agent.workflow.clone();
            // 项目规则读取拒绝 symlink：候选准备在任何写入前失败。
            let target = dir.path().join("agents-source.md");
            std::fs::write(&target, "rules\n").unwrap();
            std::os::unix::fs::symlink(&target, dir.path().join("AGENTS.md")).unwrap();
            let error = err_message(agent.open_listed_plan(
                &selection,
                PlanOpenIntent::ContinueDraft,
                &CancellationToken::new(),
            ));
            assert!(!error.is_empty());
            // 原会话/模式/输入与计划文件全部保持不变。
            assert_eq!(
                serde_json::to_value(&agent.session).unwrap(),
                previous_session
            );
            assert_eq!(agent.workflow, previous_workflow);
            assert_eq!(
                std::fs::read_to_string(dir.path().join(listed_path())).unwrap(),
                LISTED_CONTENT
            );
        }
    }

    #[test]
    fn open_listed_plan_commit_failure_keeps_the_original_session() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, EXPLORATION);
            write_listed_file(&agent);
            register_listed_entry(&agent);
            let selection = selection();
            let previous_session = serde_json::to_value(&agent.session).unwrap();
            let previous_workflow = agent.workflow.clone();
            // sessions 目录只读：候选 journal 首次建文件失败。特权环境可能
            // 绕过权限位，此时跳过该失败注入。
            let sessions = dir.path().join("sessions");
            let original = std::fs::metadata(&sessions).unwrap().permissions();
            std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o555)).unwrap();
            let probe = sessions.join("probe.jsonl");
            let enforced = std::fs::write(&probe, b"probe").is_err();
            if enforced {
                let _ = std::fs::remove_file(&probe);
                let error = err_message(agent.open_listed_plan(
                    &selection,
                    PlanOpenIntent::ContinueDraft,
                    &CancellationToken::new(),
                ));
                assert!(!error.is_empty());
                assert_eq!(
                    serde_json::to_value(&agent.session).unwrap(),
                    previous_session
                );
                assert_eq!(agent.workflow, previous_workflow);
            }
            std::fs::set_permissions(&sessions, original).unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.path().join(listed_path())).unwrap(),
                LISTED_CONTENT
            );
        }
    }

    #[test]
    fn execute_intent_opens_review_boundary_requiring_local_approval() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, EXPLORATION);
            write_listed_file(&agent);
            register_listed_entry(&agent);
            let selection = selection();
            let preparation = agent
                .open_listed_plan(
                    &selection,
                    PlanOpenIntent::Execute,
                    &CancellationToken::new(),
                )
                .unwrap();
            // Plan/Ready 预执行审阅状态：validate 显式允许；未批准、无报告。
            let document = agent.workflow.plan.as_ref().unwrap();
            assert_eq!(agent.workflow.mode, WorkMode::Plan);
            assert_eq!(document.status, PlanStatus::Ready);
            assert_eq!(document.location, PlanLocation::Drafts);
            assert_eq!(document.approved_tasks, None);
            assert!(agent.workflow.review.is_none());
            assert_eq!(
                document.origin_prompt,
                format!(
                    "Execute the selected Markdown plan at {} after local approval.",
                    listed_path()
                )
            );
            assert!(matches!(&preparation.replay, Some(replay) if replay.is_empty()));
            // 审阅 token 由 read_plan 在新会话上补发，绑定候选会话 ID。
            let review = agent.read_plan().unwrap();
            assert_eq!(review.token.session_id, agent.journal.id().unwrap());
            assert_eq!(review.token.plan_id, LISTED_ID);
            assert_eq!(review.token.content, LISTED_CONTENT);
            // 无审批就没有执行：候选会话没有助手回复，也不连接 MCP。
            assert_eq!(user_texts(&agent).len(), 1);
            let request = serde_json::to_value(&agent.session).unwrap()["request"].clone();
            let tools = request["tools"].as_array().unwrap();
            let names: Vec<&str> = tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().or(tool["name"].as_str()))
                .collect();
            assert!(names.contains(&"plan_write"));
            assert!(!names.contains(&"bash"));
        }
    }

    #[test]
    fn done_plans_refuse_continue_and_execute_intents() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
        write_plan_bytes(&agent, PlanLocation::Done, LISTED_FILE, LISTED_CONTENT);
        register_entry(&agent, LISTED_ID, LISTED_FILE, PlanLocation::Done);
        let selection = PlanSelection {
            id: LISTED_ID.to_owned(),
            path: plan::path_for(LISTED_FILE, PlanLocation::Done),
            content: LISTED_CONTENT.to_owned(),
        };
        for intent in [PlanOpenIntent::ContinueDraft, PlanOpenIntent::Execute] {
            let error =
                err_message(agent.open_listed_plan(&selection, intent, &CancellationToken::new()));
            assert!(error.contains("cannot be opened"), "{error}");
        }
        // 失败不产生任何会话或绑定变化。
        assert!(agent.workflow.plan.is_none());
        assert!(agent.journal.id().is_none());
    }

    #[test]
    fn execute_intent_opens_prose_as_draft_and_unsaved_draft_blocks_switch() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
        // 无任务正文也可打开菜单，但保持 Draft，执行仍被审批门禁拒绝。
        write_listed_file(&agent);
        std::fs::write(
            dir.path().join(listed_path()),
            "# No tasks\n\nJust prose.\n",
        )
        .unwrap();
        register_listed_entry(&agent);
        let no_task_selection = PlanSelection {
            id: LISTED_ID.to_owned(),
            path: listed_path(),
            content: "# No tasks\n\nJust prose.\n".to_owned(),
        };
        let preparation = agent
            .open_listed_plan(
                &no_task_selection,
                PlanOpenIntent::Execute,
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(
            preparation.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Draft
        );
        assert_eq!(preparation.workflow.mode, WorkMode::Plan);
        // 未首存的 revision-0 草稿拒绝“另开会话”，避免静默丢弃。
        let mut document = PlanDocument::new().unwrap();
        document.origin_prompt = "pending first save".to_owned();
        agent.workflow = WorkflowState {
            mode: WorkMode::Plan,
            plan: Some(document),
            ..Default::default()
        };
        let selection = selection();
        let error = err_message(agent.open_listed_plan(
            &selection,
            PlanOpenIntent::ContinueDraft,
            &CancellationToken::new(),
        ));
        assert_eq!(error, UNSAVED_DRAFT);
        // 未登记的选择被拒绝：绝不按列表下标直接操作磁盘。
        agent.workflow = WorkflowState::default();
        let unregistered = PlanSelection {
            id: "ffffffffffffffffffffffffffffffff".to_owned(),
            path: listed_path(),
            content: LISTED_CONTENT.to_owned(),
        };
        let error = err_message(agent.open_listed_plan(
            &unregistered,
            PlanOpenIntent::ContinueDraft,
            &CancellationToken::new(),
        ));
        assert!(!error.is_empty());
    }
}
