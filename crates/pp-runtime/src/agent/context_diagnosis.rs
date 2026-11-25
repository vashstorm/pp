//! 当前会话上下文的只读诊断快照：严格基于已发送/已恢复的真实会话数据，
//! 按 10 个固定分类拆分统计：`System Prompt`、`Instruction Files`、`Skills`、`Built-in Tools`、
//! `Custom Tools`、`User / Developer Messages`、`Assistant Messages`、`Assistant Thinking`、
//! `Tool Calls` 与 `Tool Output`。
//!
//! 半静态分区按指纹缓存、逐条消息分类按消息身份缓存：追加消息只重算新增尾部，
//! 整段替换历史时从头重算。缓存由 Agent 持有的 [`Snapshot`] 内 `Mutex` 保护，不使用进程级静态。

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
use pp_core::context::{ContextUsage, TokenUsage};
use pp_core::model::ThinkLevel;
use sections::{
    SectionWithRanges, SplitToolsBreakdown, build_agent_md_items, build_skill_items,
    build_split_tool_items, build_system_items,
};
use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
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

/// 流式指纹写入器：只累计字节数与哈希，不分配序列化副本。
#[derive(Default)]
pub(super) struct FingerprintWriter {
    hasher: DefaultHasher,
    bytes: u64,
}

impl FingerprintWriter {
    /// 已写入的字节数（等于同样内容的原生 JSON 长度）。
    pub(super) fn bytes(&self) -> u64 {
        self.bytes
    }

    pub(super) fn finish(self) -> u64 {
        self.hasher.finish()
    }

    /// 写入一段文本：先写长度，避免相邻字段的边界歧义。
    pub(super) fn text(&mut self, text: &str) {
        Hasher::write(&mut self.hasher, b"t");
        Hasher::write(&mut self.hasher, &(text.len() as u64).to_le_bytes());
        Hasher::write(&mut self.hasher, text.as_bytes());
        self.bytes = self.bytes.saturating_add(text.len() as u64);
    }

    pub(super) fn number(&mut self, value: u64) {
        Hasher::write(&mut self.hasher, b"n");
        Hasher::write(&mut self.hasher, &value.to_le_bytes());
        self.bytes = self.bytes.saturating_add(8);
    }
}

impl std::io::Write for FingerprintWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Hasher::write(&mut self.hasher, buf);
        self.bytes = self.bytes.saturating_add(buf.len() as u64);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn hash_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    Hasher::write(&mut hasher, text.as_bytes());
    hasher.finish()
}

/// 上下文诊断的缓存状态：历史分类前缀 + 半静态分区 + 思考分类。
///
/// 由 Agent 持有的 [`Snapshot`] 内 `Mutex` 保护；面板强制刷新与诊断失败会丢弃它。
#[derive(Default)]
pub(super) struct DiagnosisCache {
    history: Option<history::HistoryState>,
    sections: Option<(sections::SectionFingerprint, ClassifiedSections)>,
    thinking: Option<(ThinkingKey, ThinkingSnapshot)>,
}

impl DiagnosisCache {
    /// 丢弃全部缓存；下一次发布强制全量重算。
    pub(super) fn invalidate(&mut self) {
        *self = Self::default();
    }
}

#[derive(Clone)]
struct ClassifiedSections {
    system: SectionWithRanges,
    instructions: SectionWithRanges,
    skills: SectionWithRanges,
    tools: SplitToolsBreakdown,
}

/// 思考分类的缓存键：原生消息身份 + 已采集思考记录指纹。
#[derive(Clone, PartialEq, Eq)]
struct ThinkingKey {
    items: Vec<history::MessageId>,
    history: u64,
}

#[derive(Clone)]
struct ThinkingSnapshot {
    items: Vec<DiagnosticItem>,
    tokens: u64,
    bytes: u64,
    count: usize,
    in_context: bool,
}

impl ThinkingSnapshot {
    fn capture(breakdown: &thinking::ThinkingBreakdown) -> Self {
        Self {
            items: breakdown.items.clone(),
            tokens: breakdown.tokens,
            bytes: breakdown.bytes,
            count: breakdown.count,
            in_context: breakdown.in_context,
        }
    }

    fn breakdown(&self) -> thinking::ThinkingBreakdown {
        thinking::ThinkingBreakdown {
            items: self.items.clone(),
            tokens: self.tokens,
            bytes: self.bytes,
            count: self.count,
            in_context: self.in_context,
        }
    }
}

struct Classified {
    sections: ClassifiedSections,
    history: history::HistoryBreakdown,
    thinking: thinking::ThinkingBreakdown,
}

/// 报告装配的入参：分类结果之外只剩本轮的用量与协议信息。
struct ReportInputs {
    usage: ContextUsage,
    protocol: &'static str,
    empty_session: bool,
    thinking_in_context: bool,
    categories: Vec<CategorySummary>,
    thinking_tokens: u64,
    thinking_turns: usize,
}

impl Agent {
    /// 生成当前会话上下文的只读诊断报告。
    ///
    /// 这是强制重算入口（面板 `r` 刷新走这里），不读取也不写入任何缓存：
    /// 仅基于当前会话已发送/已恢复的真实数据统计；
    /// 尚未开始对话的空会话返回零用量及全零分类，不把未发送的配置基线计入上下文，也不创建会话日志。
    pub fn diagnose_context(&self) -> Result<ContextReport> {
        self.diagnose_context_with(&mut DiagnosisCache::default(), None)
    }

    /// 诊断发布路径：`usage` 已由调用方在本轮请求前算好时可复用，避免重复全量估算。
    pub(super) fn diagnose_context_with(
        &self,
        cache: &mut DiagnosisCache,
        usage: Option<ContextUsage>,
    ) -> Result<ContextReport> {
        let usage = match usage {
            Some(usage) => usage,
            None => self.context_usage()?,
        };
        let protocol = match &self.session {
            Session::Completion(_) => "completion",
            Session::Response(_) => "response",
        };
        let thinking_in_context = matches!(&self.session, Session::Response(_));

        if usage.estimated_tokens == 0 {
            // 空会话不读盘、不建缓存：零用量语义与既有一致。
            cache.invalidate();
            let categories = CategoryKey::ALL
                .into_iter()
                .map(|key| {
                    category(
                        key,
                        0,
                        0,
                        Vec::new(),
                        key != CategoryKey::Thinking || thinking_in_context,
                    )
                })
                .collect();
            return Ok(self.report(ReportInputs {
                usage,
                protocol,
                empty_session: true,
                thinking_in_context,
                categories,
                thinking_tokens: 0,
                thinking_turns: 0,
            }));
        }

        let classified = self.classify_context(cache)?;
        let sections = &classified.sections;
        let history = &classified.history;
        let thinking = &classified.thinking;
        let mut instruction_tokens = sections.instructions.tokens;
        let mut instruction_bytes = sections.instructions.bytes;
        let mut instruction_items = sections.instructions.items.clone();
        for (path, item) in &sections.instructions.fallback_items {
            if !history.instruction_paths.contains(path) {
                instruction_tokens = instruction_tokens.saturating_add(item.tokens);
                instruction_bytes = instruction_bytes.saturating_add(item.bytes);
                instruction_items.push(item.clone());
            }
        }
        instruction_tokens = instruction_tokens.saturating_add(history.instruction_messages.tokens);
        instruction_bytes = instruction_bytes.saturating_add(history.instruction_messages.bytes);
        instruction_items.extend(history.instruction_messages.items.iter().cloned());

        let mut categories = vec![
            category(
                CategoryKey::System,
                sections.system.tokens,
                sections.system.bytes,
                sections.system.items.clone(),
                true,
            ),
            category(
                CategoryKey::InstructionFiles,
                instruction_tokens,
                instruction_bytes,
                instruction_items,
                true,
            ),
            category(
                CategoryKey::Skills,
                sections.skills.tokens,
                sections.skills.bytes,
                sections.skills.items.clone(),
                true,
            ),
            category(
                CategoryKey::BuiltInTools,
                sections.tools.tools_tokens,
                sections.tools.tools_bytes,
                sections.tools.tool_items.clone(),
                true,
            ),
            category(
                CategoryKey::CustomTools,
                sections.tools.mcp_tokens,
                sections.tools.mcp_bytes,
                sections.tools.mcp_items.clone(),
                true,
            ),
            category(
                CategoryKey::UserMessages,
                history.user_messages.tokens,
                history.user_messages.bytes,
                history.user_messages.items.clone(),
                true,
            ),
            category(
                CategoryKey::AssistantMessages,
                history.assistant_messages.tokens,
                history.assistant_messages.bytes,
                history.assistant_messages.items.clone(),
                true,
            ),
            category(
                CategoryKey::Thinking,
                thinking.tokens,
                thinking.bytes,
                thinking.items.clone(),
                thinking.in_context,
            ),
            category(
                CategoryKey::ToolCalls,
                history.tool_calls.tokens,
                history.tool_calls.bytes,
                history.tool_calls.items.clone(),
                true,
            ),
            category(
                CategoryKey::ToolOutput,
                history.tool_outputs.tokens,
                history.tool_outputs.bytes,
                history.tool_outputs.items.clone(),
                true,
            ),
        ];

        // 将总占用 `Used` 按各在上下文分类的原始估算份额切分，使分类合计与 `Used` 对账一致。
        let share_inputs: Vec<Option<u64>> = categories
            .iter()
            .map(|cat| cat.in_context.then_some(cat.tokens))
            .collect();
        let allocated = split_used_by_share(usage.estimated_tokens, &share_inputs);
        for (cat, tokens) in categories.iter_mut().zip(allocated) {
            if cat.in_context {
                cat.tokens = tokens;
            }
        }

        Ok(self.report(ReportInputs {
            usage,
            protocol,
            empty_session: false,
            thinking_in_context,
            categories,
            thinking_tokens: thinking.tokens,
            thinking_turns: thinking.count,
        }))
    }

    fn report(&self, inputs: ReportInputs) -> ContextReport {
        let usage = inputs.usage;
        ContextReport {
            model_reference: self.current_model.reference.clone(),
            model_name: self.current_model.model.clone(),
            protocol: inputs.protocol,
            work_mode: self.workflow.mode_name(),
            think_level: self.think_level,
            empty_session: inputs.empty_session,
            active_tokens: usage.estimated_tokens,
            context_window: usage.context_window,
            auto_compact: usage.auto_compact,
            threshold_percent: usage.threshold_percent,
            last_request: usage.last_request,
            session_output_tokens: usage.session_output_tokens,
            thinking_tokens: inputs.thinking_tokens,
            thinking_in_context: inputs.thinking_in_context,
            thinking_turns: inputs.thinking_turns,
            categories: inputs.categories,
        }
    }

    fn classify_context(&self, cache: &mut DiagnosisCache) -> Result<Classified> {
        let ids = history::message_ids(&self.session);

        let actual_system = self.session_system_text().unwrap_or("");
        let skill_headers = self
            .tools
            .skills
            .lock()
            .map_err(|_| anyhow!("skill registry unavailable"))?
            .headers();
        let mcp_origins = self.tools.mcp.tool_origins(self.tools.mcp_access());
        let fingerprint =
            sections::SectionFingerprint::of(self, actual_system, &skill_headers, &mcp_origins);
        let sections = match &cache.sections {
            Some((key, sections)) if *key == fingerprint => sections.clone(),
            _ => {
                let built = self.build_sections(actual_system, &skill_headers, &mcp_origins)?;
                cache.sections = Some((fingerprint, built.clone()));
                built
            }
        };

        let history = self.inspect_history_cached(&mut cache.history, &ids)?;

        let thinking_key = ThinkingKey {
            items: match &self.session {
                Session::Response(_) => ids,
                Session::Completion(_) => Vec::new(),
            },
            history: thinking_history_fingerprint(&self.thinking_history),
        };
        let thinking = match &cache.thinking {
            Some((key, snapshot)) if *key == thinking_key => snapshot.breakdown(),
            _ => {
                let built = build_thinking_items(&self.session, &self.thinking_history)?;
                cache.thinking = Some((thinking_key, ThinkingSnapshot::capture(&built)));
                built
            }
        };

        Ok(Classified {
            sections,
            history,
            thinking,
        })
    }

    fn build_sections(
        &self,
        actual_system: &str,
        skill_headers: &[pp_config::skills::SkillHeader],
        mcp_origins: &BTreeMap<String, (String, String)>,
    ) -> Result<ClassifiedSections> {
        let (inspected_docs, _) = self.tools.inspect_project_instructions()?;
        let agent_md = build_agent_md_items(
            self.tools.cwd(),
            actual_system,
            &self.reported_project_load,
            &inspected_docs,
        )?;
        let skill_section = build_skill_items(actual_system, skill_headers)?;
        let mut excluded_ranges = agent_md.ranges.clone();
        excluded_ranges.extend(skill_section.ranges.iter().copied());

        let workflow_text = crate::subagent::catalog::workflow_instructions(&self.workflow);
        let (base_title, base_subtitle, base_text, runtime_text, append_part) =
            if let Some(source) = &self.prompt_source {
                let context = crate::system_prompt::RenderContext::new(
                    self.tools.cwd(),
                    &self.current_model.model,
                    self.tools.edit_mode(),
                    self.workflow.mode_name(),
                    self.session.tool_names(),
                )
                .with_sandbox(&self.tools.sandbox_status());
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

        let split_tools = build_split_tool_items(&self.session, mcp_origins)?;

        Ok(ClassifiedSections {
            system: SectionWithRanges {
                items: system_items,
                tokens: system_tokens,
                bytes: system_bytes,
                ranges: Vec::new(),
                fallback_items: Vec::new(),
            },
            instructions: agent_md,
            skills: skill_section,
            tools: split_tools,
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

fn category(
    key: CategoryKey,
    tokens: u64,
    bytes: u64,
    items: Vec<DiagnosticItem>,
    in_context: bool,
) -> CategorySummary {
    CategorySummary {
        key,
        name: key.label(),
        tokens,
        bytes,
        item_count: items.len(),
        in_context,
        items,
    }
}

fn thinking_history_fingerprint(history: &[ThinkingEntry]) -> u64 {
    let mut writer = FingerprintWriter::default();
    writer.number(history.len() as u64);
    for entry in history {
        writer.text(&entry.thinking);
        writer.text(&entry.summary);
    }
    writer.finish()
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
