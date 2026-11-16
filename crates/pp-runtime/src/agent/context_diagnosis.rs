//! 当前会话上下文的只读诊断快照：按分块、项目指令、工具/MCP Schema、
//! 技能、历史消息与工具结果拆分 Token 估算，并给出压缩边界预览与最大占用项排行。

mod history;
mod sections;
#[cfg(test)]
mod tests;

use super::{
    Agent, Session,
    context::{
        completion_boundaries, response_boundaries, retained_suffix_budget, select_cutoff,
        summary_budget,
    },
};
use crate::project_instructions;
use anyhow::{Result, anyhow};
use async_openai::types::{
    chat::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
        ChatCompletionRequestSystemMessageContent,
    },
    responses::InputParam,
};
use pp_core::{
    context::{TokenUsage, estimate_bytes},
    model::ThinkLevel,
};
use sections::{
    build_agent_md_items, build_skill_items, build_system_items, build_tool_items,
    completion_tools, response_tools,
};

const MAX_PREVIEW_BYTES: usize = 64 * 1024;
const LARGEST_LIMIT: usize = 30;

#[derive(Clone, Debug)]
pub struct ContextReport {
    pub model_reference: String,
    pub model_name: String,
    pub protocol: &'static str,
    pub work_mode: &'static str,
    pub think_level: ThinkLevel,
    pub empty_session: bool,
    pub active_tokens: u64,
    pub prepared_tokens: u64,
    pub context_window: Option<u64>,
    pub auto_compact: bool,
    pub threshold_percent: u8,
    pub last_request: Option<TokenUsage>,
    pub session_output_tokens: u64,
    pub compaction: CompactionPreview,
    pub categories: Vec<CategorySummary>,
    pub largest_items: Vec<DiagnosticItem>,
    pub system_items: Vec<DiagnosticItem>,
    pub agent_md_items: Vec<DiagnosticItem>,
    pub tool_items: Vec<DiagnosticItem>,
    pub skill_items: Vec<DiagnosticItem>,
    pub history_items: Vec<DiagnosticItem>,
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
    pub name: &'static str,
    pub tokens: u64,
    pub bytes: u64,
    pub item_count: usize,
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
    /// 生成当前会话上下文的只读诊断报告，不修改历史、项目指令放行状态或日志。
    pub fn diagnose_context(&self) -> Result<ContextReport> {
        let usage = self.context_usage()?;
        let empty_session = usage.estimated_tokens == 0;
        let definitions = self.tools.all_definitions(self.workflow.mode);
        let tool_names: Vec<String> = definitions.iter().map(|d| d.name.clone()).collect();

        let (inspected_docs, scope_summary) = self.tools.inspect_project_instructions()?;
        let documents: Vec<_> = inspected_docs
            .iter()
            .map(|doc| doc.document.clone())
            .collect();
        let project_text = format!(
            "{scope_summary}{}",
            project_instructions::render(&documents)
        );

        let skill_headers = self
            .tools
            .skills
            .lock()
            .map_err(|_| anyhow!("skill registry unavailable"))?
            .headers();
        let skills_summary_text = pp_config::skills::Skills::format_summary(&skill_headers);
        let workflow_text = crate::subagent::catalog::workflow_instructions(&self.workflow);

        let (base_title, base_subtitle, base_text, runtime_text, append_part, full_system) =
            if let Some(source) = &self.prompt_source {
                let context = crate::system_prompt::RenderContext::new(
                    self.tools.cwd(),
                    &self.current_model.model,
                    self.tools.edit_mode(),
                    self.workflow.mode_name(),
                    tool_names,
                );
                let parts = source.inspect_parts(&context, self.tools.limits)?;
                let composed = source.compose(
                    &context,
                    self.tools.limits,
                    &project_text,
                    &skills_summary_text,
                    &workflow_text,
                )?;
                let append_part = parts.append_origin.zip(parts.append_text);
                (
                    format!("Base System Prompt ({})", parts.base_kind),
                    parts.base_origin,
                    parts.base_text,
                    parts.runtime_text,
                    append_part,
                    composed,
                )
            } else {
                let mut composed = project_instructions::compose(
                    &self.base_system,
                    &project_text,
                    &skills_summary_text,
                );
                if !workflow_text.is_empty() {
                    composed.push_str("\n\n");
                    composed.push_str(&workflow_text);
                }
                (
                    "Base System Prompt (legacy)".to_owned(),
                    "restored session base_system".to_owned(),
                    self.base_system.clone(),
                    String::new(),
                    None,
                    composed,
                )
            };

        let prepared_tokens = self.estimate_prepared_tokens(&full_system, &definitions)?;
        let session_system_matches = self.session_system_text() == Some(full_system.as_str());

        let (system_items, system_tokens, system_bytes, system_count) = build_system_items(
            base_title,
            base_subtitle,
            base_text,
            runtime_text,
            append_part,
            workflow_text,
            &full_system,
            session_system_matches,
        )?;

        let (agent_md_items, agent_md_tokens, agent_md_bytes, agent_md_count) =
            build_agent_md_items(self.tools.cwd(), &inspected_docs, &scope_summary)?;

        let access = crate::mcp::Access::for_mode(self.workflow.mode);
        let mcp_origins = self.tools.mcp.tool_origins(access);
        let mcp_view = self.mcp_view();
        let (tool_items, tools_tokens, tools_bytes, tools_count) =
            build_tool_items(&self.session, &definitions, &mcp_origins, &mcp_view)?;

        let history = self.inspect_history()?;
        let (skill_items, skills_tokens, skills_bytes, skills_count) =
            build_skill_items(&skill_headers, &skills_summary_text, &history.loaded_skills)?;

        let compaction = self.inspect_compaction(history.has_existing_summary)?;

        let categories = vec![
            CategorySummary {
                name: "System Prompt",
                tokens: system_tokens,
                bytes: system_bytes,
                item_count: system_count,
                description: "Base, runtime, append, and workflow instructions".into(),
            },
            CategorySummary {
                name: "AGENTS.md",
                tokens: agent_md_tokens,
                bytes: agent_md_bytes,
                item_count: agent_md_count,
                description: "Maintenance rules, root boundaries, and active AGENTS.md files"
                    .into(),
            },
            CategorySummary {
                name: "Skills Index",
                tokens: skills_tokens,
                bytes: skills_bytes,
                item_count: skills_count,
                description: format!(
                    "{} enabled in system prompt · {} loaded via read(skill://...)",
                    skill_headers.len(),
                    history.loaded_skills.len()
                ),
            },
            CategorySummary {
                name: "Tools & MCP",
                tokens: tools_tokens,
                bytes: tools_bytes,
                item_count: tools_count,
                description: "Built-in and active MCP JSON schemas".into(),
            },
            CategorySummary {
                name: "History Messages",
                tokens: history.history_tokens,
                bytes: history.history_bytes,
                item_count: history.history_count,
                description: "User, summary, developer, assistant, reasoning, and tool calls"
                    .into(),
            },
            CategorySummary {
                name: "Tool Results",
                tokens: history.tool_result_tokens,
                bytes: history.tool_result_bytes,
                item_count: history.tool_result_count,
                description: "Tool outputs and read image attachments".into(),
            },
        ];

        let mut largest_items: Vec<DiagnosticItem> = system_items
            .iter()
            .chain(&agent_md_items)
            .chain(&tool_items)
            .chain(&skill_items)
            .chain(&history.history_items)
            .filter(|item| item.rankable && item.tokens > 0)
            .cloned()
            .collect();
        largest_items.sort_by(|a, b| {
            b.tokens
                .cmp(&a.tokens)
                .then_with(|| b.bytes.cmp(&a.bytes))
                .then_with(|| a.title.cmp(&b.title))
        });
        largest_items.truncate(LARGEST_LIMIT);

        let protocol = match &self.session {
            Session::Completion(_) => "completion",
            Session::Response(_) => "response",
        };

        Ok(ContextReport {
            model_reference: self.current_model.reference.clone(),
            model_name: self.current_model.model.clone(),
            protocol,
            work_mode: self.workflow.mode_name(),
            think_level: self.think_level,
            empty_session,
            active_tokens: usage.estimated_tokens,
            prepared_tokens,
            context_window: self.current_model.context_window,
            auto_compact: self.auto_compact,
            threshold_percent: self.auto_compact_threshold_percent,
            last_request: self.last_request,
            session_output_tokens: self.session_output_tokens,
            compaction,
            categories,
            largest_items,
            system_items,
            agent_md_items,
            tool_items,
            skill_items,
            history_items: history.history_items,
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

    fn estimate_prepared_tokens(
        &self,
        full_system: &str,
        definitions: &[async_openai::types::chat::FunctionObject],
    ) -> Result<u64> {
        match &self.session {
            Session::Completion(request) => {
                let total_bytes = estimate_bytes(request)?;
                let session_sys_bytes = match request.messages.first() {
                    Some(message @ ChatCompletionRequestMessage::System(_)) => {
                        estimate_bytes(message)?
                    }
                    _ => 0,
                };
                let prepared_sys =
                    ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                        content: full_system.to_owned().into(),
                        ..Default::default()
                    });
                let prepared_sys_bytes = estimate_bytes(&prepared_sys)?;
                let session_tools_bytes = estimate_bytes(&request.tools)?;
                let prepared_tools = Some(completion_tools(definitions));
                let prepared_tools_bytes = estimate_bytes(&prepared_tools)?;
                let adjusted = total_bytes
                    .saturating_sub(session_sys_bytes)
                    .saturating_add(prepared_sys_bytes)
                    .saturating_sub(session_tools_bytes)
                    .saturating_add(prepared_tools_bytes);
                Ok(adjusted.div_ceil(3))
            }
            Session::Response(request) => {
                let total_bytes = estimate_bytes(request)?;
                let session_sys_bytes = estimate_bytes(&request.instructions)?;
                let prepared_sys = Some(full_system.to_owned());
                let prepared_sys_bytes = estimate_bytes(&prepared_sys)?;
                let session_tools_bytes = estimate_bytes(&request.tools)?;
                let prepared_tools = Some(response_tools(definitions));
                let prepared_tools_bytes = estimate_bytes(&prepared_tools)?;
                let adjusted = total_bytes
                    .saturating_sub(session_sys_bytes)
                    .saturating_add(prepared_sys_bytes)
                    .saturating_sub(session_tools_bytes)
                    .saturating_add(prepared_tools_bytes);
                Ok(adjusted.div_ceil(3))
            }
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
