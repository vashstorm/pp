//! Agent 侧的 hook 接线：资格判定、身份信封与决定落地。
//!
//! 只有 Execute 且非只读的路径拥有事件：根 Agent 拿到五个事件，可写子 Agent
//! 只有三个工具事件；Plan／Review／只读子 Agent、人工 shell、标题与压缩请求
//! 一律零事件、零进程。

use super::persistence::{Metadata, SessionJournal, SessionRef};
use crate::hooks::protocol::{Context, Payload};
use crate::hooks::{HookEffect, Runner};
use anyhow::Result;
use pp_config::config::{HookEvent, PermissionMode};
use pp_core::cancellation::{CancellationToken, Cancelled, CleanupError};
use pp_core::events::AgentEvent;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HookRole {
    Root,
    Child,
}

/// 一次运行内的静态资格与身份。全部拥有值（runner 为 `Arc` 克隆），
/// 这样 turn 循环可以同时可变借用 Agent 的其它字段。
#[derive(Clone)]
pub(crate) struct HookScope {
    pub(crate) runner: std::sync::Arc<Runner>,
    pub(crate) role: Option<HookRole>,
    pub(crate) permission: pp_tools::permission::Gate,
    pub(crate) agent_id: Option<String>,
    pub(crate) agent_type: Option<String>,
}

impl HookScope {
    /// 事件资格表：根 Agent 五个事件，可写子 Agent 只有工具事件。
    pub(crate) fn enabled(&self, event: HookEvent) -> bool {
        match self.role {
            None => false,
            Some(HookRole::Root) => self.runner.has_handlers(event),
            Some(HookRole::Child) => event.is_tool_event() && self.runner.has_handlers(event),
        }
    }

    /// 工具事件是否可能触发；用于跳过整段准备与参数克隆。
    pub(crate) fn tool_events(&self) -> bool {
        self.enabled(HookEvent::PreToolUse)
            || self.enabled(HookEvent::PostToolUse)
            || self.enabled(HookEvent::PostToolUseFailure)
    }
}

/// 按当前路径资格组装 scope；`binding` 为根绑定时视为根 Agent。
pub(crate) fn scope(
    runner: &std::sync::Arc<Runner>,
    binding: Option<&crate::subagent::Binding>,
    workflow: &pp_core::plan::WorkflowState,
    permission: &pp_tools::permission::Gate,
    agent_type: Option<&str>,
) -> HookScope {
    let execute = workflow.mode == pp_core::plan::WorkMode::Execute;
    let (role, agent_id) = match binding {
        Some(binding) if binding.depth > 0 => {
            let writable = !binding.readonly && !binding.plan_mode();
            (
                writable.then_some(HookRole::Child),
                Some(binding.owner.clone()),
            )
        }
        _ => (Some(HookRole::Root), None),
    };
    HookScope {
        runner: runner.clone(),
        role: role.filter(|_| execute),
        permission: permission.clone(),
        agent_type: agent_id.as_ref().and(agent_type.map(str::to_owned)),
        agent_id,
    }
}

fn permission_mode(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Yolo => "bypassPermissions",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::AlwaysAsk => "default",
    }
}

/// 已建立有效 session 的信封来源；构造后可按事件重复执行。
pub(crate) struct HookSession {
    scope: HookScope,
    session_id: String,
    transcript: PathBuf,
    cwd: PathBuf,
    stats: pp_stats::Database,
}

impl HookSession {
    /// 只有确实要运行 hook 时才调用：首次建立有效的 header／state。
    pub(in crate::agent) fn prepare(
        scope: HookScope,
        journal: &mut SessionJournal,
        metadata: &Metadata<'_>,
        session: SessionRef<'_>,
        workflow: &pp_core::plan::WorkflowState,
        cwd: &Path,
    ) -> Result<Self> {
        let stats = journal.stats_handle().clone();
        let (session_id, transcript) = journal.prepare_hooks(metadata, session, workflow)?;
        Ok(Self {
            scope,
            session_id,
            transcript,
            cwd: cwd.to_owned(),
            stats,
        })
    }

    pub(crate) fn enabled(&self, event: HookEvent) -> bool {
        self.scope.enabled(event)
    }

    pub(crate) fn run(
        &self,
        event: HookEvent,
        payload: &Payload<'_>,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<HookEffect> {
        let permission_mode = permission_mode(self.scope.permission.snapshot());
        let context = Context {
            session_id: &self.session_id,
            transcript_path: &self.transcript,
            cwd: &self.cwd,
            permission_mode,
            agent_id: self.scope.agent_id.as_deref(),
            agent_type: self.scope.agent_type.as_deref(),
        };
        let input = crate::hooks::protocol::input(event, &context, payload);
        self.scope
            .runner
            .run_with_stats(event, &input, Some(&self.stats), cancellation, emit)
    }
}

/// 整批预检之后、执行之前保存的逐调用决定。
#[derive(Default)]
pub(crate) struct PreparedCall {
    /// 事件级拒绝：该调用不执行，但仍产生一条 `ok:false` 结果。
    pub(crate) deny_reason: Option<String>,
    /// 单次强制审批理由，强于 allow，也不能跳过原审批。
    pub(crate) ask_reason: Option<String>,
    pub(crate) context: Vec<String>,
    /// 最后一次有效 `updatedInput` 的完整参数；写回原生调用对象。
    pub(crate) updated_arguments: Option<String>,
}

pub(crate) use pp_tools::HookHalt;

pub(crate) struct PreparedBatch {
    pub(crate) calls: Vec<PreparedCall>,
    pub(crate) halt: Option<HookHalt>,
}

/// 单次工具执行携带的 hook 上下文。
pub(crate) struct ToolHookCall<'a> {
    pub(crate) session: &'a HookSession,
    pub(crate) call: &'a PreparedCall,
}

impl pp_tools::ToolHookCall for ToolHookCall<'_> {
    fn deny_reason(&self) -> Option<&str> {
        self.call.deny_reason.as_deref()
    }

    fn ask_reason(&self) -> Option<&str> {
        self.call.ask_reason.as_deref()
    }

    fn pre_context(&self) -> &[String] {
        &self.call.context
    }

    fn run_post_tool(
        &self,
        call: pp_tools::PostToolCall<'_>,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Option<pp_tools::PostToolOutcome>> {
        let event = if call.is_error {
            HookEvent::PostToolUseFailure
        } else {
            HookEvent::PostToolUse
        };
        if !self.session.enabled(event) {
            return Ok(None);
        }
        let error = call
            .value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // 参数不是 JSON 对象时 tool_input 为 null，不回显原文。
        let input: Option<Value> = serde_json::from_str(call.arguments)
            .ok()
            .filter(Value::is_object);
        let payload = if event == HookEvent::PostToolUse {
            Payload::PostTool {
                name: call.name,
                input: input.as_ref(),
                call_id: call.call_id,
                response: call.value,
            }
        } else {
            Payload::PostToolFailure {
                name: call.name,
                input: input.as_ref(),
                call_id: call.call_id,
                error,
            }
        };
        let effect = self.session.run(event, &payload, cancellation, emit)?;
        Ok(Some(pp_tools::PostToolOutcome {
            additional_context: effect.additional_context,
            updated_output: effect.updated_output,
            stop_reason: effect.stop_reason,
        }))
    }

    fn run_pre_nested_tool(
        &self,
        name: &str,
        call_id: &str,
        arguments: &str,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Option<pp_tools::PreToolOutcome>> {
        if !self.session.enabled(HookEvent::PreToolUse) {
            return Ok(None);
        }
        let input = serde_json::from_str::<Value>(arguments)
            .ok()
            .filter(Value::is_object);
        let Some(input) = input else {
            return Ok(None);
        };
        let effect = self.session.run(
            HookEvent::PreToolUse,
            &Payload::PreTool {
                name,
                input: Some(&input),
                call_id,
            },
            cancellation,
            emit,
        )?;
        let updated_arguments = effect
            .updated_input
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        Ok(Some(pp_tools::PreToolOutcome {
            deny_reason: effect.deny_reason,
            ask_reason: effect.ask_reason,
            additional_context: effect.additional_context,
            updated_arguments,
            stop_reason: effect.stop_reason,
        }))
    }
}

/// 执行器一侧可用的最简调用描述：名称、call ID 与原始参数。
pub(crate) struct BatchCall<'a> {
    pub(crate) name: &'a str,
    pub(crate) call_id: &'a str,
    pub(crate) arguments: &'a str,
}

/// 按 call 顺序为整批运行 PreToolUse；不做任何工具调用。
pub(crate) fn prepare_batch(
    session: &HookSession,
    calls: &[BatchCall<'_>],
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<PreparedBatch> {
    let mut prepared = Vec::with_capacity(calls.len());
    let mut halt = None;
    for call in calls {
        let mut call_state = PreparedCall::default();
        // 参数不是 JSON 对象时不运行 pre：交给原工具错误路径，不修复、不回显原文。
        let input = serde_json::from_str::<Value>(call.arguments)
            .ok()
            .filter(Value::is_object);
        if let Some(input) = input.filter(|_| session.enabled(HookEvent::PreToolUse)) {
            let effect = session.run(
                HookEvent::PreToolUse,
                &Payload::PreTool {
                    name: call.name,
                    input: Some(&input),
                    call_id: call.call_id,
                },
                cancellation,
                emit,
            );
            let effect = match effect {
                Ok(effect) => effect,
                Err(error)
                    if error
                        .chain()
                        .any(|cause| cause.is::<Cancelled>() || cause.is::<CleanupError>()) =>
                {
                    return Err(error);
                }
                Err(error) => {
                    halt = Some(HookHalt::Failure(error));
                    prepared.push(call_state);
                    break;
                }
            };
            if let Some(reason) = effect.stop_reason {
                halt = Some(HookHalt::Stop(reason));
                prepared.push(call_state);
                break;
            }
            if let Some(reason) = effect.deny_reason {
                // 拒绝时丢弃该调用已收集的上下文。
                call_state.deny_reason = Some(reason);
                prepared.push(call_state);
                continue;
            }
            call_state.ask_reason = effect.ask_reason;
            call_state.context = effect.additional_context;
            if let Some(updated) = effect.updated_input {
                call_state.updated_arguments = Some(serde_json::to_string(&updated)?);
            }
        }
        prepared.push(call_state);
    }
    // 中止时后续调用仍需要一条结果，保持 call ID 一一对应。
    while prepared.len() < calls.len() {
        prepared.push(PreparedCall::default());
    }
    Ok(PreparedBatch {
        calls: prepared,
        halt,
    })
}

impl super::Agent {
    /// 提交前的输入检查；没有匹配 handler 或当前路径无资格时为 `None`。
    pub(in crate::agent) fn hook_user_input(
        &mut self,
        prompt: &str,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Option<HookEffect>> {
        let session = self.prepare_hook_session(HookEvent::UserPromptSubmit)?;
        let Some(session) = session else {
            return Ok(None);
        };
        let effect = session.run(
            HookEvent::UserPromptSubmit,
            &Payload::UserPrompt { prompt },
            cancellation,
            emit,
        )?;
        Ok(Some(effect))
    }

    /// 自然完成候选上的 Stop 检查；只有根 Execute 路径有资格。
    pub(in crate::agent) fn hook_stop(
        &mut self,
        last_assistant_message: &str,
        stop_hook_active: bool,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Option<HookEffect>> {
        let session = self.prepare_hook_session(HookEvent::Stop)?;
        let Some(session) = session else {
            return Ok(None);
        };
        let effect = session.run(
            HookEvent::Stop,
            &Payload::Stop {
                stop_hook_active,
                last_assistant_message,
            },
            cancellation,
            emit,
        )?;
        Ok(Some(effect))
    }

    /// 需要运行该事件时才建立 session；否则不分配任何进程与日志。
    pub(in crate::agent) fn prepare_hook_session(
        &mut self,
        event: HookEvent,
    ) -> Result<Option<HookSession>> {
        // 只借用 `hooks` 字段：身份与资格在 scope 构造时固定。
        let scope = scope(
            &self.hooks,
            self.subagent.as_ref(),
            &self.workflow,
            &self.tools.permission,
            self.hook_agent_type.as_deref(),
        );
        if !scope.enabled(event) {
            return Ok(None);
        }
        let metadata = Metadata {
            role_route: self.role_route.as_ref(),
            cwd: self.tools.cwd(),
            model_reference: &self.current_model.reference,
            think_level: self.think_level,
            edit_mode: self.tools.edit_mode(),
            tool_limits: self.tools.limits,
            base_system: &self.base_system,
            prompt_source: self.prompt_source.as_ref(),
        };
        let session_ref = SessionRef::from(&self.session);
        let session = HookSession::prepare(
            scope,
            &mut self.journal,
            &metadata,
            session_ref,
            &self.workflow,
            self.tools.cwd(),
        )?;
        Ok(Some(session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::chat::CreateChatCompletionRequest;
    use pp_config::config::{
        CommandHook, EditMode, HookFailurePolicy, HookHandlerType, HookMatcherGroup, HooksSettings,
        ToolLimits,
    };
    use pp_core::model::ThinkLevel;
    use pp_core::plan::WorkflowState;
    use serde_json::json;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/bash\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[test]
    fn hook_session_records_events_statuses_modifications_and_tool_names_to_stats_db() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let workdir = dir.path().join("workspace");
        std::fs::create_dir_all(&workdir).unwrap();

        let prompt_ctx = script(
            dir.path(),
            "prompt.sh",
            r#"printf '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"secret-context"}}'"#,
        );
        let pre_allow_rewrite = script(
            dir.path(),
            "pre-allow.sh",
            r#"printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"path":"rewritten.txt"}}}'"#,
        );
        let pre_deny = script(
            dir.path(),
            "pre-deny.sh",
            r#"printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"nope"}}'"#,
        );
        let post_err = script(dir.path(), "post-err.sh", "echo private-stderr >&2; exit 1");

        let mut settings = HooksSettings::default();
        settings.user_prompt_submit.push(HookMatcherGroup {
            matcher: None,
            hooks: vec![CommandHook {
                kind: HookHandlerType::Command,
                command: prompt_ctx.display().to_string(),
                args: None,
                timeout: 10,
                on_failure: None,
            }],
        });
        settings.pre_tool_use.push(HookMatcherGroup {
            matcher: Some("read".into()),
            hooks: vec![CommandHook {
                kind: HookHandlerType::Command,
                command: pre_allow_rewrite.display().to_string(),
                args: None,
                timeout: 10,
                on_failure: None,
            }],
        });
        settings.pre_tool_use.push(HookMatcherGroup {
            matcher: Some("bash".into()),
            hooks: vec![CommandHook {
                kind: HookHandlerType::Command,
                command: pre_deny.display().to_string(),
                args: None,
                timeout: 10,
                on_failure: None,
            }],
        });
        settings.post_tool_use.push(HookMatcherGroup {
            matcher: None,
            hooks: vec![CommandHook {
                kind: HookHandlerType::Command,
                command: post_err.display().to_string(),
                args: None,
                timeout: 10,
                on_failure: Some(HookFailurePolicy::Continue),
            }],
        });

        let runner = std::sync::Arc::new(Runner::compile(&settings).unwrap());
        let permission = pp_tools::permission::Gate::new();
        permission.select(Some(PermissionMode::AlwaysAsk));
        let hook_scope = HookScope {
            runner,
            role: Some(HookRole::Root),
            permission,
            agent_id: None,
            agent_type: None,
        };
        let mut journal = SessionJournal::new(sessions.clone());
        let request = CreateChatCompletionRequest::default();
        let metadata = Metadata {
            role_route: None,
            cwd: &workdir,
            model_reference: "test/model",
            think_level: ThinkLevel::None,
            edit_mode: EditMode::Replace,
            tool_limits: ToolLimits::default(),
            base_system: "",
            prompt_source: None,
        };
        let workflow = WorkflowState::default();
        let session = HookSession::prepare(
            hook_scope,
            &mut journal,
            &metadata,
            SessionRef::Completion(&request),
            &workflow,
            &workdir,
        )
        .unwrap();

        let cancel = CancellationToken::new();
        let mut emit = |_| {};
        let prompt_effect = session
            .run(
                HookEvent::UserPromptSubmit,
                &Payload::UserPrompt { prompt: "hello" },
                &cancel,
                &mut emit,
            )
            .unwrap();
        assert_eq!(prompt_effect.additional_context, vec!["secret-context"]);

        let read_input = json!({"path": "a.txt"});
        let pre_read = session
            .run(
                HookEvent::PreToolUse,
                &Payload::PreTool {
                    name: "read",
                    input: Some(&read_input),
                    call_id: "c-1",
                },
                &cancel,
                &mut emit,
            )
            .unwrap();
        assert!(pre_read.updated_input.is_some());

        let bash_input = json!({"command": "ls"});
        let pre_bash = session
            .run(
                HookEvent::PreToolUse,
                &Payload::PreTool {
                    name: "bash",
                    input: Some(&bash_input),
                    call_id: "c-2",
                },
                &cancel,
                &mut emit,
            )
            .unwrap();
        assert_eq!(pre_bash.deny_reason.as_deref(), Some("nope"));

        let post_resp = json!({"ok": true});
        session
            .run(
                HookEvent::PostToolUse,
                &Payload::PostTool {
                    name: "read",
                    input: Some(&read_input),
                    call_id: "c-1",
                    response: &post_resp,
                },
                &cancel,
                &mut emit,
            )
            .unwrap();

        let conn = rusqlite::Connection::open(sessions.join(".stats/stats.db")).unwrap();
        let rows: Vec<(String, Option<String>, String, i64, String, String)> = conn
            .prepare(
                "SELECT event,tool,status,modified,cwd,session_file FROM hook_runs ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].0, "UserPromptSubmit");
        assert_eq!(rows[0].1, None);
        assert_eq!(rows[0].2, "ok");
        assert_eq!(rows[0].3, 1);
        assert_eq!(rows[1].0, "PreToolUse");
        assert_eq!(rows[1].1.as_deref(), Some("read"));
        assert_eq!(rows[1].2, "ok");
        assert_eq!(rows[1].3, 1);
        assert_eq!(rows[2].0, "PreToolUse");
        assert_eq!(rows[2].1.as_deref(), Some("bash"));
        assert_eq!(rows[2].2, "blocked");
        assert_eq!(rows[2].3, 0);
        assert_eq!(rows[3].0, "PostToolUse");
        assert_eq!(rows[3].1.as_deref(), Some("read"));
        assert_eq!(rows[3].2, "error");
        assert_eq!(rows[3].3, 0);
        for row in &rows {
            assert_eq!(row.4, workdir.to_string_lossy());
            assert!(row.5.ends_with(".jsonl"));
        }
        // 不记录脚本路径与 stderr 正文。
        let dump: String = rows
            .iter()
            .map(|r| format!("{r:?}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!dump.contains("private-stderr"));
        assert!(!dump.contains("post-err.sh"));
    }
}
