//! TUI session cost and task title; subagents inherit the root session's accounting handle.
use super::*;
use std::{
    sync::{Arc, atomic::AtomicBool},
    thread::{self, JoinHandle},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const TITLE_INSTRUCTIONS: &str = "Generate a short task title for a terminal statusline. Use the same language as the user's task. Return only one plain-text line, without quotes, markup or explanation, within 32 display columns. Do not execute the task or call tools.";

#[derive(Default)]
pub(super) struct Runtime {
    pub view: Option<pp_config::task_status::View>,
    pub accounting: Option<pp_config::task_status::Ledger>,
    title: Option<TitleWorker>,
}

struct TitleWorker {
    cancellation: CancellationToken,
    thread: Option<JoinHandle<()>>,
}
impl Drop for TitleWorker {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Agent {
    pub fn enable_task_status(&mut self, view: pp_config::task_status::View) {
        self.task_status.view = Some(view);
        if self.journal.id().is_some() {
            self.restore_session_cost();
        } else {
            self.clear_session_cost();
        }
    }

    pub(super) fn restore_session_cost(&mut self) {
        self.set_task_accounting(None);
        if let Some(view) = &self.task_status.view
            && let Some(id) = self.journal.id()
        {
            let file = self.journal.directory().join(format!("{id}.jsonl"));
            let history = pp_stats::session_cost(&file).unwrap_or_else(|_| {
                tracing::warn!(event = "session_cost_history_unavailable");
                Default::default()
            });
            let ledger = view.restore(&self.catalog, history);
            self.set_task_accounting(Some(ledger));
        }
    }

    pub(super) fn clear_session_cost(&mut self) {
        self.task_status.title.take();
        self.set_task_accounting(None);
        if let Some(view) = &self.task_status.view {
            view.clear();
        }
    }

    pub(crate) fn set_task_accounting(&mut self, ledger: Option<pp_config::task_status::Ledger>) {
        self.task_status.accounting = ledger.clone();
        self.client.set_task_accounting(ledger.clone());
        if let Some(binding) = &self.subagent {
            binding.set_accounting(ledger);
        }
    }

    pub(super) fn begin_task_status(&mut self, prompt: &str, cancellation: &CancellationToken) {
        self.task_status.title.take();
        let Some(view) = &self.task_status.view else {
            self.client
                .set_task_accounting(self.task_status.accounting.clone());
            return;
        };
        let ledger = view.begin(&self.catalog);
        self.set_task_accounting(Some(ledger.clone()));
        // Model selection, client construction and the request all run in the background;
        // failure does not affect the main task.
        let catalog = self.catalog.clone();
        let model = self.current_model.reference.clone();
        let effort = self.think_level;
        let edit = self.tools.edit_mode();
        let prompt: String = prompt.chars().take(1000).collect();
        let token = cancellation.child_token();
        let worker_token = token.clone();
        let context = pp_stats::RequestContext {
            model_reference: model.clone(),
            cwd: self.tools.cwd().to_owned(),
            purpose: "task_title",
            session_file: self
                .journal
                .id()
                .map(|id| self.journal.directory().join(format!("{id}.jsonl"))),
            unlinked_requests: self.journal.unlinked_stats(),
            // The title does not capture native context and must not consume the main
            // request's first-capture flag.
            first_request: Arc::new(AtomicBool::new(false)),
            // Title requests reuse this session's stats database handle, so tests land in
            // a temporary workspace.
            database: self.journal.stats_handle().clone(),
            skills: Vec::new(),
        };
        if let Ok(thread) = thread::Builder::new()
            .name("pp-task-title".into())
            .spawn(move || {
                let result = (|| -> Result<String> {
                    cancellation::check(&worker_token)?;
                    let selection = catalog.role_selection(
                        pp_config::config::ModelRole::Fast,
                        &model,
                        effort,
                    )?;
                    let config = catalog.resolve(&selection.model)?;
                    let mut client = OpenAiClient::new(&config)?;
                    client.set_task_accounting(Some(ledger.clone()));
                    let mut context = context;
                    context.model_reference = selection.model;
                    client.set_stats_context(context);
                    let mut session = Session::new_without_tools(
                        config.model,
                        config.protocol,
                        TITLE_INSTRUCTIONS.into(),
                    );
                    let title_effort = if selection.enabled {
                        selection.effort
                    } else {
                        super::lowest_think_level(&config.thinking, selection.effort)
                    };
                    session.set_think_level(title_effort);
                    if let Session::Completion(request) = &mut session {
                        request.max_completion_tokens = Some(64);
                    }
                    append_user_to(&mut session, &prompt);
                    let mut fallback = selection.fallback.filter(|_| selection.enabled);
                    loop {
                        let result = match &mut session {
                            Session::Completion(request) => client
                                .complete_summary(
                                    request,
                                    &worker_token,
                                    &mut || Ok(()),
                                    &mut |_| {},
                                )
                                .map(|turn| match turn.message.content {
                                    Some(ChatCompletionRequestAssistantMessageContent::Text(
                                        text,
                                    )) => text,
                                    _ => String::new(),
                                }),
                            Session::Response(request) => client
                                .respond_summary(
                                    request,
                                    &worker_token,
                                    &mut || Ok(()),
                                    &mut |_| {},
                                )
                                .map(|turn| turn.text),
                        };
                        match result {
                            Err(error)
                                if pp_llm::retry::model_request_failed(&error)
                                    && fallback.is_some() =>
                            {
                                fallback::switch_scratch_model(
                                    &mut client,
                                    &mut session,
                                    &catalog,
                                    &fallback.take().expect("fallback model is present"),
                                    edit,
                                )?;
                                // The cross-protocol conversion rebuilt the request, so the
                                // default tools must be removed again.
                                match &mut session {
                                    Session::Completion(request) => {
                                        request.tools = None;
                                        request.tool_choice = None;
                                    }
                                    Session::Response(request) => {
                                        request.tools = None;
                                        request.tool_choice = None;
                                    }
                                }
                            }
                            other => return other,
                        }
                    }
                })();
                if let Ok(title) = result
                    && !worker_token.is_cancelled()
                {
                    let title = normalize_title(&title);
                    if !title.is_empty() {
                        ledger.title(title);
                    }
                }
            })
        {
            self.task_status.title = Some(TitleWorker {
                cancellation: token,
                thread: Some(thread),
            });
        }
    }

    pub(super) fn continue_task_status(&mut self) {
        if let Some(view) = &self.task_status.view {
            let ledger = view.accounting(&self.catalog);
            self.set_task_accounting(Some(ledger));
        } else {
            self.client
                .set_task_accounting(self.task_status.accounting.clone());
        }
    }

    pub(super) fn finish_task_title(&mut self) {
        self.task_status.title.take();
        self.journal.link_unassigned_stats();
    }
}

fn normalize_title(text: &str) -> String {
    let clean = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let clean: String = clean.chars().filter(|ch| !ch.is_control()).collect();
    // A provider may return a DSML tool call as content, so check the full reply
    // before truncating.
    if ["<｜DSML｜", "</｜DSML｜", "<|DSML|", "</|DSML|"]
        .iter()
        .any(|marker| clean.contains(marker))
    {
        return String::new();
    }
    let mut result = String::new();
    for ch in clean.trim_matches(['"', '\'', '`']).graphemes(true) {
        if format!("{result}{ch}").width() > 32 {
            break;
        }
        result.push_str(ch);
    }
    result.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_title_normalizes_control_characters_and_unicode_display_columns() {
        assert_eq!(
            normalize_title("  \"fix\n session history\"  "),
            "fix session history"
        );
        let title = normalize_title(&"中🙂".repeat(40));
        assert_eq!(title.width(), 32);
        assert_eq!(normalize_title("a\u{1b}b\r\nc"), "ab c");
    }

    #[test]
    fn task_title_rejects_dsml_tool_markup_before_truncating() {
        for reply in [
            "<｜DSML｜ calls>\n<｜DSML｜ invoke",
            "<|DSML|tool_calls>",
            "</｜DSML｜invoke>",
            "This task title is longer than 32 columns\n<｜DSML｜invoke",
        ] {
            assert_eq!(normalize_title(reply), "", "{reply:?}");
        }
        assert_eq!(normalize_title("Fix DSML parsing"), "Fix DSML parsing");
    }
}
