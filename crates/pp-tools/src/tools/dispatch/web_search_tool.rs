//! `web_search` 分支：执行无密钥 HTTP 网页检索并落实沙盒网络边界。

use super::*;

impl Tools {
    pub(in crate::tools) fn run_web_search(
        &self,
        arguments: &str,
        cancellation: &CancellationToken,
        web: &mut web_search::WebHandler<'_>,
        sandbox: pp_sandbox::SandboxView<'_>,
    ) -> Result<(Value, Option<PlanChange>, Option<todo::TodoChange>)> {
        let mut runner = |req: &pp_llm::web_search::WebSearchRequest,
                          settings: &pp_config::config::WebSearchSettings,
                          allow_external: bool,
                          token: &CancellationToken| {
            match web(
                web_search::WebAction::Search {
                    request: req,
                    settings,
                    allow_external,
                },
                token,
            )? {
                web_search::WebOutcome::Search(resp) => Ok(resp),
                web_search::WebOutcome::Read(_) => {
                    anyhow::bail!("unexpected URL read response for web_search")
                }
            }
        };
        let value = web_search::run(
            arguments,
            &self.web_search,
            sandbox,
            cancellation,
            &mut runner,
        )?;
        Ok((value, None, None))
    }
}
