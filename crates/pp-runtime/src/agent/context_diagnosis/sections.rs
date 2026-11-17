//! 基于当前会话真实数据的半静态上下文分块构建：
//! 系统提示词、已发送的指令文件（AGENTS.md）、技能索引、内置工具 Schema 与自定义/MCP 工具 Schema。

use super::{DiagnosticItem, FingerprintWriter, MAX_PREVIEW_BYTES};
use crate::agent::{Agent, Session};
use anyhow::Result;
use async_openai::types::{
    chat::{ChatCompletionTools, FunctionObject},
    responses::Tool,
};
use pp_config::config::EditMode;
use pp_core::context::estimate_bytes;
use pp_tools::project_instructions::{self, InspectedDocument};
use std::borrow::Cow;
use std::collections::BTreeMap;

#[derive(Clone)]
pub(super) struct SectionWithRanges {
    pub items: Vec<DiagnosticItem>,
    pub tokens: u64,
    pub bytes: u64,
    pub ranges: Vec<(usize, usize)>,
}

/// 从 `actual_system` 中剥离并构建已真实注入的 `AGENTS.md` 指令文件条目，
/// 排除尚未随请求发送的规则。
pub(super) fn build_agent_md_items(
    cwd: &std::path::Path,
    actual_system: &str,
    reported_loads: &BTreeMap<String, String>,
    inspected_docs: &[InspectedDocument],
) -> Result<SectionWithRanges> {
    let mut items = Vec::new();
    let mut tokens = 0u64;
    let mut bytes = 0u64;
    let mut consumed_ranges: Vec<(usize, usize)> = Vec::new();

    // 提取真实存在于 actual_system 中的 `[Project root boundary: ...]` 片段
    let boundary_prefix = "[Project root boundary: ";
    let boundary_suffix = "Ancestor project rules do not apply inside this project root.\n";
    let mut search_from = 0usize;
    let mut boundaries_text = String::new();
    while let Some(rel_start) = actual_system[search_from..].find(boundary_prefix) {
        let start = search_from + rel_start;
        if let Some(rel_end) = actual_system[start..].find(boundary_suffix) {
            let end = start + rel_end + boundary_suffix.len();
            consumed_ranges.push((start, end));
            if !boundaries_text.is_empty() {
                boundaries_text.push('\n');
            }
            boundaries_text.push_str(actual_system[start..end].trim());
            search_from = end;
        } else {
            break;
        }
    }
    if !boundaries_text.is_empty() {
        let b = estimate_bytes(&boundaries_text)?;
        let t = b.div_ceil(3);
        tokens = tokens.saturating_add(t);
        bytes = bytes.saturating_add(b);
        items.push(DiagnosticItem {
            kind_label: "Boundary".into(),
            title: "Project Root Boundaries".into(),
            subtitle: "Nested git root isolation notices".into(),
            tokens: t,
            bytes: b,
            preview: boundaries_text,
        });
    }

    // 优先从 actual_system 中直接提取真实注入的 `[Project instructions: <path>]...[End project instructions]` 块
    let instr_prefix = "[Project instructions: ";
    let instr_end_marker = "[End project instructions]";
    let mut seen_paths = std::collections::BTreeSet::new();
    let mut pos = 0usize;
    while let Some(rel_start) = actual_system[pos..].find(instr_prefix) {
        let start = pos + rel_start;
        let Some(rel_end) = actual_system[start..].find(instr_end_marker) else {
            break;
        };
        let end = start + rel_end + instr_end_marker.len();
        consumed_ranges.push((start, end));
        let block = &actual_system[start..end];
        let (path_str, root_str, body) = parse_project_instruction_block(block);
        let b = estimate_bytes(&block)?;
        let t = b.div_ceil(3);
        tokens = tokens.saturating_add(t);
        bytes = bytes.saturating_add(b);
        seen_paths.insert(path_str.clone());
        let display_path = std::path::Path::new(&path_str)
            .strip_prefix(cwd)
            .map(|rel| rel.display().to_string())
            .unwrap_or_else(|_| path_str.clone());
        items.push(DiagnosticItem {
            kind_label: "AGENTS.md".into(),
            title: display_path,
            subtitle: if root_str.is_empty() {
                "sent in session".into()
            } else {
                format!("root {root_str}")
            },
            tokens: t,
            bytes: b,
            preview: format!(
                "Path: {path_str}\nRoot: {}\nStatus: sent in session\nRaw bytes: {} · Injected bytes: {b} (~{t} tokens)\n\n{}",
                if root_str.is_empty() { "—" } else { &root_str },
                body.len(),
                truncate_preview(body),
            ),
        });
        pos = end;
    }

    // 兜底：对于通过 `reported_loads` 或 `doc.sent`（以及空会话初始活跃链）确认的文档
    for (path_str, content) in reported_loads {
        if seen_paths.contains(path_str) {
            continue;
        }
        let doc_opt = inspected_docs
            .iter()
            .find(|d| d.document.path.to_string_lossy() == path_str.as_str())
            .map(|d| d.document.clone())
            .unwrap_or_else(|| project_instructions::Document {
                path: std::path::PathBuf::from(path_str),
                root: cwd.to_owned(),
                content: content.clone(),
            });
        let rendered = project_instructions::render(std::slice::from_ref(&doc_opt));
        let b = estimate_bytes(&rendered)?;
        let t = b.div_ceil(3);
        tokens = tokens.saturating_add(t);
        bytes = bytes.saturating_add(b);
        seen_paths.insert(path_str.clone());
        let display_path = doc_opt
            .path
            .strip_prefix(cwd)
            .map(|rel| rel.display().to_string())
            .unwrap_or_else(|_| path_str.clone());
        items.push(DiagnosticItem {
            kind_label: "AGENTS.md".into(),
            title: display_path,
            subtitle: format!("root {}", doc_opt.root.display()),
            tokens: t,
            bytes: b,
            preview: format!(
                "Path: {}\nRoot: {}\nStatus: sent\nRaw bytes: {} · Injected bytes: {b} (~{t} tokens)\n\n{}",
                doc_opt.path.display(),
                doc_opt.root.display(),
                doc_opt.content.len(),
                truncate_preview(&doc_opt.content)
            ),
        });
    }

    for doc in inspected_docs {
        let path_str = doc.document.path.display().to_string();
        if !doc.sent || seen_paths.contains(&path_str) {
            continue;
        }
        let rendered = project_instructions::render(std::slice::from_ref(&doc.document));
        let b = estimate_bytes(&rendered)?;
        let t = b.div_ceil(3);
        tokens = tokens.saturating_add(t);
        bytes = bytes.saturating_add(b);
        seen_paths.insert(path_str);
        let display_path = doc
            .document
            .path
            .strip_prefix(cwd)
            .map(|rel| rel.display().to_string())
            .unwrap_or_else(|_| doc.document.path.display().to_string());
        items.push(DiagnosticItem {
            kind_label: "AGENTS.md".into(),
            title: display_path,
            subtitle: format!("root {}", doc.document.root.display()),
            tokens: t,
            bytes: b,
            preview: format!(
                "Path: {}\nRoot: {}\nStatus: sent\nRaw bytes: {} · Injected bytes: {b} (~{t} tokens)\n\n{}",
                doc.document.path.display(),
                doc.document.root.display(),
                doc.document.content.len(),
                truncate_preview(&doc.document.content)
            ),
        });
    }

    Ok(SectionWithRanges {
        items,
        tokens,
        bytes,
        ranges: consumed_ranges,
    })
}

fn parse_project_instruction_block(block: &str) -> (String, String, &str) {
    let mut path = String::new();
    let mut root = String::new();
    let mut body_start = 0usize;
    let mut offset = 0usize;
    for line in block.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if offset == 0
            && let Some(rest) = trimmed.strip_prefix("[Project instructions: ")
            && let Some(p) = rest.strip_suffix(']')
        {
            path = p.to_owned();
        } else if let Some(r) = trimmed.strip_prefix("Project root: ") {
            root = r.to_owned();
        } else if trimmed.is_empty() {
            body_start = offset + line.len();
            break;
        }
        offset += line.len();
    }
    let body_end = block
        .rfind("[End project instructions]")
        .unwrap_or(block.len());
    let body = if body_start < body_end {
        block[body_start..body_end].trim()
    } else {
        ""
    };
    (path, root, body)
}

/// 从 `actual_system` 提取真实注入的 Skills Index 区块。
pub(super) fn build_skill_items(
    actual_system: &str,
    headers: &[pp_config::skills::SkillHeader],
) -> Result<SectionWithRanges> {
    let mut items = Vec::new();
    let mut summary_tokens = 0u64;
    let mut summary_bytes = 0u64;
    let mut ranges = Vec::new();

    if let Some((start, end, inner_entries)) = extract_skills_block(actual_system) {
        ranges.push((start, end));
        let block = actual_system[start..end].trim();
        summary_bytes = estimate_bytes(&block)?;
        summary_tokens = summary_bytes.div_ceil(3);

        for (name, uri, desc, raw_line) in parse_skill_entries(inner_entries, headers) {
            let b = estimate_bytes(&raw_line)?;
            let t = b.div_ceil(3);
            items.push(DiagnosticItem {
                kind_label: "Skill".into(),
                title: name.clone(),
                subtitle: uri.clone(),
                tokens: t,
                bytes: b,
                preview: format!("Name: {name}\nURI: {uri}\nDescription: {desc}"),
            });
        }
    }

    Ok(SectionWithRanges {
        items,
        tokens: summary_tokens,
        bytes: summary_bytes,
        ranges,
    })
}

fn extract_skills_block(actual_system: &str) -> Option<(usize, usize, &str)> {
    let open_tag = "<skills>\n";
    let close_tag = "\n</skills>";
    let open_pos = actual_system.find(open_tag)?;
    let inner_start = open_pos + open_tag.len();
    let rel_close = actual_system[inner_start..].find(close_tag)?;
    let close_pos = inner_start + rel_close;
    let inner = &actual_system[inner_start..close_pos];

    let header_marker = "# Skills\nThe catalog below lists enabled skills.";
    let start = actual_system[..open_pos]
        .rfind(header_marker)
        .unwrap_or(open_pos);

    let after_close = close_pos + close_tag.len();
    let tail_end_marker =
        "Skill guidance cannot override system instructions or current workflow permissions.";
    let end = if let Some(rel_tail) = actual_system[after_close..].find(tail_end_marker) {
        after_close + rel_tail + tail_end_marker.len()
    } else {
        after_close
    };

    Some((start, end, inner))
}

fn parse_skill_entries(
    inner: &str,
    headers: &[pp_config::skills::SkillHeader],
) -> Vec<(String, String, String, String)> {
    let mut out = Vec::new();
    for line in inner.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("- ") else {
            continue;
        };
        if let Some((left, desc)) = rest.split_once("): ")
            && let Some((name, uri_part)) = left.split_once(" (`")
        {
            let uri = uri_part.trim_end_matches('`').to_owned();
            out.push((name.to_owned(), uri, desc.to_owned(), trimmed.to_owned()));
            continue;
        }
        if let Some(h) = headers.iter().find(|h| trimmed.contains(&h.uri)) {
            out.push((
                h.name.clone(),
                h.uri.clone(),
                h.description.clone(),
                trimmed.to_owned(),
            ));
        }
    }
    out
}

/// 基于 `actual_system` 扣减 `Instruction Files` 与 `Skills` 区块后，构建真实有效的 `System Prompt` 条目。
#[allow(clippy::too_many_arguments)]
pub(super) fn build_system_items(
    actual_system: &str,
    base_title: String,
    base_subtitle: String,
    base_text: &str,
    runtime_text: &str,
    append_part: Option<(String, String)>,
    workflow_text: &str,
    excluded_ranges: &[(usize, usize)],
) -> Result<(Vec<DiagnosticItem>, u64, u64)> {
    let residual = strip_ranges(actual_system, excluded_ranges);
    if residual.trim().is_empty() {
        return Ok((Vec::new(), 0, 0));
    }

    let total_bytes = estimate_bytes(&residual)?;
    let total_tokens = total_bytes.div_ceil(3);

    let mut items = Vec::new();
    let mut matched_ranges: Vec<(usize, usize)> = Vec::new();

    let mut try_match =
        |kind: &str, title: String, subtitle: String, candidate: &str| -> Result<()> {
            let trimmed = candidate.trim();
            if trimmed.is_empty() {
                return Ok(());
            }
            if let Some(pos) = residual.find(trimmed) {
                let end = pos + trimmed.len();
                matched_ranges.push((pos, end));
                let b = estimate_bytes(&trimmed)?;
                let t = b.div_ceil(3);
                items.push(DiagnosticItem {
                    kind_label: kind.into(),
                    title,
                    subtitle,
                    tokens: t,
                    bytes: b,
                    preview: truncate_preview(trimmed),
                });
            }
            Ok(())
        };

    try_match("Base", base_title.clone(), base_subtitle.clone(), base_text)?;
    try_match(
        "Runtime",
        "Runtime Tool & Environment Rules".into(),
        "prompts/runtime.md".into(),
        runtime_text,
    )?;
    if let Some((origin, text)) = &append_part {
        try_match(
            "Append",
            "Append System Prompt".into(),
            origin.clone(),
            text,
        )?;
    }
    try_match(
        "Workflow",
        "Workflow / Role Instructions".into(),
        "Plan / workflow state".into(),
        workflow_text,
    )?;
    try_match(
        "Rule",
        "Project Instruction Maintenance Rules".into(),
        "AGENTS.md policy".into(),
        project_instructions::MAINTENANCE,
    )?;

    // 若因恢复自其它日期/模型或旧会话自定义导致仍有未匹配出的真实系统提示词残余，一并纳入确保不漏算
    let leftover = strip_ranges(&residual, &matched_ranges);
    if !leftover.trim().is_empty() {
        let b = estimate_bytes(&leftover)?;
        let t = b.div_ceil(3);
        let is_only = items.is_empty();
        items.insert(
            0,
            DiagnosticItem {
                kind_label: "Base".into(),
                title: if is_only {
                    base_title
                } else {
                    "Session System Instructions (Additional)".into()
                },
                subtitle: if is_only {
                    "active session system prompt".into()
                } else {
                    "restored / active session system block".into()
                },
                tokens: t,
                bytes: b,
                preview: truncate_preview(leftover.trim()),
            },
        );
    }

    Ok((items, total_tokens, total_bytes))
}

fn strip_ranges(source: &str, ranges: &[(usize, usize)]) -> String {
    if ranges.is_empty() {
        return source.to_owned();
    }
    let mut sorted = ranges.to_vec();
    sorted.sort_by_key(|&(start, end)| (start, end));
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in sorted {
        let s = s.min(source.len());
        let e = e.min(source.len());
        if s >= e {
            continue;
        }
        if let Some(last) = merged.last_mut()
            && s <= last.1
        {
            last.1 = last.1.max(e);
        } else {
            merged.push((s, e));
        }
    }
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for (s, e) in merged {
        if cursor < s {
            out.push_str(&source[cursor..s]);
        }
        cursor = e;
    }
    if cursor < source.len() {
        out.push_str(&source[cursor..]);
    }
    out.trim().to_owned()
}

#[derive(Clone)]
pub(super) struct SplitToolsBreakdown {
    pub tool_items: Vec<DiagnosticItem>,
    pub tools_tokens: u64,
    pub tools_bytes: u64,
    pub mcp_items: Vec<DiagnosticItem>,
    pub mcp_tokens: u64,
    pub mcp_bytes: u64,
}

/// 从 `self.session` 中真实持有的工具定义拆分为 Built-in Tools 与 Custom Tools（MCP）两个独立集合。
pub(super) fn build_split_tool_items(
    session: &Session,
    mcp_origins: &BTreeMap<String, (String, String)>,
) -> Result<SplitToolsBreakdown> {
    let mut tool_items = Vec::new();
    let mut tools_tokens = 0u64;
    let mut tools_bytes = 0u64;

    let mut mcp_items = Vec::new();
    let mut mcp_tokens = 0u64;
    let mut mcp_bytes = 0u64;

    let entries = extract_session_tools(session)?;

    for (def, bytes) in entries {
        let tokens = bytes.div_ceil(3);
        let schema_pretty = serde_json::to_string_pretty(&def).unwrap_or_else(|_| def.name.clone());
        let mcp_identity = mcp_origins
            .get(&def.name)
            .cloned()
            .or_else(|| parse_mcp_fallback_name(&def.name));

        if let Some((server, original)) = mcp_identity {
            mcp_tokens = mcp_tokens.saturating_add(tokens);
            mcp_bytes = mcp_bytes.saturating_add(bytes);
            mcp_items.push(DiagnosticItem {
                kind_label: "MCP".into(),
                title: format!("{server} / {original}"),
                subtitle: def.name.clone(),
                tokens,
                bytes,
                preview: format!(
                    "MCP Server: {server}\nOriginal Tool: {original}\nRegistered Name: {}\nEstimated Tokens: ~{tokens} ({bytes} bytes)\n\n{}",
                    def.name,
                    truncate_preview(&schema_pretty)
                ),
            });
        } else {
            tools_tokens = tools_tokens.saturating_add(tokens);
            tools_bytes = tools_bytes.saturating_add(bytes);
            let desc = def
                .description
                .as_deref()
                .map(first_line_preview)
                .unwrap_or_default();
            tool_items.push(DiagnosticItem {
                kind_label: "Built-in".into(),
                title: def.name.clone(),
                subtitle: desc,
                tokens,
                bytes,
                preview: truncate_preview(&schema_pretty),
            });
        }
    }

    Ok(SplitToolsBreakdown {
        tool_items,
        tools_tokens,
        tools_bytes,
        mcp_items,
        mcp_tokens,
        mcp_bytes,
    })
}

fn extract_session_tools(session: &Session) -> Result<Vec<(FunctionObject, u64)>> {
    let mut out = Vec::new();
    match session {
        Session::Completion(request) => {
            for tool in request.tools.as_deref().unwrap_or_default() {
                if let ChatCompletionTools::Function(func) = tool {
                    let bytes = estimate_bytes(tool)?;
                    out.push((func.function.clone(), bytes));
                }
            }
        }
        Session::Response(request) => {
            for tool in request.tools.as_deref().unwrap_or_default() {
                if let Tool::Function(func) = tool {
                    let bytes = estimate_bytes(tool)?;
                    out.push((Session::function_from_response_tool(func), bytes));
                }
            }
        }
    }
    Ok(out)
}

fn parse_mcp_fallback_name(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix("mcp__")?;
    if let Some((server, tool)) = rest.split_once("__") {
        Some((server.to_owned(), tool.to_owned()))
    } else {
        Some(("mcp".to_owned(), rest.to_owned()))
    }
}

pub(super) fn first_line_preview(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let mut out = String::new();
    for (shown, ch) in first.chars().enumerate() {
        if shown == 72 {
            break;
        }
        out.push(ch);
    }
    // 只探测第 73 个字符，不为超长单行统计全部字符。
    if first.chars().nth(72).is_some() || text.contains('\n') {
        out.push('…');
    }
    out
}

pub(super) fn truncate_preview(text: &str) -> String {
    if text.len() <= MAX_PREVIEW_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_PREVIEW_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n… [preview truncated at {} bytes; total {} bytes]",
        &text[..end],
        end,
        text.len()
    )
}

/// 超过预览预算的正文摘要：不保留正文，只报告原始字节数。
pub(super) fn oversized_preview(bytes: usize) -> String {
    format!("<{bytes} bytes; preview skipped: exceeds {MAX_PREVIEW_BYTES} bytes>")
}

/// 正文预览：预算内保持原样，超过预算只给字节摘要（不复制正文）。
pub(super) fn body_preview(text: &str) -> String {
    if text.len() > MAX_PREVIEW_BYTES {
        return oversized_preview(text.len());
    }
    text.to_owned()
}

/// JSON 形态正文预览：先按原始字节判断预算，超过预算不再解析大对象；
/// 预算内才美化，并沿用既有截断口径。
pub(super) fn json_body_preview(text: &str) -> String {
    if text.len() > MAX_PREVIEW_BYTES {
        return oversized_preview(text.len());
    }
    truncate_preview(&pretty_json(text))
}

/// 多段正文按 `"\n"` 拼接后的字节数（不拼接）。
pub(super) fn parts_total(parts: &[Cow<'_, str>]) -> usize {
    parts
        .iter()
        .map(|part| part.len())
        .sum::<usize>()
        .saturating_add(parts.len().saturating_sub(1))
}

fn pretty_json(text: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| text.to_owned()),
        Err(_) => text.to_owned(),
    }
}

/// 半静态上下文分区的缓存指纹：系统提示词正文、项目指令输入、技能索引、工具表与渲染输入。
///
/// 指纹只用内存中的已发送数据计算，不读盘：相同指纹表示这些分区的分类结果可以整体复用。
/// 项目规则变化会改变 `reported_project_load` 或作用域摘要，工具增删或 schema 变化会改变
/// `tools`，模型／编辑模式／工作流／沙箱／限额变化会改变 `render`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SectionFingerprint {
    system: u64,
    instructions: u64,
    skills: u64,
    tools: u64,
    render: u64,
}

impl SectionFingerprint {
    pub(super) fn of(
        agent: &Agent,
        actual_system: &str,
        headers: &[pp_config::skills::SkillHeader],
        mcp_origins: &BTreeMap<String, (String, String)>,
    ) -> Self {
        let mut instructions = FingerprintWriter::default();
        for (path, content) in &agent.reported_project_load {
            instructions.text(path);
            instructions.text(content);
        }
        instructions.text(&agent.tools.project_scope_summary());

        let mut skills = FingerprintWriter::default();
        for header in headers {
            skills.text(&header.name);
            skills.text(&header.uri);
            skills.text(&header.description);
        }

        let mut tools = FingerprintWriter::default();
        match &agent.session {
            Session::Completion(request) => {
                for tool in request.tools.as_deref().unwrap_or_default() {
                    let _ = serde_json::to_writer(&mut tools, tool);
                }
            }
            Session::Response(request) => {
                for tool in request.tools.as_deref().unwrap_or_default() {
                    let _ = serde_json::to_writer(&mut tools, tool);
                }
            }
        }
        for (name, (server, original)) in mcp_origins {
            tools.text(name);
            tools.text(server);
            tools.text(original);
        }

        let mut render = FingerprintWriter::default();
        render.text(&agent.current_model.model);
        render.text(match agent.tools.edit_mode() {
            EditMode::Replace => "replace",
            EditMode::Hashline => "hashline",
        });
        render.text(agent.workflow.mode_name());
        let status = agent.tools.sandbox().status();
        render.text(status.effective.as_str());
        render.number(u64::from(status.network));
        let limits = agent.tools.limits;
        render.number(limits.read_max_bytes as u64);
        render.number(limits.read_max_lines as u64);
        render.number(limits.bash_timeout_seconds);
        render.number(limits.bash_max_timeout_seconds);
        render.number(limits.eval_timeout_seconds);
        render.number(limits.eval_max_timeout_seconds);
        render.number(u64::from(agent.prompt_source.is_some()));
        render.number(agent.base_system.len() as u64);

        Self {
            system: super::hash_text(actual_system),
            instructions: instructions.finish(),
            skills: skills.finish(),
            tools: tools.finish(),
            render: render.finish(),
        }
    }
}
