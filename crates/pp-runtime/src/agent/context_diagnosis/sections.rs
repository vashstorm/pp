//! 静态与半静态上下文分块构建：系统提示词、AGENTS.md、工具/MCP Schema 与技能索引。

use super::{DiagnosticItem, MAX_PREVIEW_BYTES};
use crate::agent::Session;
use crate::project_instructions::{self, InspectedDocument};
use anyhow::Result;
use async_openai::types::{
    chat::{ChatCompletionTool, ChatCompletionTools},
    responses::{FunctionTool, Tool},
};
use pp_core::context::estimate_bytes;
use std::collections::BTreeMap;

#[allow(clippy::too_many_arguments)]
pub(super) fn build_system_items(
    base_title: String,
    base_subtitle: String,
    base_text: String,
    runtime_text: String,
    append_part: Option<(String, String)>,
    workflow_text: String,
    full_system: &str,
    session_system_matches: bool,
) -> Result<(Vec<DiagnosticItem>, u64, u64, usize)> {
    let mut items = Vec::new();
    let mut tokens = 0u64;
    let mut bytes = 0u64;
    let mut count = 0usize;

    let mut push = |kind: &str, title: String, subtitle: String, text: String| -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let b = estimate_bytes(&text)?;
        let t = b.div_ceil(3);
        tokens = tokens.saturating_add(t);
        bytes = bytes.saturating_add(b);
        count += 1;
        items.push(DiagnosticItem {
            kind_label: kind.into(),
            title,
            subtitle,
            tokens: t,
            bytes: b,
            preview: truncate_preview(&text),
            is_header: false,
            rankable: true,
        });
        Ok(())
    };

    push("Base", base_title, base_subtitle, base_text)?;
    push(
        "Runtime",
        "Runtime Tool & Environment Rules".into(),
        "prompts/runtime.md (rendered)".into(),
        runtime_text,
    )?;
    if let Some((origin, text)) = append_part {
        push("Append", "Append System Prompt".into(), origin, text)?;
    }
    push(
        "Workflow",
        "Workflow / Role Instructions".into(),
        "Plan / workflow state".into(),
        workflow_text,
    )?;

    let full_bytes = estimate_bytes(&full_system)?;
    let full_tokens = full_bytes.div_ceil(3);
    items.insert(
        0,
        DiagnosticItem {
            kind_label: "Summary Row".into(),
            title: "Full Composed System Prompt".into(),
            subtitle: if session_system_matches {
                "Matches active session system prompt".into()
            } else {
                "Prepared snapshot (refreshes before next request)".into()
            },
            tokens: full_tokens,
            bytes: full_bytes,
            preview: truncate_preview(full_system),
            is_header: true,
            rankable: false,
        },
    );

    Ok((items, tokens, bytes, count))
}

pub(super) fn build_agent_md_items(
    cwd: &std::path::Path,
    docs: &[InspectedDocument],
    scope_summary: &str,
) -> Result<(Vec<DiagnosticItem>, u64, u64, usize)> {
    let mut items = Vec::new();
    let mut tokens = 0u64;
    let mut bytes = 0u64;
    let mut count = 0usize;

    let maint_bytes = estimate_bytes(&project_instructions::MAINTENANCE)?;
    let maint_tokens = maint_bytes.div_ceil(3);
    tokens = tokens.saturating_add(maint_tokens);
    bytes = bytes.saturating_add(maint_bytes);
    count += 1;
    items.push(DiagnosticItem {
        kind_label: "Rule".into(),
        title: "AGENTS.md Maintenance Rules".into(),
        subtitle: "Fixed instruction header".into(),
        tokens: maint_tokens,
        bytes: maint_bytes,
        preview: project_instructions::MAINTENANCE.to_owned(),
        is_header: false,
        rankable: true,
    });

    if !scope_summary.trim().is_empty() {
        let b = estimate_bytes(&scope_summary)?;
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
            preview: scope_summary.trim().to_owned(),
            is_header: false,
            rankable: true,
        });
    }

    for doc in docs {
        let rendered = project_instructions::render(std::slice::from_ref(&doc.document));
        let b = estimate_bytes(&rendered)?;
        let t = b.div_ceil(3);
        tokens = tokens.saturating_add(t);
        bytes = bytes.saturating_add(b);
        count += 1;
        let display_path = doc
            .document
            .path
            .strip_prefix(cwd)
            .map(|rel| rel.display().to_string())
            .unwrap_or_else(|_| doc.document.path.display().to_string());
        let state = if doc.sent { "sent" } else { "pending" };
        items.push(DiagnosticItem {
            kind_label: "AGENTS.md".into(),
            title: display_path,
            subtitle: format!("{state} · root {}", doc.document.root.display()),
            tokens: t,
            bytes: b,
            preview: format!(
                "Path: {}\nRoot: {}\nStatus: {state}\nRaw bytes: {} · Injected bytes: {b} (~{t} tokens)\n\n{}",
                doc.document.path.display(),
                doc.document.root.display(),
                doc.document.content.len(),
                truncate_preview(&doc.document.content)
            ),
            is_header: false,
            rankable: true,
        });
    }

    Ok((items, tokens, bytes, count))
}

pub(super) fn build_tool_items(
    session: &Session,
    definitions: &[async_openai::types::chat::FunctionObject],
    mcp_origins: &BTreeMap<String, (String, String)>,
    mcp_view: &crate::mcp::View,
) -> Result<(Vec<DiagnosticItem>, u64, u64, usize)> {
    let mut items = Vec::new();
    let mut total_tokens = 0u64;
    let mut total_bytes = 0u64;
    let mut server_totals: BTreeMap<String, (usize, u64, u64)> = BTreeMap::new();

    for def in definitions {
        let bytes = match session {
            Session::Completion(_) => {
                estimate_bytes(&ChatCompletionTools::Function(ChatCompletionTool {
                    function: def.clone(),
                }))?
            }
            Session::Response(_) => estimate_bytes(&Tool::Function(FunctionTool {
                name: def.name.clone(),
                description: def.description.clone(),
                parameters: def.parameters.clone(),
                strict: def.strict,
                ..Default::default()
            }))?,
        };
        let tokens = bytes.div_ceil(3);
        total_tokens = total_tokens.saturating_add(tokens);
        total_bytes = total_bytes.saturating_add(bytes);

        let (kind_label, subtitle) = if let Some((server, original)) = mcp_origins.get(&def.name) {
            let entry = server_totals.entry(server.clone()).or_insert((0, 0, 0));
            entry.0 += 1;
            entry.1 = entry.1.saturating_add(bytes);
            entry.2 = entry.2.saturating_add(tokens);
            ("MCP", format!("server: {server} · tool: {original}"))
        } else {
            let desc = def
                .description
                .as_deref()
                .map(first_line_preview)
                .unwrap_or_default();
            ("Built-in", desc)
        };
        let schema_pretty = serde_json::to_string_pretty(def).unwrap_or_else(|_| def.name.clone());
        items.push(DiagnosticItem {
            kind_label: kind_label.into(),
            title: def.name.clone(),
            subtitle,
            tokens,
            bytes,
            preview: truncate_preview(&schema_pretty),
            is_header: false,
            rankable: true,
        });
    }

    if !mcp_view.entries.is_empty() {
        let mut mcp_tokens = 0u64;
        let mut mcp_bytes = 0u64;
        let mut lines = vec!["Configured MCP servers:\n".to_owned()];
        for entry in &mcp_view.entries {
            let (count, b, t) = server_totals.get(&entry.name).copied().unwrap_or((0, 0, 0));
            mcp_tokens = mcp_tokens.saturating_add(t);
            mcp_bytes = mcp_bytes.saturating_add(b);
            lines.push(format!(
                "- {} [{}] ({})  {} tools  ~{} tok  {} bytes",
                entry.name, entry.status, entry.source, count, t, b
            ));
        }
        items.insert(
            0,
            DiagnosticItem {
                kind_label: "Summary Row".into(),
                title: "MCP Servers Overview".into(),
                subtitle: format!("{} configured servers", mcp_view.entries.len()),
                tokens: mcp_tokens,
                bytes: mcp_bytes,
                preview: lines.join("\n"),
                is_header: true,
                rankable: false,
            },
        );
    }

    Ok((items, total_tokens, total_bytes, definitions.len()))
}

pub(super) fn build_skill_items(
    headers: &[pp_config::skills::SkillHeader],
    summary_text: &str,
    loaded_skills: &[DiagnosticItem],
) -> Result<(Vec<DiagnosticItem>, u64, u64, usize)> {
    let mut items = Vec::new();
    let summary_bytes = if summary_text.trim().is_empty() {
        0
    } else {
        estimate_bytes(&summary_text)?
    };
    let summary_tokens = summary_bytes.div_ceil(3);

    items.push(DiagnosticItem {
        kind_label: "Summary Row".into(),
        title: "Skills Index in System Prompt".into(),
        subtitle: format!(
            "{} enabled skills (bodies load on demand via read)",
            headers.len()
        ),
        tokens: summary_tokens,
        bytes: summary_bytes,
        preview: if summary_text.trim().is_empty() {
            "No skills enabled.".into()
        } else {
            truncate_preview(summary_text.trim())
        },
        is_header: true,
        rankable: false,
    });

    for header in headers {
        let entry_text = format!(
            "- {}: {} (URI: {})",
            header.name, header.description, header.uri
        );
        let b = estimate_bytes(&entry_text)?;
        let t = b.div_ceil(3);
        items.push(DiagnosticItem {
            kind_label: "Skill Index".into(),
            title: header.name.clone(),
            subtitle: header.uri.clone(),
            tokens: t,
            bytes: b,
            preview: format!(
                "Name: {}\nURI: {}\nDescription: {}",
                header.name, header.uri, header.description
            ),
            is_header: false,
            rankable: true,
        });
    }

    if !loaded_skills.is_empty() {
        let loaded_tokens: u64 = loaded_skills.iter().map(|i| i.tokens).sum();
        let loaded_bytes: u64 = loaded_skills.iter().map(|i| i.bytes).sum();
        items.push(DiagnosticItem {
            kind_label: "Summary Row".into(),
            title: "Loaded Skill Bodies (in History)".into(),
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

    Ok((items, summary_tokens, summary_bytes, headers.len()))
}

pub(super) fn completion_tools(
    definitions: &[async_openai::types::chat::FunctionObject],
) -> Vec<ChatCompletionTools> {
    definitions
        .iter()
        .cloned()
        .map(|function| ChatCompletionTools::Function(ChatCompletionTool { function }))
        .collect()
}

pub(super) fn response_tools(
    definitions: &[async_openai::types::chat::FunctionObject],
) -> Vec<Tool> {
    definitions
        .iter()
        .cloned()
        .map(|function| {
            Tool::Function(FunctionTool {
                name: function.name,
                description: function.description,
                parameters: function.parameters,
                strict: function.strict,
                ..Default::default()
            })
        })
        .collect()
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
