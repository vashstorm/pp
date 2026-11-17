//! 当前会话上下文的只读诊断快照：严格基于已发送/已恢复的真实会话数据，
//! 按 10 个固定分类拆分统计：`System Prompt`、`Instruction Files`、`Skills`、`Built-in Tools`、
//! `Custom Tools`、`User / Developer Messages`、`Assistant Messages`、`Assistant Thinking`、
//! `Tool Calls` 与 `Tool Output`。

mod history;
mod sections;
mod snapshot;
#[cfg(test)]
mod tests;
mod thinking;

pub use snapshot::Snapshot;

pub(crate) use thinking::{ThinkingEntry, extract_thinking_from_replay, trim_for_retained_turns};

use super::{Agent, Session};
use anyhow::{Result, anyhow};
use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageContent,
};
use pp_core::{context::TokenUsage, model::ThinkLevel};
use sections::{
    build_agent_md_items, build_skill_items, build_split_tool_items, build_system_items,
};
use thinking::build_thinking_items;

const MAX_PREVIEW_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CategoryKey {
    System,
    InstructionFiles,
    Skills,
    BuiltInTools,
    CustomTools,
    UserMessages,
    AssistantMessages,
    Thinking,
    ToolCalls,
    ToolOutput,
}

impl CategoryKey {
    pub const ALL: [Self; 10] = [
        Self::System,
        Self::InstructionFiles,
        Self::Skills,
        Self::BuiltInTools,
        Self::CustomTools,
        Self::UserMessages,
        Self::AssistantMessages,
        Self::Thinking,
        Self::ToolCalls,
        Self::ToolOutput,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "System Prompt",
            Self::InstructionFiles => "Instruction Files",
            Self::Skills => "Skills",
            Self::BuiltInTools => "Built-in Tools",
            Self::CustomTools => "Custom Tools",
            Self::UserMessages => "User / Developer Messages",
            Self::AssistantMessages => "Assistant Messages",
            Self::Thinking => "Assistant Thinking",
            Self::ToolCalls => "Tool Calls",
            Self::ToolOutput => "Tool Output",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ContextReport {
    pub model_reference: String,
    pub model_name: String,
    pub protocol: &'static str,
    pub work_mode: &'static str,
    pub think_level: ThinkLevel,
    pub empty_session: bool,
    pub active_tokens: u64,
    pub context_window: Option<u64>,
    pub auto_compact: bool,
    pub threshold_percent: u8,
    pub last_request: Option<TokenUsage>,
    pub session_output_tokens: u64,
    pub thinking_tokens: u64,
    pub thinking_in_context: bool,
    pub thinking_turns: usize,
    pub categories: Vec<CategorySummary>,
}

#[derive(Clone, Debug)]
pub struct CategorySummary {
    pub key: CategoryKey,
    pub name: &'static str,
    pub tokens: u64,
    pub bytes: u64,
    pub item_count: usize,
    /// 为 true 表示计入下一次请求上下文窗口；为 false 表示历史产出（如 Chat Completions 思考）。
    pub in_context: bool,
    pub items: Vec<DiagnosticItem>,
}

#[derive(Clone, Debug)]
pub struct DiagnosticItem {
    pub kind_label: String,
    pub title: String,
    pub subtitle: String,
    pub tokens: u64,
    pub bytes: u64,
    pub preview: String,
}

impl Agent {
    /// 生成当前会话上下文的只读诊断报告：
    /// 仅基于当前会话已发送/已恢复的真实数据统计；
    /// 尚未开始对话的空会话返回零用量及全零分类，不把未发送的配置基线计入上下文，也不创建会话日志。
    pub fn diagnose_context(&self) -> Result<ContextReport> {
        let usage = self.context_usage()?;
        let empty_session = usage.estimated_tokens == 0;
        let protocol = match &self.session {
            Session::Completion(_) => "completion",
            Session::Response(_) => "response",
        };
        let thinking_in_context = matches!(&self.session, Session::Response(_));

        if empty_session {
            let categories = CategoryKey::ALL
                .into_iter()
                .map(|key| CategorySummary {
                    key,
                    name: key.label(),
                    tokens: 0,
                    bytes: 0,
                    item_count: 0,
                    in_context: key != CategoryKey::Thinking || thinking_in_context,
                    items: Vec::new(),
                })
                .collect();

            return Ok(ContextReport {
                model_reference: self.current_model.reference.clone(),
                model_name: self.current_model.model.clone(),
                protocol,
                work_mode: self.workflow.mode_name(),
                think_level: self.think_level,
                empty_session: true,
                active_tokens: 0,
                context_window: self.current_model.context_window,
                auto_compact: self.auto_compact,
                threshold_percent: self.auto_compact_threshold_percent,
                last_request: self.last_request,
                session_output_tokens: self.session_output_tokens,
                thinking_tokens: 0,
                thinking_in_context,
                thinking_turns: 0,
                categories,
            });
        }

        let actual_system = self.session_system_text().unwrap_or("");
        let (inspected_docs, _) = self.tools.inspect_project_instructions()?;
        let agent_md_section = build_agent_md_items(
            self.tools.cwd(),
            actual_system,
            &self.reported_project_load,
            &inspected_docs,
        )?;
        let (agent_md_items, agent_md_tokens, agent_md_bytes, mut excluded_ranges) = (
            agent_md_section.items,
            agent_md_section.tokens,
            agent_md_section.bytes,
            agent_md_section.ranges,
        );

        let skill_headers = self
            .tools
            .skills
            .lock()
            .map_err(|_| anyhow!("skill registry unavailable"))?
            .headers();
        let skill_section = build_skill_items(actual_system, &skill_headers)?;
        let (skill_items, skills_tokens, skills_bytes) = (
            skill_section.items,
            skill_section.tokens,
            skill_section.bytes,
        );
        excluded_ranges.extend(skill_section.ranges);

        let workflow_text = crate::subagent::catalog::workflow_instructions(&self.workflow);
        let (base_title, base_subtitle, base_text, runtime_text, append_part) =
            if let Some(source) = &self.prompt_source {
                let context = crate::system_prompt::RenderContext::new(
                    self.tools.cwd(),
                    &self.current_model.model,
                    self.tools.edit_mode(),
                    self.workflow.mode_name(),
                    self.session.tool_names(),
                );
                let parts = source.inspect_parts(&context, self.tools.limits)?;
                let append_part = parts.append_origin.zip(parts.append_text);
                (
                    format!("Base System Prompt ({})", parts.base_kind),
                    parts.base_origin,
                    parts.base_text,
                    parts.runtime_text,
                    append_part,
                )
            } else {
                (
                    "Base System Prompt (legacy)".to_owned(),
                    "restored session base_system".to_owned(),
                    self.base_system.clone(),
                    String::new(),
                    None,
                )
            };

        let (system_items, system_tokens, system_bytes) = build_system_items(
            actual_system,
            base_title,
            base_subtitle,
            &base_text,
            &runtime_text,
            append_part,
            &workflow_text,
            &excluded_ranges,
        )?;

        let access = pp_mcp::Access::for_mode(self.workflow.mode);
        let mcp_origins = self.tools.mcp.tool_origins(access);
        let split_tools = build_split_tool_items(&self.session, &mcp_origins)?;

        let history = self.inspect_history()?;
        let thinking = build_thinking_items(&self.session, &self.thinking_history)?;
        let active_tokens = usage.estimated_tokens;

        let mut categories = vec![
            CategorySummary {
                key: CategoryKey::System,
                name: CategoryKey::System.label(),
                tokens: system_tokens,
                bytes: system_bytes,
                item_count: system_items.len(),
                in_context: true,
                items: system_items,
            },
            CategorySummary {
                key: CategoryKey::InstructionFiles,
                name: CategoryKey::InstructionFiles.label(),
                tokens: agent_md_tokens,
                bytes: agent_md_bytes,
                item_count: agent_md_items.len(),
                in_context: true,
                items: agent_md_items,
            },
            CategorySummary {
                key: CategoryKey::Skills,
                name: CategoryKey::Skills.label(),
                tokens: skills_tokens,
                bytes: skills_bytes,
                item_count: skill_items.len(),
                in_context: true,
                items: skill_items,
            },
            CategorySummary {
                key: CategoryKey::BuiltInTools,
                name: CategoryKey::BuiltInTools.label(),
                tokens: split_tools.tools_tokens,
                bytes: split_tools.tools_bytes,
                item_count: split_tools.tool_items.len(),
                in_context: true,
                items: split_tools.tool_items,
            },
            CategorySummary {
                key: CategoryKey::CustomTools,
                name: CategoryKey::CustomTools.label(),
                tokens: split_tools.mcp_tokens,
                bytes: split_tools.mcp_bytes,
                item_count: split_tools.mcp_items.len(),
                in_context: true,
                items: split_tools.mcp_items,
            },
            CategorySummary {
                key: CategoryKey::UserMessages,
                name: CategoryKey::UserMessages.label(),
                tokens: history.user_messages.tokens,
                bytes: history.user_messages.bytes,
                item_count: history.user_messages.items.len(),
                in_context: true,
                items: history.user_messages.items,
            },
            CategorySummary {
                key: CategoryKey::AssistantMessages,
                name: CategoryKey::AssistantMessages.label(),
                tokens: history.assistant_messages.tokens,
                bytes: history.assistant_messages.bytes,
                item_count: history.assistant_messages.items.len(),
                in_context: true,
                items: history.assistant_messages.items,
            },
            CategorySummary {
                key: CategoryKey::Thinking,
                name: CategoryKey::Thinking.label(),
                tokens: thinking.tokens,
                bytes: thinking.bytes,
                item_count: thinking.items.len(),
                in_context: thinking.in_context,
                items: thinking.items,
            },
            CategorySummary {
                key: CategoryKey::ToolCalls,
                name: CategoryKey::ToolCalls.label(),
                tokens: history.tool_calls.tokens,
                bytes: history.tool_calls.bytes,
                item_count: history.tool_calls.items.len(),
                in_context: true,
                items: history.tool_calls.items,
            },
            CategorySummary {
                key: CategoryKey::ToolOutput,
                name: CategoryKey::ToolOutput.label(),
                tokens: history.tool_outputs.tokens,
                bytes: history.tool_outputs.bytes,
                item_count: history.tool_outputs.items.len(),
                in_context: true,
                items: history.tool_outputs.items,
            },
        ];

        // 将总占用 `Used` 按各在上下文分类的原始估算份额切分，使分类合计与 `Used` 对账一致。
        let share_inputs: Vec<Option<u64>> = categories
            .iter()
            .map(|cat| cat.in_context.then_some(cat.tokens))
            .collect();
        let allocated = split_used_by_share(active_tokens, &share_inputs);
        for (cat, tokens) in categories.iter_mut().zip(allocated) {
            if cat.in_context {
                cat.tokens = tokens;
            }
        }

        Ok(ContextReport {
            model_reference: self.current_model.reference.clone(),
            model_name: self.current_model.model.clone(),
            protocol,
            work_mode: self.workflow.mode_name(),
            think_level: self.think_level,
            empty_session,
            active_tokens,
            context_window: self.current_model.context_window,
            auto_compact: self.auto_compact,
            threshold_percent: self.auto_compact_threshold_percent,
            last_request: self.last_request,
            session_output_tokens: self.session_output_tokens,
            thinking_tokens: thinking.tokens,
            thinking_in_context: thinking.in_context,
            thinking_turns: thinking.count,
            categories,
        })
    }

    fn session_system_text(&self) -> Option<&str> {
        match &self.session {
            Session::Completion(request) => match request.messages.first() {
                Some(ChatCompletionRequestMessage::System(message)) => match &message.content {
                    ChatCompletionRequestSystemMessageContent::Text(text) => Some(text.as_str()),
                    ChatCompletionRequestSystemMessageContent::Array(_) => None,
                },
                _ => None,
            },
            Session::Response(request) => request.instructions.as_deref(),
        }
    }
}

fn split_used_by_share(used: u64, raw_tokens: &[Option<u64>]) -> Vec<u64> {
    let raw_sum: u128 = raw_tokens
        .iter()
        .filter_map(|opt| *opt)
        .map(u128::from)
        .sum();
    let mut out = vec![0u64; raw_tokens.len()];
    if used == 0 || raw_sum == 0 {
        return out;
    }
    let used_u128 = u128::from(used);
    let mut remainders: Vec<(usize, u128, u64)> = Vec::new();
    let mut assigned = 0u64;
    for (idx, opt) in raw_tokens.iter().enumerate() {
        let Some(raw) = *opt else {
            continue;
        };
        if raw == 0 {
            continue;
        }
        let exact = used_u128 * u128::from(raw);
        let share = (exact / raw_sum) as u64;
        let rem = exact % raw_sum;
        out[idx] = share;
        assigned = assigned.saturating_add(share);
        remainders.push((idx, rem, raw));
    }
    let left = used.saturating_sub(assigned) as usize;
    remainders.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.0.cmp(&b.0))
    });
    for (idx, _, _) in remainders.into_iter().take(left) {
        out[idx] += 1;
    }
    out
}
