//! 基于当前会话真实数据的半静态上下文分块构建：
//! 系统提示词、已发送的 AGENTS.md、技能索引、内置工具 Schema 与 MCP 工具 Schema。

use super::{DiagnosticItem, MAX_PREVIEW_BYTES};
use crate::agent::Session;
use crate::project_instructions::{self, InspectedDocument};
use anyhow::Result;
use async_openai::types::{
    chat::{ChatCompletionTools, FunctionObject},
    responses::Tool,
};
use pp_core::context::estimate_bytes;
use std::collections::BTreeMap;

pub(super) struct SectionWithRanges {
    pub items: Vec<DiagnosticItem>,
    pub tokens: u64,
    pub bytes: u64,
    pub count: usize,
    pub ranges: Vec<(usize, usize)>,
}

/// 从 `actual_system` 中剥离并构建已真实注入的 `AGENTS.md` 规则条目。
pub(super) fn build_agent_md_items(
    cwd: &std::path::Path,
    actual_system: &str,
    reported_loads: &BTreeMap<String, String>,
    inspected_docs: &[InspectedDocument],
) -> Result<SectionWithRanges> {
    let mut items = Vec::new();
    let mut tokens = 0u64;
    let mut bytes = 0u64;
    let mut count = 0usize;
    let mut consumed_ranges: Vec<(usize, usize)> = Vec::new();

    if let Some(pos) = actual_system.find(project_instructions::MAINTENANCE) {
        let len = project_instructions::MAINTENANCE.len();
        consumed_ranges.push((pos, pos + len));
        let b = estimate_bytes(&project_instructions::MAINTENANCE)?;
        let t = b.div_ceil(3);
        tokens = tokens.saturating_add(t);
        bytes = bytes.saturating_add(b);
        count += 1;
        items.push(DiagnosticItem {
            kind_label: "Rule".into(),
            title: "AGENTS.md Maintenance Rules".into(),
            subtitle: "Fixed instruction header in session".into(),
            tokens: t,
            bytes: b,
            preview: project_instructions::MAINTENANCE.to_owned(),
            is_header: false,
            rankable: true,
        });
    }

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
        count += 1;
        items.push(DiagnosticItem {
            kind_label: "Boundary".into(),
            title: "Project Root Boundaries".into(),
            subtitle: "Nested git root isolation notices".into(),
            tokens: t,
            bytes: b,
            preview: boundaries_text,
            is_header: false,
            rankable: true,
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
        count += 1;
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
                format!("sent · root {root_str}")
            },
            tokens: t,
            bytes: b,
            preview: format!(
                "Path: {path_str}\nRoot: {}\nStatus: sent in session\nRaw bytes: {} · Injected bytes: {b} (~{t} tokens)\n\n{}",
                if root_str.is_empty() { "—" } else { &root_str },
                body.len(),
                truncate_preview(body),
            ),
            is_header: false,
            rankable: true,
        });
        pos = end;
    }

    // 兜底：对于通过 `reported_loads` 或 `doc.sent` 确认已发送、但未出现在 system 文本（如测试构造）中的文档
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
        count += 1;
        seen_paths.insert(path_str.clone());
        let display_path = doc_opt
            .path
            .strip_prefix(cwd)
            .map(|rel| rel.display().to_string())
            .unwrap_or_else(|_| path_str.clone());
        items.push(DiagnosticItem {
            kind_label: "AGENTS.md".into(),
            title: display_path,
            subtitle: format!("sent · root {}", doc_opt.root.display()),
            tokens: t,
            bytes: b,
            preview: format!(
                "Path: {}\nRoot: {}\nStatus: sent\nRaw bytes: {} · Injected bytes: {b} (~{t} tokens)\n\n{}",
                doc_opt.path.display(),
                doc_opt.root.display(),
                doc_opt.content.len(),
                truncate_preview(&doc_opt.content)
            ),
            is_header: false,
            rankable: true,
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
        count += 1;
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
            subtitle: format!("sent · root {}", doc.document.root.display()),
            tokens: t,
            bytes: b,
            preview: format!(
                "Path: {}\nRoot: {}\nStatus: sent\nRaw bytes: {} · Injected bytes: {b} (~{t} tokens)\n\n{}",
                doc.document.path.display(),
                doc.document.root.display(),
                doc.document.content.len(),
                truncate_preview(&doc.document.content)
            ),
            is_header: false,
            rankable: true,
        });
    }

    Ok(SectionWithRanges {
        items,
        tokens,
        bytes,
        count,
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
    loaded_skills: &[DiagnosticItem],
) -> Result<SectionWithRanges> {
    let mut items = Vec::new();
    let mut summary_tokens = 0u64;
    let mut summary_bytes = 0u64;
    let mut index_count = 0usize;
    let mut ranges = Vec::new();

    if let Some((start, end, inner_entries)) = extract_skills_block(actual_system) {
        ranges.push((start, end));
        let block = actual_system[start..end].trim();
        summary_bytes = estimate_bytes(&block)?;
        summary_tokens = summary_bytes.div_ceil(3);

        let parsed_entries = parse_skill_entries(inner_entries, headers);
        index_count = parsed_entries.len();

        items.push(DiagnosticItem {
            kind_label: "Summary Row".into(),
            title: "Skills Index in System Prompt".into(),
            subtitle: format!("{index_count} skills listed in session prompt"),
            tokens: summary_tokens,
            bytes: summary_bytes,
            preview: truncate_preview(block),
            is_header: true,
            rankable: false,
        });

        for (name, uri, desc, raw_line) in parsed_entries {
            let b = estimate_bytes(&raw_line)?;
            let t = b.div_ceil(3);
            items.push(DiagnosticItem {
                kind_label: "Skill Index".into(),
                title: name.clone(),
                subtitle: uri.clone(),
                tokens: t,
                bytes: b,
                preview: format!("Name: {name}\nURI: {uri}\nDescription: {desc}"),
                is_header: false,
                rankable: true,
            });
        }
    }

    if !loaded_skills.is_empty() {
        let loaded_tokens: u64 = loaded_skills.iter().map(|i| i.tokens).sum();
        let loaded_bytes: u64 = loaded_skills.iter().map(|i| i.bytes).sum();
        items.push(DiagnosticItem {
            kind_label: "Summary Row".into(),
            title: "Loaded Skill Bodies (in Tool Results)".into(),
            subtitle: format!(
                "{} read(skill://...) results (counted under Tool Results)",
                loaded_skills.len()
            ),
            tokens: loaded_tokens,
            bytes: loaded_bytes,
            preview: loaded_skills
                .iter()
                .map(|item| {
                    format!(
                        "- {}  ~{} tok  ({})",
                        item.title, item.tokens, item.subtitle
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            is_header: true,
            rankable: false,
        });
        items.extend(loaded_skills.iter().cloned());
    }

    Ok(SectionWithRanges {
        items,
        tokens: summary_tokens,
        bytes: summary_bytes,
        count: index_count,
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

/// 基于 `actual_system` 扣减 `AGENTS.md` 与 `Skills` 区块后，构建真实有效的 `System Prompt` 条目。
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
) -> Result<(Vec<DiagnosticItem>, u64, u64, usize)> {
    let residual = strip_ranges(actual_system, excluded_ranges);
    if residual.trim().is_empty() {
        return Ok((Vec::new(), 0, 0, 0));
    }

    let total_bytes = estimate_bytes(&residual)?;
    let total_tokens = total_bytes.div_ceil(3);

    let mut items = Vec::new();
    let mut matched_ranges: Vec<(usize, usize)> = Vec::new();
    let mut count = 0usize;

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
                count += 1;
                items.push(DiagnosticItem {
                    kind_label: kind.into(),
                    title,
                    subtitle,
                    tokens: t,
                    bytes: b,
                    preview: truncate_preview(trimmed),
                    is_header: false,
                    rankable: true,
                });
            }
            Ok(())
        };

    try_match("Base", base_title.clone(), base_subtitle.clone(), base_text)?;
    try_match(
        "Runtime",
        "Runtime Tool & Environment Rules".into(),
        "prompts/runtime.md (rendered)".into(),
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

    // 若因恢复自其它日期/模型或旧会话自定义导致仍有未匹配出的真实系统提示词残余，一并纳入确保不漏算
    let leftover = strip_ranges(&residual, &matched_ranges);
    if !leftover.trim().is_empty() {
        let b = estimate_bytes(&leftover)?;
        let t = b.div_ceil(3);
        count += 1;
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
                is_header: false,
                rankable: true,
            },
        );
    }

    items.insert(
        0,
        DiagnosticItem {
            kind_label: "Summary Row".into(),
            title: "Active System Prompt (excluding AGENTS.md & Skills)".into(),
            subtitle: format!("{count} system instruction section(s) in session"),
            tokens: total_tokens,
            bytes: total_bytes,
            preview: truncate_preview(residual.trim()),
            is_header: true,
            rankable: false,
        },
    );

    Ok((items, total_tokens, total_bytes, count))
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

pub(super) struct SplitToolsBreakdown {
    pub tool_items: Vec<DiagnosticItem>,
    pub tools_tokens: u64,
    pub tools_bytes: u64,
    pub tools_count: usize,
    pub mcp_items: Vec<DiagnosticItem>,
    pub mcp_tokens: u64,
    pub mcp_bytes: u64,
    pub mcp_count: usize,
}

/// 从 `self.session` 中真实持有的工具定义拆分为 Built-in `Tools` 与 `MCP` 两个独立集合。
pub(super) fn build_split_tool_items(
    session: &Session,
    mcp_origins: &BTreeMap<String, (String, String)>,
    mcp_view: &crate::mcp::View,
) -> Result<SplitToolsBreakdown> {
    let mut tool_items = Vec::new();
    let mut tools_tokens = 0u64;
    let mut tools_bytes = 0u64;
    let mut tools_count = 0usize;

    let mut mcp_items = Vec::new();
    let mut mcp_tokens = 0u64;
    let mut mcp_bytes = 0u64;
    let mut mcp_count = 0usize;
    let mut server_totals: BTreeMap<String, (usize, u64, u64)> = BTreeMap::new();

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
            mcp_count += 1;
            let entry = server_totals.entry(server.clone()).or_insert((0, 0, 0));
            entry.0 += 1;
            entry.1 = entry.1.saturating_add(bytes);
            entry.2 = entry.2.saturating_add(tokens);

            mcp_items.push(DiagnosticItem {
                kind_label: "MCP".into(),
                title: format!("{server} / {original}"),
                subtitle: format!("{} · {}", def.name, def.description.as_deref().map(first_line_preview).unwrap_or_default()),
                tokens,
                bytes,
                preview: format!(
                    "MCP Server: {server}\nOriginal Tool: {original}\nRegistered Name: {}\nEstimated Tokens: ~{tokens} ({bytes} bytes)\n\n{}",
                    def.name,
                    truncate_preview(&schema_pretty)
                ),
                is_header: false,
                rankable: true,
            });
        } else {
            tools_tokens = tools_tokens.saturating_add(tokens);
            tools_bytes = tools_bytes.saturating_add(bytes);
            tools_count += 1;
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
                is_header: false,
                rankable: true,
            });
        }
    }

    if !mcp_items.is_empty() || !mcp_view.entries.is_empty() {
        let mut overview_lines = Vec::new();
        let mut seen_servers = std::collections::BTreeSet::new();
        for entry in &mcp_view.entries {
            seen_servers.insert(entry.name.clone());
            let (count, b, t) = server_totals.get(&entry.name).copied().unwrap_or((0, 0, 0));
            overview_lines.push(format!(
                "- {} [{}] ({})  {} tools  ~{} tok  {} bytes",
                entry.name, entry.status, entry.source, count, t, b
            ));
        }
        for (server, (count, b, t)) in &server_totals {
            if !seen_servers.contains(server) {
                overview_lines.push(format!(
                    "- {server} [saved in session]  {count} tools  ~{t} tok  {b} bytes"
                ));
            }
        }
        if !overview_lines.is_empty() {
            let server_count = seen_servers.len().max(server_totals.len());
            mcp_items.insert(
                0,
                DiagnosticItem {
                    kind_label: "Summary Row".into(),
                    title: "MCP Servers Overview".into(),
                    subtitle: format!("{server_count} server(s) · {mcp_count} tools in session"),
                    tokens: mcp_tokens,
                    bytes: mcp_bytes,
                    preview: format!(
                        "Active MCP tools in session: {mcp_count} (~{mcp_tokens} tokens, {mcp_bytes} bytes)\n\nServers:\n{}",
                        overview_lines.join("\n")
                    ),
                    is_header: true,
                    rankable: false,
                },
            );
        }
    }

    Ok(SplitToolsBreakdown {
        tool_items,
        tools_tokens,
        tools_bytes,
        tools_count,
        mcp_items,
        mcp_tokens,
        mcp_bytes,
        mcp_count,
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
                    out.push((
                        FunctionObject {
                            name: func.name.clone(),
                            description: func.description.clone(),
                            parameters: func.parameters.clone(),
                            strict: func.strict,
                        },
                        bytes,
                    ));
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
    for ch in first.chars().take(72) {
        out.push(ch);
    }
    if first.chars().count() > 72 || text.contains('\n') {
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
