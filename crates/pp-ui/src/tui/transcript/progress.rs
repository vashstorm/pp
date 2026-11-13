//! 活动行的阶段文案与 `AgentEvent` 分派；整块 match 保持原顺序与早退语义。

use super::*;

impl Transcript {
    pub(super) fn progress_phase(&mut self, event: &AgentEvent) {
        if let Some(activity) = &mut self.activity {
            match event {
                AgentEvent::ModelAttemptFailed { .. } => activity.phase("Model attempt failed"),
                AgentEvent::ModelRouteChanged { .. } => activity.phase("Switching model"),
                AgentEvent::AssistantStarted => {
                    // 新请求只重置分片解析；整轮任务继续沿用最近一次摘要。
                    activity.summary_line.clear();
                    activity.summary_has_title = false;
                    activity.phase("Requesting model");
                }
                AgentEvent::ThinkingDelta { .. } => activity.phase("Thinking"),
                AgentEvent::ReasoningSummaryDelta { text } => {
                    activity.push_reasoning_summary(text);
                    activity.phase("Thinking");
                }
                AgentEvent::AssistantDelta { .. } => activity.phase("Responding"),
                AgentEvent::ToolArgumentsDelta { tool, .. } => {
                    activity.phase(&format!("Generating {}", pp_core::text::display_text(tool)))
                }
                AgentEvent::AssistantFinished { .. } => activity.phase("Processing response"),
                AgentEvent::ToolStarted { name, .. } => {
                    activity.phase(&format!("Running {}", name.replace(char::is_control, " ")))
                }
                AgentEvent::ToolFinished { .. } => activity.phase("Processing tool result"),
                AgentEvent::ToolOutput { .. }
                | AgentEvent::ToolOutputSnapshot { .. }
                | AgentEvent::ToolPipelineSnapshot { .. }
                | AgentEvent::ToolShellSnapshot { .. }
                | AgentEvent::FileChanged { .. }
                | AgentEvent::LspDiagnostics { .. } => {}
                // 后台自动加载不改变活动阶段，不伪装成模型工具执行。
                AgentEvent::ProjectInstructionsLoaded { .. } => {}
                AgentEvent::TodoSnapshot { .. } | AgentEvent::TodoStateUpdated { .. } => {}
                // 提醒通知不改变活动阶段。
                AgentEvent::TodoReminder { .. } => {}
                // 运行时指标只服务状态栏，不改变对话阶段。
                AgentEvent::ModelRequestStarted
                | AgentEvent::RequestRetrying { .. }
                | AgentEvent::ModelOutputProgress { .. }
                | AgentEvent::ModelRequestFinished { .. } => {}
                // 运行时上下文管理：自动压缩开始切换阶段；用量更新不影响活动显示。
                AgentEvent::ContextUpdated { .. } | AgentEvent::PlanProgressUpdated { .. } => {}
                AgentEvent::CompactionStarted { .. } => activity.phase("Compacting context"),
                AgentEvent::CompactionFinished { .. } => {}
                // 用户输入确认由 App 消费；不重置活动阶段、不产生重复卡片。
                AgentEvent::UserInputAccepted { .. } => {}
            }
        }
    }

    pub(in crate::tui) fn progress(&mut self, event: AgentEvent) {
        if matches!(
            event,
            AgentEvent::AssistantStarted
                | AgentEvent::AssistantFinished { .. }
                | AgentEvent::ModelAttemptFailed { .. }
        ) {
            self.clear_argument_previews();
        }
        if !self.replaying {
            self.progress_phase(&event);
        }
        match event {
            AgentEvent::ToolArgumentsDelta { tool, index, text } => {
                self.preview_arguments(index, &tool, &text);
            }
            // 用量更新不产生对话条目。
            AgentEvent::ContextUpdated { .. } | AgentEvent::PlanProgressUpdated { .. } => {}
            // 运行时指标只服务状态栏，不产生对话条目。
            AgentEvent::ModelRequestStarted
            | AgentEvent::RequestRetrying { .. }
            | AgentEvent::ModelOutputProgress { .. }
            | AgentEvent::ModelRequestFinished { .. } => {}
            // 已接受的补充输入由 App 直接追加，不生成重复用户卡。
            AgentEvent::UserInputAccepted { .. } => {}
            AgentEvent::CompactionStarted { .. } => {}
            // 完整任务只在固定区展示，旧日志的快照同样不生成历史卡片。
            AgentEvent::TodoSnapshot { .. } | AgentEvent::TodoStateUpdated { .. } => {}
            AgentEvent::ProjectInstructionsLoaded { path, content } => {
                // 自动加载的规则沿用普通 read 展示，不注册模型工具调用。
                let mut entry = TranscriptEntry::new(
                    EntryKind::Data,
                    "read".into(),
                    content,
                    Some("read".into()),
                );
                entry.arguments = serde_json::json!({"path": path});
                entry.display = self.display;
                entry.expanded = self.tools_expanded;
                self.read_groups_key = None;
                self.entries.push(entry);
            }
            AgentEvent::CompactionFinished {
                before_tokens,
                after_tokens,
            } => {
                // 简短结果用现有 Note 样式追加，保留原对话。
                self.append(
                    "Note",
                    &format!("Context compacted: ~{before_tokens} → ~{after_tokens} tokens"),
                );
            }
            AgentEvent::TodoReminder {
                todos,
                attempt,
                max_attempts,
            } => {
                // 现有 Note 通知列出未完成项；replay 只重放通知，不触发请求。
                let open: Vec<&pp_core::todo::TodoItem> = todos
                    .iter()
                    .filter(|item| {
                        !matches!(
                            item.status,
                            pp_core::todo::TodoStatus::Completed
                                | pp_core::todo::TodoStatus::Abandoned
                        )
                    })
                    .collect();
                let mut text = format!(
                    "Todo reminder {attempt}/{max_attempts}: {} incomplete todo item(s):",
                    open.len()
                );
                for item in &open {
                    text.push_str("\n- ");
                    text.push_str(&pp_core::text::display_text(&item.content));
                }
                self.append("Note", &text);
            }
            AgentEvent::ModelAttemptFailed {
                attempt,
                model,
                error,
            } => {
                for index in [self.assistant.take(), self.thinking.take()]
                    .into_iter()
                    .flatten()
                {
                    self.entries[index].finish_markdown(EntryStatus::Failed);
                }
                let mut text = format!(
                    "Model attempt {attempt} failed; partial output is excluded from history."
                );
                if !model.is_empty() {
                    text.push_str(&format!("\nModel: {model}"));
                }
                if !error.is_empty() {
                    text.push_str(&format!("\nReason: {error}"));
                }
                self.append_error(&text);
            }
            AgentEvent::ModelRouteChanged {
                model,
                effort,
                source,
                ..
            } => {
                // 切到备用模型意味着当前请求已失败或降级，必须显式告知；
                // 其余来源（手动、默认、角色）的切换只记一行普通说明。
                if source.is_fallback() {
                    self.append_notice(
                        "Fallback",
                        &format!(
                            "Primary model failed.\nUsing the fallback model.\nModel: {model} (effort {})\nApplies to the rest of this task;\nthe next task uses the primary again.",
                            effort.as_str()
                        ),
                    );
                } else {
                    self.append("Note", &format!("{}: {model}", source.describe()));
                }
            }
            AgentEvent::AssistantStarted => self.finish_markdown_streams(),
            AgentEvent::ThinkingDelta { text } | AgentEvent::ReasoningSummaryDelta { text } => {
                if text.is_empty() {
                    return;
                }
                if let Some(index) = self
                    .thinking
                    .filter(|index| *index + 1 == self.entries.len())
                {
                    let entry = &mut self.entries[index];
                    entry.text.push_str(&text);
                    entry.markdown_dirty = true;
                    entry.render.invalidate(false);
                    entry.layout = None;
                } else {
                    self.finish_thinking();
                    self.read_groups_key = None;
                    self.thinking = Some(self.entries.len());
                    let mut entry =
                        TranscriptEntry::new(EntryKind::Think, String::new(), text, None);
                    entry.status = EntryStatus::Running;
                    self.entries.push(entry);
                }
            }
            AgentEvent::AssistantDelta { text, refusal } => {
                if text.is_empty() {
                    return;
                }
                let index = self.assistant_entry();
                let entry = &mut self.entries[index];
                if refusal {
                    entry.refusal.push_str(&text);
                } else {
                    entry.text.push_str(&text);
                }
                entry.markdown_dirty = true;
                entry.render.invalidate(false);
                entry.layout = None;
            }
            AgentEvent::AssistantFinished { text } => {
                self.finish_thinking();
                if text.trim().is_empty() {
                    if let Some(index) = self.assistant.take() {
                        self.read_groups_key = None;
                        self.entries.remove(index);
                    }
                } else {
                    let index = self.assistant_entry();
                    let entry = &mut self.entries[index];
                    let previous = entry.visible_text();
                    if previous != text {
                        let replaced = !text.starts_with(previous);
                        entry.render.invalidate(replaced);
                        entry.markdown_dirty = true;
                        entry.layout = None;
                    }
                    entry.text = text;
                    entry.refusal.clear();
                    entry.finish_markdown(EntryStatus::Complete);
                }
                self.assistant = None;
            }
            AgentEvent::ToolStarted {
                id,
                name,
                arguments,
            } => {
                let ask_args = (name == "ask")
                    .then(|| pp_core::interaction::AskArgs::parse(&arguments).ok())
                    .flatten();
                let arguments = serde_json::from_str::<Value>(&arguments).ok();
                let lsp_block = (name == "lsp").then(crate::tui::lsp::Block::default);
                if name == "bash_task"
                    && let Some(index) = arguments
                        .as_ref()
                        .and_then(|args| self.background_bash_index(args))
                {
                    let entry = &mut self.entries[index];
                    entry.stdout.clear();
                    entry.stderr.clear();
                    entry.status = EntryStatus::Running;
                    entry.layout = None;
                    self.running_tools.push((id, index));
                    return;
                }
                let detail = arguments.as_ref().and_then(|args| {
                    args.get(match name.as_str() {
                        "bash" => "command",
                        _ => "path",
                    })
                    .and_then(Value::as_str)
                    .or_else(|| {
                        (name == "edit")
                            .then(|| pp_runtime::tools::edit_path(args))
                            .flatten()
                    })
                });
                let kind = if matches!(name.as_str(), "bash" | "bash_task") {
                    EntryKind::Bash
                } else if name == "rg" {
                    EntryKind::Rg
                } else if matches!(name.as_str(), "edit" | "write") {
                    EntryKind::Diff
                } else if name == "ask" {
                    EntryKind::Ask
                } else if name == "lsp" {
                    EntryKind::Lsp
                } else {
                    EntryKind::Data
                };
                let search_args = arguments
                    .as_ref()
                    .and_then(|args| args.get("args"))
                    .and_then(Value::as_array)
                    .filter(|args| args.iter().all(Value::is_string));
                let title = if name == "lsp" {
                    crate::tui::lsp::tool_title(arguments.as_ref().unwrap_or(&Value::Null))
                } else if name == "bash_task" {
                    let args = arguments.as_ref();
                    format!(
                        "bash_task · {} · {}",
                        args.and_then(|args| args["action"].as_str())
                            .unwrap_or("unknown"),
                        args.and_then(|args| args["task_id"].as_str())
                            .unwrap_or("unknown")
                    )
                } else if name == "todo" {
                    // 缺 op 时保持 Todo；完成后从结果修正推断操作。
                    match arguments.as_ref().and_then(|args| args["op"].as_str()) {
                        Some(op) => format!("Todo · {}", pp_core::text::display_text(op)),
                        None => "Todo".to_owned(),
                    }
                } else if matches!(name.as_str(), "rg" | "find") {
                    search_args.map_or_else(
                        || {
                            if name == "rg" {
                                let pattern = arguments
                                    .as_ref()
                                    .and_then(|a| a["pattern"].as_str())
                                    .unwrap_or("");
                                let path = arguments
                                    .as_ref()
                                    .and_then(|a| a["path"].as_str())
                                    .unwrap_or(".");
                                format!("rg · {pattern} · {path}")
                            } else {
                                name.clone()
                            }
                        },
                        |args| {
                            format!(
                                "{name} · {}",
                                serde_json::to_string(args).expect("JSON values serialize")
                            )
                        },
                    )
                } else {
                    // MCP 名称拆成 `mcp <server> <tool>`，避免 `mcp__…__…` 整串连写。
                    let label = palette::mcp_label(&name).unwrap_or_else(|| name.clone());
                    match detail {
                        Some(command) if name == "bash" => format!("$ {command}"),
                        Some(path) => format!("{label} · {path}"),
                        None => label,
                    }
                };
                let title = if matches!(kind, EntryKind::Diff | EntryKind::Ask) {
                    pp_core::text::display_text(&title).into_owned()
                } else {
                    title
                };
                let title = if name == "bash" || name == "lsp" {
                    title
                } else {
                    format!("{} {title}", palette::tool_icon(&name))
                };
                let mut entry = TranscriptEntry::new(kind, title, String::new(), Some(name));
                entry.arguments = arguments.unwrap_or(Value::Null);
                if entry.tool.as_deref() == Some("bash") && entry.arguments["mode"] != "background"
                {
                    entry.bash_timeout_seconds = Some(
                        entry.arguments["timeout_seconds"]
                            .as_u64()
                            .filter(|seconds| *seconds <= 3600)
                            .unwrap_or(self.bash_timeout_seconds),
                    );
                }
                entry.display = self.display;
                entry.expanded = self.tools_expanded;
                if let Some(args) = ask_args {
                    entry.body = EntryBody::Ask(AskBlock::new(args));
                }
                if let Some(block) = lsp_block {
                    entry.body = EntryBody::Lsp(block);
                }
                entry.status = EntryStatus::Running;
                self.read_groups_key = None;
                self.running_tools.push((id, self.entries.len()));
                self.entries.push(entry);
            }
            AgentEvent::FileChanged { id, path, diff } => {
                if let Some(index) = self.tool_index(&id) {
                    let entry = &mut self.entries[index];
                    if entry.kind == EntryKind::Diff {
                        entry.body = EntryBody::Diff(diff);
                        entry.layout = None;
                    } else if let EntryBody::Lsp(block) = &mut entry.body {
                        // LSP 的 rename/资源操作会按路径依次通知；卡片累积全部
                        // 文件，不能只保留最后一条 FileChanged。
                        block.record_change(path, diff);
                        entry.layout = None;
                        entry.display_title = None;
                    }
                }
            }
            AgentEvent::LspDiagnostics { id, report } => {
                if let Some(id) = id {
                    // 内联报告属于当前工具调用（工具结果仍是模型权威）；
                    // 这里只把同一份结构化报告附到卡片正文之后。
                    if let Some(index) = self.tool_index(&id) {
                        let entry = &mut self.entries[index];
                        if entry.tool.as_deref() == Some("lsp")
                            && !matches!(entry.body, EntryBody::Lsp(_))
                        {
                            entry.body = EntryBody::Lsp(crate::tui::lsp::Block::default());
                        }
                        entry.set_diagnostic(report);
                    }
                } else {
                    // `id = None` 是保存后的迟到报告：追加独立卡片，不改写已冻结
                    // 的历史卡片，也不启动新的模型请求。
                    let mut entry = TranscriptEntry::new(
                        EntryKind::Lsp,
                        crate::tui::lsp::late_title(),
                        String::new(),
                        None,
                    );
                    entry.display = self.display;
                    entry.expanded = self.tools_expanded;
                    entry.set_diagnostic(report);
                    self.read_groups_key = None;
                    self.entries.push(entry);
                }
            }
            AgentEvent::ToolShellSnapshot { id, snapshot } => {
                if snapshot.version == pp_core::shell_display::VERSION
                    && let Some(index) = self.tool_index(&id)
                {
                    let entry = &mut self.entries[index];
                    if entry.kind == EntryKind::Bash && entry.shell.as_ref() != Some(&snapshot) {
                        entry.shell = Some(snapshot);
                        entry.layout = None;
                    }
                }
            }
            AgentEvent::ToolPipelineSnapshot { id, pipes, preview } => {
                if let Some(index) = self.tool_index(&id) {
                    let replaying = self.replaying;
                    let entry = &mut self.entries[index];
                    if entry.kind == EntryKind::Bash && entry.result.is_none() {
                        if entry.pipeline_preview != preview {
                            entry.pipeline_preview = preview;
                            entry.layout = None;
                        }
                        let badge = pp_runtime::tools::output::pipes_status(&pipes).map(|status| {
                            PipesBadge {
                                status,
                                restored: replaying,
                            }
                        });
                        if entry.pipes_badge != badge {
                            entry.pipes_badge = badge;
                            entry.update_bash_footer(
                                entry
                                    .started
                                    .filter(|_| !replaying)
                                    .map(|started| started.elapsed()),
                            );
                            entry.layout = None;
                            entry.display_title = None;
                        }
                    }
                }
            }
            AgentEvent::ToolOutput { id, stream, text } => {
                if let Some(index) = self.tool_index(&id) {
                    let entry = &mut self.entries[index];
                    entry.pipeline_preview = None;
                    if entry.kind == EntryKind::Bash && entry.result.is_some() {
                        // 后台快照替换累计输出，不和上一轮正文重复拼接。
                        entry.text.clear();
                    }
                    match stream {
                        OutputStream::Stdout => entry.stdout.push_str(&text),
                        OutputStream::Stderr => entry.stderr.push_str(&text),
                    }
                    entry.layout = None;
                    entry.display_title = None;
                    if entry.tool.as_deref() == Some("read") {
                        self.read_groups_key = None;
                    }
                }
            }
            AgentEvent::ToolOutputSnapshot { id, output } => {
                if let Some(index) = self.tool_index(&id) {
                    let entry = &mut self.entries[index];
                    entry.pipeline_preview = None;
                    if entry.tool.as_deref() == Some("wait") {
                        if let Ok(snapshot) = serde_json::from_str::<Value>(&output) {
                            entry.result = Some(snapshot);
                            entry.layout = None;
                        }
                        return;
                    }
                    if entry.kind == EntryKind::Bash {
                        entry.text.clear();
                    }
                    entry.stdout = output;
                    entry.stderr.clear();
                    entry.layout = None;
                    entry.display_title = None;
                }
            }
            AgentEvent::ToolFinished { id, mut result } => {
                if self
                    .tool_index(&id)
                    .is_some_and(|index| self.entries[index].tool.as_deref() == Some("wait"))
                {
                    self.apply_wait_background(&result);
                }
                if let Some(position) = self.running_tools.iter().position(|(call, _)| call == &id)
                {
                    let (_, index) = self.running_tools.remove(position);
                    self.merge_background_bash_result(index, &mut result);
                    let entry = &mut self.entries[index];
                    entry.status = if result.get("ok").and_then(Value::as_bool) == Some(false)
                        || (entry.kind == EntryKind::Bash
                            && (result["exit_code"].as_i64().is_some_and(|code| code != 0)
                                || result["timed_out"] == true
                                || result["cancelled"] == true))
                    {
                        EntryStatus::Failed
                    } else if entry.kind == EntryKind::Bash && result["status"] == "running" {
                        EntryStatus::Running
                    } else {
                        EntryStatus::Complete
                    };
                    if entry.tool.as_deref() == Some("todo")
                        && let Some(op) = result["op"].as_str()
                    {
                        // 成功结果携带真实操作；修正开始时无法推断的标题。
                        entry.title = format!(
                            "{} Todo · {}",
                            palette::tool_icon("todo"),
                            pp_core::text::display_text(op)
                        );
                        entry.layout = None;
                    }
                    entry.stdout.clear();
                    entry.stderr.clear();
                    entry.pipeline_preview = None;
                    if entry.tool.as_deref() == Some("lsp") {
                        // 工具结果是内联权威；结构化 diagnostics 直接渲染，
                        // 同一份内容的可读正文不再重复；其他动作展示 content。
                        let reports = crate::tui::lsp::reports_from_result(&result);
                        let text = tool_result(entry.kind, "lsp", &result);
                        let content = reports
                            .is_empty()
                            .then_some(text)
                            .filter(|text| !text.is_empty());
                        if let EntryBody::Lsp(block) = &mut entry.body {
                            block.set_content(content);
                        } else {
                            let mut block = crate::tui::lsp::Block::default();
                            block.set_content(content);
                            entry.body = EntryBody::Lsp(block);
                        }
                        for report in reports {
                            entry.set_diagnostic(report);
                        }
                    }
                    if let EntryBody::Ask(block) = &mut entry.body {
                        if !block.finish(&result) {
                            entry.body = EntryBody::Plain;
                            entry.status = EntryStatus::Failed;
                            entry.text = "invalid ask response".into();
                        } else {
                            entry.text = if entry.status == EntryStatus::Failed {
                                tool_result(entry.kind, "ask", &result)
                            } else {
                                String::new()
                            };
                        }
                    } else {
                        entry.text = if matches!(entry.body, EntryBody::Diff(_))
                            && entry.status == EntryStatus::Complete
                        {
                            result
                                .get("warning")
                                .and_then(Value::as_str)
                                .map(|warning| format!("Warning: {warning}"))
                                .unwrap_or_default()
                        } else if matches!(entry.body, EntryBody::Lsp(_)) {
                            String::new()
                        } else {
                            tool_result(
                                entry.kind,
                                entry.tool.as_deref().unwrap_or_default(),
                                &result,
                            )
                        };
                    }
                    if entry.diagnostics.is_empty() {
                        for report in crate::tui::lsp::reports_from_result(&result) {
                            entry.set_diagnostic(report);
                        }
                    }
                    if let Some(shell) = result
                        .get("shell")
                        .and_then(|value| {
                            serde_json::from_value::<pp_core::shell_display::ShellPresentation>(
                                value.clone(),
                            )
                            .ok()
                        })
                        .filter(|shell| shell.version == pp_core::shell_display::VERSION)
                    {
                        entry.shell = Some(shell);
                    }
                    entry.result = Some(result);
                    if entry.kind == EntryKind::Bash {
                        // 最终结果优先于运行中的瞬时快照；没有真实管道时清空入口。
                        entry.sync_pipes_badge(self.replaying);
                        entry.update_bash_footer(
                            entry
                                .started
                                .filter(|_| !self.replaying)
                                .map(|started| started.elapsed()),
                        );
                    }
                    entry.layout = None;
                    entry.display_title = None;
                    if entry.tool.as_deref() == Some("read") {
                        self.read_groups_key = None;
                    }
                }
            }
        }
    }
}
