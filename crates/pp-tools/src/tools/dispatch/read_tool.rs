//! `read` 分支：本地分页读取与 `agent://`、`pipe://`、`artifact://` 资源。

use super::*;

impl Tools {
    pub(in crate::tools) fn run_read(
        &self,
        arguments: &str,
        cancellation: &CancellationToken,
        web: &mut web_search::WebHandler<'_>,
        images: &mut Vec<read::ToolImage>,
    ) -> Result<(Value, Option<PlanChange>, Option<todo::TodoChange>)> {
        let args: ReadArgs = serde_json::from_str(arguments).context("invalid read arguments")?;
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
        if read::is_pipe_uri(&args.path) {
            return read::read_pipe(
                &args.path,
                &self.cwd,
                self.limits,
                self.edit_mode,
                &self.pipe_store,
                cancellation,
            )
            .map(|result| (result.value, None, None));
        }
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
            self.sandbox.check_read(&local)?;
        }
        self.activate_read_instructions(&args.path)?;
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
        images.extend(result.images);
        Ok((result.value, None, None))
    }
}
