//! `read` branch: local paged reads and `agent://` and `artifact://` resources.

use super::*;

impl Tools {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::tools) fn run_read(
        &self,
        arguments: &str,
        cancellation: &CancellationToken,
        web: &mut web_search::WebHandler<'_>,
        images: &mut Vec<read::ToolImage>,
        permissions: pp_config::config::PermissionMode,
        sandbox: pp_sandbox::SandboxView<'_>,
        read_policy: Option<std::sync::Arc<pp_sandbox::Policy>>,
        hook_ask: Option<&str>,
    ) -> Result<(Value, Option<PlanChange>, Option<todo::TodoChange>)> {
        let path = read::parse_args(arguments, &self.cwd)?;
        let args = ReadArgs { path };
        if args.path.starts_with("agent://") {
            let binding = self
                .subagent
                .as_ref()
                .context("subagent results unavailable")?;
            let result = read::agent_result(
                &args.path,
                &self.cwd,
                self.limits,
                self.edit_mode,
                binding.as_ref(),
                cancellation,
            )?;
            return Ok((result, None, None));
        }
        ensure!(
            !args.path.starts_with(LEGACY_PIPE_URI),
            "{LEGACY_PIPE_ERROR}"
        );
        if args.path.starts_with("artifact://") {
            return read::artifact(
                &args.path,
                &self.cwd,
                self.limits,
                self.edit_mode,
                &self.artifacts,
                cancellation,
            )
            .map(|result| (result.value, None, None));
        }
        if let Some(local) = read::local_path(&args.path, &self.cwd)? {
            if let Some(policy) = read_policy.as_deref() {
                policy.check_read(&local)?;
            } else {
                sandbox.check_read(&local)?;
            }
        } else if read::is_http_url(&args.path, &self.cwd)? {
            let network_enabled = sandbox.policy()?.is_none_or(|policy| policy.network());
            self.permission.authorize_network(
                permissions,
                "read",
                &self.cwd,
                &args.path,
                network_enabled,
                hook_ask,
                cancellation,
            )?;
        }
        self.activate_read_instructions(&args.path)?;
        let dedup_probe = read::dedup_probe(
            &args.path,
            &self.cwd,
            &self.workspace,
            self.edit_mode,
            self.limits,
        );
        if let Some(probe) = dedup_probe.as_ref() {
            let hit = self
                .read_dedup
                .borrow_mut()
                .check(probe, |ws_key| self.snapshots.borrow().is_active(ws_key));
            if let Some(value) = hit {
                return Ok((value, None, None));
            }
        }
        let mut fetch = |url: &str, token: &CancellationToken| match web(
            web_search::WebAction::Read(url),
            token,
        )? {
            web_search::WebOutcome::Read(resp) => Ok(resp),
            web_search::WebOutcome::Search(_) => {
                anyhow::bail!("unexpected web_search response for URL read")
            }
        };
        let result = read::run_with_snapshots(
            &args.path,
            &self.cwd,
            self.limits,
            self.edit_mode,
            &self.skills,
            &mut self.web_cache.borrow_mut(),
            Some((&self.workspace, &mut self.snapshots.borrow_mut())),
            &mut fetch,
            cancellation,
        )?;
        if result.images.is_empty()
            && let Some(probe) = dedup_probe
        {
            self.read_dedup.borrow_mut().record(probe, &result.value);
        }
        images.extend(result.images);
        Ok((result.value, None, None))
    }
}
