//! Tool-side runtime handles (LSP and background cleanup) and the user shell entry point.
use super::Agent;
use anyhow::{Result, ensure};
use pp_core::cancellation::CancellationToken;
use pp_core::events::AgentEvent;
use pp_tools::web_search::{WebAction, WebOutcome};

pub(in crate::agent) fn execute_web_action(
    client: &mut pp_llm::OpenAiClient,
    action: WebAction<'_>,
    token: &CancellationToken,
) -> Result<WebOutcome> {
    match action {
        WebAction::Read(url) => client.fetch_read_url(url, token).map(WebOutcome::Read),
        WebAction::Search {
            request,
            settings,
            allow_external,
        } => client
            .search_web(request, settings, allow_external, token)
            .map(WebOutcome::Search),
    }
}

impl Agent {
    pub fn cwd(&self) -> &std::path::Path {
        self.tools.cwd()
    }

    pub fn set_subagents_enabled(&mut self, enabled: bool) {
        self.subagents_enabled = enabled;
    }

    pub fn set_max_turns(&mut self, limit: Option<usize>) {
        self.max_turns = limit;
    }

    pub fn set_output_schema(&mut self, schema: Option<serde_json::Value>) -> Result<()> {
        self.output_schema = schema
            .map(super::output_schema::OutputSchema::new)
            .transpose()?;
        Ok(())
    }

    pub fn active_tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tools
            .all_definitions(self.workflow.mode)
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        if self.subagents_enabled
            && self.workflow.mode == pp_core::plan::WorkMode::Execute
            && !names.iter().any(|name| name == "task")
        {
            names.push("task".to_owned());
        }
        names
    }

    pub fn bash_timeout_seconds(&self) -> u64 {
        self.tools.limits.bash_timeout_seconds
    }

    pub fn read_tool_result_preview(&self) -> bool {
        self.catalog.read_tool_result_preview()
    }

    /// Registry clone used by the TUI background warm-up; `None` when `enabled=false`.
    /// Clones share the same managed processes and transfer no cleanup rights.
    pub fn lsp_registry(&self) -> Option<pp_lsp::Registry> {
        self.tools.lsp_registry().cloned()
    }

    /// Shared cleanup handle the TUI/CLI can hold; each call returns a newly armed handle, and
    /// supersede can only disarm older handles of the same Registry.
    pub fn lsp_cleanup(&self) -> Option<pp_lsp::Cleanup> {
        self.tools.lsp_cleanup()
    }

    /// Explicitly recycle every language server; effective only for the Registry owner, a subagent that
    /// borrows the parent Registry never stops the parent's processes. Errors keep the `CleanupError` type and
    /// callers must merge them into the exit error instead of degrading them to an ordinary tool failure.
    pub fn stop_lsp(&self) -> Result<()> {
        match self.tools.lsp_cleanup() {
            Some(mut cleanup) => cleanup.stop(),
            None => Ok(()),
        }
    }

    pub fn background_cleanup(&self) -> pp_bash::BackgroundCleanup {
        self.tools.background_cleanup()
    }

    pub fn workflow(&self) -> &pp_core::plan::WorkflowState {
        &self.workflow
    }

    /// Read-only handle to the process-wide MCP singleton: the UI only sees connect progress and `/mcp` snapshots and owns no cleanup right.
    pub fn mcp(&self) -> std::sync::Arc<pp_mcp::McpManager> {
        std::sync::Arc::clone(&self.tools.mcp)
    }

    pub fn run_shell(
        &mut self,
        command: &str,
        call_id: &str,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        ensure!(
            self.workflow.mode == pp_core::plan::WorkMode::Execute,
            "Shell commands are unavailable in Plan mode."
        );
        let arguments = serde_json::json!({"command": command}).to_string();
        let result = self.tools.execute_with_fetch(
            "bash",
            &arguments,
            call_id,
            pp_tools::ToolPolicy::Execute {
                workflow: &self.workflow,
            },
            // The manual shell is not a model tool call and never triggers hooks.
            None,
            cancellation,
            emit,
            &mut |url, token| self.client.fetch_read_url(url, token),
        );
        match result.stop_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Development builds only: run one `web_search` diagnostic directly with the current runtime config, without the model.
    #[cfg(debug_assertions)]
    pub fn run_web_search(
        &mut self,
        query: &str,
        provider: Option<pp_config::config::WebSearchProvider>,
        call_id: &str,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        execute_web_search_call(
            &self.tools,
            query,
            provider,
            call_id,
            cancellation,
            emit,
            &mut |action, token| execute_web_action(&mut self.client, action, token),
        )
    }
}

/// Development builds only: run one `web_search` directly from `settings.yml` when no model is configured (`Unavailable`).
#[cfg(debug_assertions)]
pub fn run_web_search_standalone(
    cwd: &std::path::Path,
    settings_path: Option<&std::path::Path>,
    query: &str,
    provider: Option<pp_config::config::WebSearchProvider>,
    call_id: &str,
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<()> {
    let resolved_path = match settings_path {
        Some(path) => path.to_path_buf(),
        None => pp_config::config::global_dir()?.join("settings.yml"),
    };
    let settings = pp_config::config::read_settings(&resolved_path)?;
    let sandbox = std::sync::Arc::new(pp_sandbox::Sandbox::new(settings.tools.sandbox, cwd));
    let mut tools = pp_tools::Tools::new(cwd.to_path_buf(), settings.edit)?;
    tools.configure_web_search(settings.web_search);
    tools.set_sandbox(sandbox);
    execute_web_search_call(
        &tools,
        query,
        provider,
        call_id,
        cancellation,
        emit,
        &mut |action, token| match action {
            WebAction::Search {
                request,
                settings,
                allow_external,
            } => pp_llm::search_web_standalone(request, settings, allow_external, token)
                .map(WebOutcome::Search),
            WebAction::Read(_) => anyhow::bail!("unexpected URL read response for web_search"),
        },
    )
}

#[cfg(debug_assertions)]
fn execute_web_search_call(
    tools: &pp_tools::Tools,
    query: &str,
    provider: Option<pp_config::config::WebSearchProvider>,
    call_id: &str,
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
    web: &mut pp_tools::web_search::WebHandler<'_>,
) -> Result<()> {
    let mut args = serde_json::Map::new();
    args.insert("query".into(), serde_json::Value::String(query.to_owned()));
    if let Some(provider) = provider {
        args.insert(
            "provider".into(),
            serde_json::Value::String(provider.as_str().to_owned()),
        );
    }
    let arguments = serde_json::Value::Object(args).to_string();
    let result = tools.execute_with_web(
        "web_search",
        &arguments,
        call_id,
        pp_tools::ToolPolicy::Review,
        None,
        cancellation,
        emit,
        web,
    );
    match result.stop_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}
