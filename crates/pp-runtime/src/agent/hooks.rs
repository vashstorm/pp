//! Agent 侧的 hook 接线：资格判定、身份信封与决定落地。
//!
//! 只有 Execute 且非只读的路径拥有事件：根 Agent 拿到五个事件，可写子 Agent
//! 只有三个工具事件；Plan／Review／只读子 Agent、人工 shell、标题与压缩请求
//! 一律零事件、零进程。

use super::persistence::{Metadata, SessionJournal, SessionRef};
use crate::hooks::protocol::{Context, Payload};
use crate::hooks::{HookEffect, Runner};
use anyhow::Result;
use pp_config::config::{ApprovalMode, HookEvent};
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
    pub(crate) permission_mode: &'static str,
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
    approval: &crate::tools::approval::Gate,
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
        permission_mode: permission_mode(approval.effective_mode()),
        agent_type: agent_id.as_ref().and(agent_type.map(str::to_owned)),
        agent_id,
    }
}

fn permission_mode(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Yolo => "bypassPermissions",
        ApprovalMode::Write => "acceptEdits",
        ApprovalMode::AlwaysAsk => "default",
    }
}

/// 已建立有效 session 的信封来源；构造后可按事件重复执行。
pub(crate) struct HookSession {
    scope: HookScope,
    session_id: String,
    transcript: PathBuf,
    cwd: PathBuf,
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
        let (session_id, transcript) = journal.prepare_hooks(metadata, session, workflow)?;
        Ok(Self {
            scope,
            session_id,
            transcript,
            cwd: cwd.to_owned(),
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
        let context = Context {
            session_id: &self.session_id,
            transcript_path: &self.transcript,
            cwd: &self.cwd,
            permission_mode: self.scope.permission_mode,
            agent_id: self.scope.agent_id.as_deref(),
            agent_type: self.scope.agent_type.as_deref(),
        };
        let input = crate::hooks::protocol::input(event, &context, payload);
        self.scope.runner.run(event, &input, cancellation, emit)
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

/// 必须中止本批的行为：`continue:false` 或无法放行的 hook 故障。
pub(crate) enum HookHalt {
    Stop(String),
    Failure(anyhow::Error),
}

pub(crate) struct PreparedBatch {
    pub(crate) calls: Vec<PreparedCall>,
    pub(crate) halt: Option<HookHalt>,
}

/// 单次工具执行携带的 hook 上下文。
pub(crate) struct ToolHookCall<'a> {
    pub(crate) session: &'a HookSession,
    pub(crate) call: &'a PreparedCall,
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
                Err(error) if error.is::<Cancelled>() || error.is::<CleanupError>() => {
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
            self.tools.subagent.as_ref(),
            &self.workflow,
            &self.tools.approval,
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
