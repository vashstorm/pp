use anyhow::{Context, Result, ensure};
use async_openai::types::chat::FunctionObject;
use pp_config::config::{WebSearchProvider, WebSearchSettings};
use pp_core::cancellation::CancellationToken;
use pp_llm::web_search::{Recency, WebSearchRequest, WebSearchResponse};
use pp_sandbox::SandboxView;
use serde::Deserialize;
use serde_json::{Value, json};

pub enum WebAction<'a> {
    Read(&'a str),
    Search {
        request: &'a WebSearchRequest,
        settings: &'a WebSearchSettings,
        allow_external: bool,
    },
}

pub enum WebOutcome {
    Read(pp_llm::http::HttpResponse),
    Search(WebSearchResponse),
}

pub type WebHandler<'a> = dyn FnMut(WebAction<'_>, &CancellationToken) -> Result<WebOutcome> + 'a;

pub type Runner<'a> = dyn FnMut(
        &WebSearchRequest,
        &WebSearchSettings,
        bool,
        &CancellationToken,
    ) -> Result<WebSearchResponse>
    + 'a;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WebSearchArgs {
    query: String,
    #[serde(default)]
    recency: Option<Recency>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    provider: Option<WebSearchProvider>,
}

const WEB_SEARCH_DESC: &str = concat!(
    "Search the web without external MCP servers (use `read` for known URLs). ",
    "Supports `site:`/`-site:`, `after:`/`before: YYYY-MM-DD`, `inurl:`, `intitle:`, `filetype:`, `\"phrase\"`, `-term`, `OR`. ",
    "No filesystem sandbox; outbound network obeys sandbox network policy. Cite source URLs."
);

pub(super) fn definition() -> FunctionObject {
    super::tool_def(
        "web_search",
        WEB_SEARCH_DESC,
        super::object_schema(
            json!({
                "query": {"type": "string"},
                "recency": {"type": "string", "enum": ["day", "week", "month", "year"]},
                "limit": {"type": "integer", "minimum": 1, "maximum": 30},
                "provider": {"type": "string", "enum": ["auto", "public", "duckduckgo", "startpage", "hackernews", "searxng"]}
            }),
            &["query"],
        ),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NetworkDecision {
    AllowExternal,
    LoopbackOnly,
    NeedsApproval,
}

#[cfg(test)]
pub(crate) fn sandbox_network_decision(
    sandbox: SandboxView<'_>,
    settings: &WebSearchSettings,
    requested: Option<WebSearchProvider>,
) -> Result<bool> {
    match sandbox_network_check(sandbox, settings, requested)? {
        NetworkDecision::AllowExternal => Ok(true),
        NetworkDecision::LoopbackOnly => Ok(false),
        NetworkDecision::NeedsApproval => {
            anyhow::bail!("web_search is blocked by sandbox policy (outbound network is disabled).")
        }
    }
}

pub(crate) fn sandbox_network_check(
    sandbox: SandboxView<'_>,
    settings: &WebSearchSettings,
    requested: Option<WebSearchProvider>,
) -> Result<NetworkDecision> {
    let Some(policy) = sandbox.policy()? else {
        return Ok(NetworkDecision::AllowExternal);
    };
    if policy.network() {
        return Ok(NetworkDecision::AllowExternal);
    }
    if policy.backend() == "bubblewrap" {
        return Ok(NetworkDecision::NeedsApproval);
    }

    let effective = requested.unwrap_or(settings.provider);
    let loopback_searxng = settings
        .searxng
        .as_ref()
        .is_some_and(|cfg| cfg.is_loopback_endpoint());

    match effective {
        WebSearchProvider::Searxng | WebSearchProvider::Auto if loopback_searxng => {
            Ok(NetworkDecision::LoopbackOnly)
        }
        _ => Ok(NetworkDecision::NeedsApproval),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_with_approval(
    arguments: &str,
    settings: &WebSearchSettings,
    sandbox: SandboxView<'_>,
    cancellation: &CancellationToken,
    authorize: &mut dyn FnMut(&str) -> Result<()>,
    runner: &mut Runner<'_>,
) -> Result<Value> {
    let args: WebSearchArgs =
        serde_json::from_str(arguments).context("invalid web_search arguments")?;
    let trimmed = args.query.trim();
    ensure!(!trimmed.is_empty(), "web_search query must not be empty");
    if let Some(limit) = args.limit {
        ensure!(
            (1..=30).contains(&limit),
            "web_search limit must be between 1 and 30"
        );
    }

    let allow_external_network = match sandbox_network_check(sandbox, settings, args.provider)? {
        NetworkDecision::AllowExternal => true,
        NetworkDecision::LoopbackOnly => false,
        NetworkDecision::NeedsApproval => {
            authorize(&format!("query={trimmed}"))?;
            true
        }
    };
    let request = WebSearchRequest {
        query: trimmed.to_owned(),
        recency: args.recency,
        limit: args.limit,
        provider: args.provider,
    };
    let response = runner(&request, settings, allow_external_network, cancellation)?;
    let source_count = response.sources.len();

    Ok(json!({
        "ok": true,
        "provider": response.provider,
        "provider_label": response.provider_label,
        "content": response.formatted_text,
        "details": {
            "query": trimmed,
            "source_count": source_count,
            "answer": response.answer,
            "sources": response.sources,
            "notes": response.notes,
            "failures": response.failures,
        }
    }))
}
