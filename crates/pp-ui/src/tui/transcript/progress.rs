//! Stage copy for activity lines and `AgentEvent` dispatch; the whole match keeps its original order and early-return semantics.

use super::*;

impl Transcript {
    pub(super) fn progress_phase(&mut self, event: &AgentEvent) {
        if let Some(activity) = &mut self.activity {
            match event {
                AgentEvent::ModelAttemptFailed { .. } => activity.phase("Model attempt failed"),
                AgentEvent::ModelRouteChanged { .. } => activity.phase("Switching model"),
                AgentEvent::AssistantStarted => {
                    // A new request only resets chunk parsing; the whole turn keeps the most recent summary.
                    activity.summary_line.clear();
                    activity.summary_has_title = false;
                    activity.phase("Requesting model");
                }
                AgentEvent::ThinkingDelta { .. } => activity.phase("Thinking"),
                AgentEvent::ReasoningSummaryDelta { text } => {
                    activity.push_reasoning_summary(text, Instant::now());
                    activity.phase("Thinking");
                }
                AgentEvent::AssistantDelta { .. } => activity.phase("Responding"),
                AgentEvent::ToolArgumentsDelta { tool, .. } => {
                    activity.phase(&format!("Generating {}", palette::tool_label(tool)))
                }
                AgentEvent::AssistantFinished { .. } => activity.phase("Processing response"),
                AgentEvent::ToolStarted { name, .. } => {
                    activity.phase(&format!("Running {}", palette::tool_label(name)))
                }
                AgentEvent::ToolFinished { .. } => activity.phase("Processing tool result"),
                AgentEvent::ToolOutput { .. }
                | AgentEvent::ToolOutputSnapshot { .. }
                | AgentEvent::ToolPipelineSnapshot { .. }
                | AgentEvent::ToolShellSnapshot { .. }
                | AgentEvent::SubagentResultsInjected { .. }
                | AgentEvent::FileChanged { .. }
                | AgentEvent::LspDiagnostics { .. } => {}
                // Background auto-loading does not change the activity stage and is not disguised as a model tool run.
                AgentEvent::ProjectInstructionsLoaded { .. } => {}
                AgentEvent::TodoSnapshot { .. } | AgentEvent::TodoStateUpdated { .. } => {}
                // Reminder notifications do not change the activity stage.
                AgentEvent::TodoReminder { .. } => {}
                // Runtime metrics only serve the status bar and do not change the conversation stage.
                AgentEvent::ModelRequestStarted
                | AgentEvent::RequestRetrying { .. }
                | AgentEvent::ModelOutputProgress { .. }
                | AgentEvent::ModelRequestFinished { .. } => {}
                // Runtime context management: a compaction start switches the stage; usage updates do not affect the activity display.
                AgentEvent::ContextUpdated { .. } | AgentEvent::PlanProgressUpdated { .. } => {}
                AgentEvent::CompactionStarted { .. } => activity.phase("Compacting context"),
                AgentEvent::CompactionFinished { .. } => {}
                // User input acknowledgements are consumed by App; they do not reset the activity stage or produce duplicate cards.
                AgentEvent::UserInputAccepted { .. } | AgentEvent::UserInputRejected { .. } => {}
                // Hook diagnostics and auto_review only hint the stage; they do not change the conversation record structure.
                AgentEvent::HookDiagnostic { .. } => {}
                AgentEvent::AutoReviewStarted { tool } => {
                    activity.phase(&format!("Reviewing {}", palette::tool_label(tool)));
                }
                AgentEvent::AutoReviewFinished { .. } => {}
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
            // Usage updates produce no conversation entry.
            AgentEvent::ContextUpdated { .. }
            | AgentEvent::PlanProgressUpdated { .. }
            | AgentEvent::AutoReviewStarted { .. }
            | AgentEvent::AutoReviewFinished { .. } => {}
            AgentEvent::SubagentResultsInjected { run_ids } => {
                self.injected_subagent_runs.extend(run_ids);
                for entry in &mut self.entries {
                    if entry.tool.as_deref() == Some("wait")
                        && let Some(result) = &mut entry.result
                        && mark_injected_jobs(result, &self.injected_subagent_runs)
                    {
                        entry.layout = None;
                    }
                }
            }
            // Runtime metrics only serve the status bar and produce no conversation entry.
            AgentEvent::ModelRequestStarted
            | AgentEvent::RequestRetrying { .. }
            | AgentEvent::ModelOutputProgress { .. }
            | AgentEvent::ModelRequestFinished { .. } => {}
            // Accepted follow-up input is appended directly by App without duplicating the user card.
            AgentEvent::UserInputAccepted { .. } => {}
            // Rejected follow-up input is appended by App as a rejection card and never enters the normal conversation stream.
            AgentEvent::UserInputRejected { .. } => {}
            // Hook diagnostics reuse the Note style: display only, never entering the model context.
            AgentEvent::HookDiagnostic { event, message } => {
                self.append("Note", &format!("{event} hook: {message}"));
            }
            AgentEvent::CompactionStarted { .. } => {}
            // The full task is only shown in the fixed area; snapshots from old logs likewise produce no history card.
            AgentEvent::TodoSnapshot { .. } | AgentEvent::TodoStateUpdated { .. } => {}
            AgentEvent::ProjectInstructionsLoaded { path, content } => {
                // Auto-loaded instructions reuse the normal read display and register no model tool call.
                let mut entry = TranscriptEntry::new(
                    EntryKind::Data,
                    "read".into(),
                    content,
                    Some("read".into()),
                );
                entry.cwd = self.cwd.clone();
                entry.arguments = serde_json::json!({"path": path});
                entry.display = self.display;
                entry.expanded = self.tools_expanded && entry.is_collapsible_tool();
                self.read_groups_key = None;
                self.entries.push(entry);
            }
            AgentEvent::CompactionFinished {
                before_tokens,
                after_tokens,
            } => {
                // Brief results are appended with the existing Note style, preserving the original conversation.
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
                // The existing Note notification lists unfinished items; replay only replays the notification and triggers no request.
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
                // Switching to the fallback model means the current request already failed or degraded, so it must be announced explicitly;
                // switches from other sources (manual, default, role) only record one ordinary note.
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
                            .then(|| pp_tools::edit_path(args))
                            .flatten()
                    })
                });
                let kind = if matches!(name.as_str(), "bash" | "bash_task") {
                    EntryKind::Bash
                } else if name == "eval" {
                    EntryKind::Eval
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
                let label = palette::tool_label(&name);
                let title = if name == "lsp" {
                    crate::tui::lsp::tool_title(arguments.as_ref().unwrap_or(&Value::Null))
                } else if name == "eval" {
                    let args = arguments.as_ref();
                    let lang =
                        tool_display::EvalLang::from_raw(args.and_then(|a| a["language"].as_str()));
                    let label = args
                        .and_then(|a| a["title"].as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty());
                    match label {
                        Some(label) => format!("{} · {label}", lang.label()),
                        None => lang.label().to_owned(),
                    }
                } else if name == "bash_task" {
                    let args = arguments.as_ref();
                    format!(
                        "{label} · {} · {}",
                        args.and_then(|args| args["action"].as_str())
                            .unwrap_or("unknown"),
                        args.and_then(|args| args["task_id"].as_str())
                            .unwrap_or("unknown")
                    )
                } else if name == "todo" {
                    // Keep Todo when op is missing; infer the operation from the result correction once it completes.
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
                                format!("{label} · {pattern} · {path}")
                            } else {
                                label.to_string()
                            }
                        },
                        |args| {
                            format!(
                                "{label} · {}",
                                serde_json::to_string(args).expect("JSON values serialize")
                            )
                        },
                    )
                } else {
                    match detail {
                        Some(command) if name == "bash" => format!("$ {command}"),
                        Some(path) if matches!(name.as_str(), "read" | "write" | "edit") => {
                            let path = super::entry_tool::display_path(self.cwd.as_deref(), path);
                            format!("{label} · {path}")
                        }
                        Some(path) => format!("{label} · {path}"),
                        None => label.into_owned(),
                    }
                };
                let title = if matches!(kind, EntryKind::Diff | EntryKind::Ask) {
                    pp_core::text::display_text(&title).into_owned()
                } else {
                    title
                };
                let title = if matches!(name.as_str(), "bash" | "lsp" | "eval") {
                    title
                } else {
                    format!("{} {title}", palette::tool_icon(&name))
                };
                let mut entry = TranscriptEntry::new(kind, title, String::new(), Some(name));
                entry.cwd = self.cwd.clone();
                entry.arguments = arguments.unwrap_or(Value::Null);
                if entry.tool.as_deref() == Some("bash") && entry.arguments["mode"] != "background"
                {
                    entry.bash_timeout_seconds = Some(
                        entry.arguments["timeout_seconds"]
                            .as_u64()
                            .unwrap_or(self.bash_timeout_seconds),
                    );
                } else if entry.tool.as_deref() == Some("eval") {
                    entry.eval_timeout_seconds =
                        Some(entry.arguments["timeout_seconds"].as_u64().unwrap_or(
                            pp_config::config::ToolLimits::default().eval_timeout_seconds,
                        ));
                }
                entry.display = self.display;
                entry.expanded = self.tools_expanded && entry.is_collapsible_tool();
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
                        // LSP rename/resource operations notify per path in sequence; the card accumulates every
                        // file rather than keeping only the last FileChanged.
                        block.record_change(path, diff);
                        entry.layout = None;
                        entry.display_title = None;
                    }
                }
            }
            AgentEvent::LspDiagnostics { id, report } => {
                if let Some(id) = id {
                    // Inline reports belong to the current tool call (the tool result remains the model authority);
                    // here the same structured report is only appended after the card body.
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
                    // `id = None` is a late report after saving: append a standalone card, do not rewrite the already frozen
                    // history card, and do not start a new model request.
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
                        let badge =
                            pp_workspace::output::pipes_status(&pipes).map(|status| PipesBadge {
                                status,
                                restored: replaying,
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
                        // Background snapshots replace the accumulated output instead of appending a duplicate of the previous body.
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
                        if let Ok(mut snapshot) = serde_json::from_str::<Value>(&output) {
                            mark_injected_jobs(&mut snapshot, &self.injected_subagent_runs);
                            entry.result = Some(snapshot);
                            entry.layout = None;
                        }
                        return;
                    }
                    if matches!(entry.kind, EntryKind::Bash | EntryKind::Eval) {
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
                    mark_injected_jobs(&mut result, &self.injected_subagent_runs);
                }
                if let Some(position) = self.running_tools.iter().position(|(call, _)| call == &id)
                {
                    let (_, index) = self.running_tools.remove(position);
                    self.merge_background_bash_result(index, &mut result);
                    let entry = &mut self.entries[index];
                    entry.status = if result.get("ok").and_then(Value::as_bool) == Some(false)
                        || (matches!(entry.kind, EntryKind::Bash | EntryKind::Eval)
                            && (result["timed_out"] == true || result["cancelled"] == true))
                        || (entry.kind == EntryKind::Bash
                            && result["exit_code"].as_i64().is_some_and(|code| code != 0))
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
                        // Successful results carry the real operation; fix the title that could not be inferred when it started.
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
                        // The tool result is the inline authority; structured diagnostics render directly,
                        // so the readable body of the same content is not repeated; other actions show content.
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
                        // The final result wins over in-flight snapshots; clear the entry when there is no real pipeline.
                        entry.sync_pipes_badge(self.replaying);
                        entry.update_bash_footer(
                            entry
                                .started
                                .filter(|_| !self.replaying)
                                .map(|started| started.elapsed()),
                        );
                    } else if entry.kind == EntryKind::Eval {
                        entry.update_eval_footer(
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

fn mark_injected_jobs(result: &mut Value, injected: &BTreeSet<String>) -> bool {
    let mut changed = false;
    for job in result
        .get_mut("jobs")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if job["type"] == "task"
            && job["injected"] != true
            && job["id"].as_str().is_some_and(|id| injected.contains(id))
        {
            job["injected"] = true.into();
            changed = true;
        }
    }
    changed
}
