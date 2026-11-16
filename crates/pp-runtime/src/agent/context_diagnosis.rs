//! 当前会话上下文的只读诊断快照：严格基于已发送/已恢复的真实会话数据，
//! 按系统指令、已发送 AGENTS.md、技能、内置工具、MCP、历史消息、LLM 思考与工具结果拆分统计。

mod history;
mod sections;
mod snapshot;
#[cfg(test)]
mod tests;
mod thinking;

pub use snapshot::Snapshot;

pub(crate) use thinking::{ThinkingEntry, extract_thinking_from_replay, trim_for_retained_turns};

use super::{
    Agent, Session,
    context::{
        completion_boundaries, response_boundaries, retained_suffix_budget, select_cutoff,
        summary_budget,
    },
};
use anyhow::{Result, anyhow};
use async_openai::types::{
    chat::{ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageContent},
    responses::InputParam,
};
use pp_core::{
    context::{TokenUsage, estimate_bytes},
    model::ThinkLevel,
};
use sections::{
    build_agent_md_items, build_skill_items, build_split_tool_items, build_system_items,
};
use thinking::build_thinking_items;

const MAX_PREVIEW_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CategoryKey {
    System,
    AgentsMd,
    Skills,
    Tools,
    Mcp,
    Messages,
    Thinking,
    ToolResults,
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
    pub compaction: CompactionPreview,
    pub categories: Vec<CategorySummary>,
    pub system_items: Vec<DiagnosticItem>,
    pub agent_md_items: Vec<DiagnosticItem>,
    pub skill_items: Vec<DiagnosticItem>,
    pub tool_items: Vec<DiagnosticItem>,
    pub mcp_items: Vec<DiagnosticItem>,
    pub message_items: Vec<DiagnosticItem>,
    pub thinking_items: Vec<DiagnosticItem>,
    pub tool_result_items: Vec<DiagnosticItem>,
}

#[derive(Clone, Debug)]
pub struct CompactionPreview {
    pub retained_budget_tokens: u64,
    pub summary_budget_tokens: u64,
    pub total_exchanges: usize,
    pub has_existing_summary: bool,
    pub status: CompactionStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompactionStatus {
    NotEnoughHistory,
    Ready {
        compressible_exchanges: usize,
        compressible_tokens: u64,
        retained_exchanges: usize,
        retained_tokens: u64,
    },
    UnsupportedHistory,
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
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct DiagnosticItem {
    pub kind_label: String,
    pub title: String,
    pub subtitle: String,
    pub tokens: u64,
    pub bytes: u64,
    pub preview: String,
    pub is_header: bool,
    pub rankable: bool,
}

impl Agent {
    /// 生成当前会话上下文的只读诊断报告，仅统计真实已发送/已恢复的数据。
    pub fn diagnose_context(&self) -> Result<ContextReport> {
        let usage = self.context_usage()?;
        let empty_session = usage.estimated_tokens == 0;
        let protocol = match &self.session {
            Session::Completion(_) => "completion",
            Session::Response(_) => "response",
        };
        let default_thinking_in_ctx = matches!(&self.session, Session::Response(_));

        if empty_session {
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
                thinking_in_context: default_thinking_in_ctx,
                thinking_turns: 0,
                compaction: CompactionPreview {
                    retained_budget_tokens: retained_suffix_budget(
                        self.current_model.context_window,
                    ),
                    summary_budget_tokens: summary_budget(self.current_model.context_window),
                    total_exchanges: 0,
                    has_existing_summary: false,
                    status: CompactionStatus::NotEnoughHistory,
                },
                categories: Vec::new(),
                system_items: Vec::new(),
                agent_md_items: Vec::new(),
                skill_items: Vec::new(),
                tool_items: Vec::new(),
                mcp_items: Vec::new(),
                message_items: Vec::new(),
                thinking_items: Vec::new(),
                tool_result_items: Vec::new(),
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
        let (agent_md_items, agent_md_tokens, agent_md_bytes, agent_md_count, mut excluded_ranges) = (
            agent_md_section.items,
            agent_md_section.tokens,
            agent_md_section.bytes,
            agent_md_section.count,
            agent_md_section.ranges,
        );

        let history = self.inspect_history()?;
        let skill_headers = self
            .tools
            .skills
            .lock()
            .map_err(|_| anyhow!("skill registry unavailable"))?
            .headers();
        let skill_section =
            build_skill_items(actual_system, &skill_headers, &history.loaded_skills)?;
        let (skill_items, skills_tokens, skills_bytes, skills_count) = (
            skill_section.items,
            skill_section.tokens,
            skill_section.bytes,
            skill_section.count,
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

        let (system_items, system_tokens, system_bytes, system_count) = build_system_items(
            actual_system,
            base_title,
            base_subtitle,
            &base_text,
            &runtime_text,
            append_part,
            &workflow_text,
            &excluded_ranges,
        )?;

        let access = crate::mcp::Access::for_mode(self.workflow.mode);
        let mcp_origins = self.tools.mcp.tool_origins(access);
        let mcp_view = self.mcp_view();
        let split_tools = build_split_tool_items(&self.session, &mcp_origins, &mcp_view)?;

        let thinking = build_thinking_items(&self.session, &self.thinking_history)?;
        let compaction = self.inspect_compaction(history.has_existing_summary)?;

        let categories = vec![
            CategorySummary {
                key: CategoryKey::System,
                name: "System Prompt",
                tokens: system_tokens,
                bytes: system_bytes,
                item_count: system_count,
                in_context: true,
                description: "Base, runtime, append, and workflow instructions in session".into(),
            },
            CategorySummary {
                key: CategoryKey::AgentsMd,
                name: "AGENTS.md",
                tokens: agent_md_tokens,
                bytes: agent_md_bytes,
                item_count: agent_md_count,
                in_context: true,
                description: "Maintenance rules and active AGENTS.md sent in session".into(),
            },
            CategorySummary {
                key: CategoryKey::Skills,
                name: "Skills",
                tokens: skills_tokens,
                bytes: skills_bytes,
                item_count: skills_count,
                in_context: true,
                description: format!(
                    "{skills_count} in prompt · {} bodies read in history",
                    history.loaded_skills.len()
                ),
            },
            CategorySummary {
                key: CategoryKey::Tools,
                name: "Tools",
                tokens: split_tools.tools_tokens,
                bytes: split_tools.tools_bytes,
                item_count: split_tools.tools_count,
                in_context: true,
                description: "Built-in tool JSON schemas in session".into(),
            },
            CategorySummary {
                key: CategoryKey::Mcp,
                name: "MCP",
                tokens: split_tools.mcp_tokens,
                bytes: split_tools.mcp_bytes,
                item_count: split_tools.mcp_count,
                in_context: true,
                description: "External MCP server tool schemas in session".into(),
            },
            CategorySummary {
                key: CategoryKey::Messages,
                name: "Messages",
                tokens: history.message_tokens,
                bytes: history.message_bytes,
                item_count: history.message_count,
                in_context: true,
                description: "User, summary, developer, assistant replies, and tool calls".into(),
            },
            CategorySummary {
                key: CategoryKey::Thinking,
                name: "Thinking",
                tokens: thinking.tokens,
                bytes: thinking.bytes,
                item_count: thinking.count,
                in_context: thinking.in_context,
                description: if thinking.in_context {
                    "LLM reasoning items carried in context".into()
                } else {
                    "LLM thinking output recorded from model responses".into()
                },
            },
            CategorySummary {
                key: CategoryKey::ToolResults,
                name: "Tool Results",
                tokens: history.tool_result_tokens,
                bytes: history.tool_result_bytes,
                item_count: history.tool_result_count,
                in_context: true,
                description: "Tool outputs and read image attachments".into(),
            },
        ];

        Ok(ContextReport {
            model_reference: self.current_model.reference.clone(),
            model_name: self.current_model.model.clone(),
            protocol,
            work_mode: self.workflow.mode_name(),
            think_level: self.think_level,
            empty_session: false,
            active_tokens: usage.estimated_tokens,
            context_window: self.current_model.context_window,
            auto_compact: self.auto_compact,
            threshold_percent: self.auto_compact_threshold_percent,
            last_request: self.last_request,
            session_output_tokens: self.session_output_tokens,
            thinking_tokens: thinking.tokens,
            thinking_in_context: thinking.in_context,
            thinking_turns: thinking.count,
            compaction,
            categories,
            system_items,
            agent_md_items,
            skill_items,
            tool_items: split_tools.tool_items,
            mcp_items: split_tools.mcp_items,
            message_items: history.message_items,
            thinking_items: thinking.items,
            tool_result_items: history.tool_result_items,
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

    fn inspect_compaction(&self, has_existing_summary: bool) -> Result<CompactionPreview> {
        let retained_budget_tokens = retained_suffix_budget(self.current_model.context_window);
        let summary_budget_tokens = summary_budget(self.current_model.context_window);
        let (total_exchanges, status) = match &self.session {
            Session::Completion(request) => match completion_boundaries(&request.messages) {
                Err(_) => (0, CompactionStatus::UnsupportedHistory),
                Ok(boundaries) => {
                    let total = boundaries.len();
                    match select_cutoff(&boundaries, retained_budget_tokens, |cutoff| {
                        Ok(estimate_bytes(&&request.messages[cutoff..])?.div_ceil(3))
                    })? {
                        None => (total, CompactionStatus::NotEnoughHistory),
                        Some(cutoff) => {
                            let retained_exchanges =
                                boundaries.iter().filter(|&&b| b > cutoff).count();
                            let compressible_exchanges = total.saturating_sub(retained_exchanges);
                            let prefix_start = usize::from(matches!(
                                request.messages.first(),
                                Some(ChatCompletionRequestMessage::System(_))
                            ));
                            let compressible_tokens =
                                estimate_bytes(&&request.messages[prefix_start..cutoff])?
                                    .div_ceil(3);
                            let retained_tokens =
                                estimate_bytes(&&request.messages[cutoff..])?.div_ceil(3);
                            (
                                total,
                                CompactionStatus::Ready {
                                    compressible_exchanges,
                                    compressible_tokens,
                                    retained_exchanges,
                                    retained_tokens,
                                },
                            )
                        }
                    }
                }
            },
            Session::Response(request) => {
                let InputParam::Items(items) = &request.input else {
                    return Ok(CompactionPreview {
                        retained_budget_tokens,
                        summary_budget_tokens,
                        total_exchanges: 0,
                        has_existing_summary,
                        status: CompactionStatus::NotEnoughHistory,
                    });
                };
                match response_boundaries(items) {
                    Err(_) => (0, CompactionStatus::UnsupportedHistory),
                    Ok(boundaries) => {
                        let total = boundaries.len();
                        match select_cutoff(&boundaries, retained_budget_tokens, |cutoff| {
                            Ok(estimate_bytes(&&items[cutoff..])?.div_ceil(3))
                        })? {
                            None => (total, CompactionStatus::NotEnoughHistory),
                            Some(cutoff) => {
                                let retained_exchanges =
                                    boundaries.iter().filter(|&&b| b > cutoff).count();
                                let compressible_exchanges =
                                    total.saturating_sub(retained_exchanges);
                                let compressible_tokens =
                                    estimate_bytes(&&items[..cutoff])?.div_ceil(3);
                                let retained_tokens =
                                    estimate_bytes(&&items[cutoff..])?.div_ceil(3);
                                (
                                    total,
                                    CompactionStatus::Ready {
                                        compressible_exchanges,
                                        compressible_tokens,
                                        retained_exchanges,
                                        retained_tokens,
                                    },
                                )
                            }
                        }
                    }
                }
            }
        };
        Ok(CompactionPreview {
            retained_budget_tokens,
            summary_budget_tokens,
            total_exchanges,
            has_existing_summary,
            status,
        })
    }
}
