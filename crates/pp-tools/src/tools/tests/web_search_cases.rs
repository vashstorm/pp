use super::*;
use pp_config::config::{
    SandboxMode, SandboxSettings, SearxngSettings, WebSearchProvider, WebSearchSettings,
};
use pp_llm::web_search::{SearchSource, WebSearchResponse};
use pp_sandbox::{ProbeResult, Sandbox};
use std::sync::Arc;

fn ok_response(provider: &str, label: &str) -> WebSearchResponse {
    WebSearchResponse {
        provider: provider.to_owned(),
        provider_label: label.to_owned(),
        answer: None,
        sources: vec![SearchSource {
            title: "Rust Docs".into(),
            url: "https://doc.rust-lang.org/".into(),
            snippet: Some("Official documentation".into()),
            published_date: None,
            age_seconds: None,
            author: None,
        }],
        notes: Vec::new(),
        failures: Vec::new(),
        formatted_text: "[1] Rust Docs\n    https://doc.rust-lang.org/\n    Official documentation"
            .into(),
    }
}

#[test]
fn web_search_is_exposed_in_execute_plan_and_review_and_respects_enabled_flag() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = Tools::new(dir.path().to_path_buf(), EditMode::Replace).unwrap();

    for defs in [
        tools.all_definitions(WorkMode::Execute),
        tools.all_definitions(WorkMode::Plan),
        tools.review_definitions(),
    ] {
        assert!(defs.iter().any(|d| d.name == "web_search"));
    }

    let document = PlanDocument::new().unwrap();
    let workflow = WorkflowState::default();
    let run_with = |policy: ToolPolicy<'_>| {
        let mut called = false;
        let exec = tools.execute_with_web(
            "web_search",
            &json!({"query": "rust 1.88"}).to_string(),
            "ws-call",
            policy,
            None,
            &CancellationToken::new(),
            &mut |_| {},
            &mut |action, _| match action {
                web_search::WebAction::Search {
                    request,
                    allow_external,
                    ..
                } => {
                    called = true;
                    assert_eq!(request.query, "rust 1.88");
                    assert!(allow_external);
                    Ok(web_search::WebOutcome::Search(ok_response(
                        "public",
                        "Public Web",
                    )))
                }
                web_search::WebAction::Read(_) => unreachable!(),
            },
        );
        assert!(called);
        let val: Value = serde_json::from_str(&exec.output).unwrap();
        assert_eq!(val["ok"], true, "{val}");
        assert_eq!(val["provider"], "public");
        assert_eq!(val["details"]["source_count"], 1);
    };

    run_with(ToolPolicy::Execute {
        workflow: &workflow,
    });
    run_with(ToolPolicy::Plan {
        document: &document,
        sealed: false,
        input: None,
    });
    run_with(ToolPolicy::Review);

    // 关闭 webSearch.enabled 后，三个模式均不再暴露且执行期拒绝。
    tools.configure_web_search(WebSearchSettings {
        enabled: false,
        ..Default::default()
    });
    for defs in [
        tools.all_definitions(WorkMode::Execute),
        tools.all_definitions(WorkMode::Plan),
        tools.review_definitions(),
    ] {
        assert!(!defs.iter().any(|d| d.name == "web_search"));
    }
    let disabled = tools.execute(
        "web_search",
        &json!({"query": "rust"}).to_string(),
        "ws-disabled",
        ToolPolicy::Execute {
            workflow: &workflow,
        },
        &CancellationToken::new(),
        &mut |_| {},
    );
    let val: Value = serde_json::from_str(&disabled.output).unwrap();
    assert_eq!(val["ok"], false);
    assert!(
        val["error"]
            .as_str()
            .is_some_and(|e| e.contains("disabled")),
        "{val}"
    );
}

#[test]
fn web_search_enforces_sandbox_network_policy_and_allows_loopback_searxng() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let mut tools = Tools::new(dir.path().to_path_buf(), EditMode::Replace).unwrap();
    tools.set_sandbox(Arc::new(Sandbox::with_probe_for_test(
        SandboxSettings {
            mode: SandboxMode::ReadOnly,
            network: false,
            ..Default::default()
        },
        dir.path(),
        Some(home.path()),
        Some(Path::new("/tmp")),
        ProbeResult::Available,
    )));
    let workflow = WorkflowState::default();

    // 默认无本地 SearXNG 时，断网沙盒直接拒绝。
    let denied = tools.execute_with_web(
        "web_search",
        &json!({"query": "rust"}).to_string(),
        "sbx-deny",
        ToolPolicy::Execute {
            workflow: &workflow,
        },
        None,
        &CancellationToken::new(),
        &mut |_| {},
        &mut |_, _| unreachable!("runner must not be called when sandbox blocks network"),
    );
    let val: Value = serde_json::from_str(&denied.output).unwrap();
    assert_eq!(val["ok"], false);
    assert!(
        val["error"].as_str().is_some_and(|e| e.contains("sandbox")),
        "{val}"
    );

    // 配置本地回环 SearXNG 后，macOS loopback 允许语义下放行并传入 allow_external_network=false。
    tools.configure_web_search(WebSearchSettings {
        provider: WebSearchProvider::Auto,
        searxng: Some(SearxngSettings {
            endpoint: "http://127.0.0.1:8080".into(),
            token: None,
            basic_username: None,
            basic_password: None,
            engines: None,
            categories: None,
            language: None,
            safesearch: None,
        }),
        ..Default::default()
    });
    let mut observed_external = None;
    let allowed = tools.execute_with_web(
        "web_search",
        &json!({"query": "rust"}).to_string(),
        "sbx-loopback",
        ToolPolicy::Execute {
            workflow: &workflow,
        },
        None,
        &CancellationToken::new(),
        &mut |_| {},
        &mut |action, _| match action {
            web_search::WebAction::Search { allow_external, .. } => {
                observed_external = Some(allow_external);
                Ok(web_search::WebOutcome::Search(ok_response(
                    "searxng", "SearXNG",
                )))
            }
            web_search::WebAction::Read(_) => unreachable!(),
        },
    );
    let val: Value = serde_json::from_str(&allowed.output).unwrap();
    assert_eq!(val["ok"], true, "{val}");
    assert_eq!(observed_external, Some(false));

    // 显式要求外部引擎仍被沙盒拒绝。
    let explicit_ext = tools.execute_with_web(
        "web_search",
        &json!({"query": "rust", "provider": "duckduckgo"}).to_string(),
        "sbx-ext",
        ToolPolicy::Execute {
            workflow: &workflow,
        },
        None,
        &CancellationToken::new(),
        &mut |_| {},
        &mut |_, _| unreachable!(),
    );
    let val: Value = serde_json::from_str(&explicit_ext.output).unwrap();
    assert_eq!(val["ok"], false);

    // 显式开启沙盒但平台不可用时，必须 fail-closed 拒绝外部搜索。
    let unavailable = Sandbox::with_probe_for_test(
        SandboxSettings {
            mode: SandboxMode::ReadOnly,
            network: false,
            ..Default::default()
        },
        dir.path(),
        Some(home.path()),
        Some(Path::new("/tmp")),
        ProbeResult::Unavailable {
            reason: "nested".into(),
        },
    );
    assert!(
        web_search::sandbox_network_decision(
            unavailable.view(None),
            &WebSearchSettings::default(),
            None
        )
        .is_err()
    );
}
