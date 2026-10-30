use super::*;
use std::thread;
use tiny_http::{Header, Response, Server};

struct Captured {
    path: String,
    body: String,
    headers: Vec<(String, String)>,
}

fn mock(replies: Vec<(u16, Value)>) -> (String, thread::JoinHandle<Vec<Captured>>) {
    let server = Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr());
    let handle = thread::spawn(move || {
        replies
            .into_iter()
            .map(|(status, body)| {
                let mut request = server
                    .recv_timeout(Duration::from_secs(15))
                    .unwrap()
                    .expect("认证请求必须到达");
                let mut input = String::new();
                request.as_reader().read_to_string(&mut input).unwrap();
                let captured = Captured {
                    path: request.url().to_owned(),
                    body: input,
                    headers: request
                        .headers()
                        .iter()
                        .map(|h| {
                            (
                                h.field.as_str().to_string().to_lowercase(),
                                h.value.to_string(),
                            )
                        })
                        .collect(),
                };
                request
                    .respond(
                        Response::from_string(body.to_string())
                            .with_status_code(status)
                            .with_header(
                                Header::from_bytes("Content-Type", "application/json").unwrap(),
                            ),
                    )
                    .unwrap();
                captured
            })
            .collect()
    });
    (url, handle)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn auth_discovery_filters_unsupported_models_and_preserves_unknown_metadata() {
    let models = discovered_models(Provider::GithubCopilot, &json!({"data": [
        {"id":"usable", "capabilities":{"type":"chat", "supports":{"tool_calls":true}, "limits":{"max_context_window_tokens":12345}}, "supported_endpoints":["/chat/completions"]},
        {"id":"no-tools", "supports_tool_calls":false},
        {"id":"embedding", "capabilities":{"type":"embeddings"}},
        {"id":"response-only", "supported_endpoints":["/responses"]},
        {"id":"both", "supported_endpoints":["/chat/completions", "/responses"]},
        {"id":"messages-only", "supported_endpoints":["/v1/messages"]},
        {"id":"no-endpoints", "supported_endpoints":[]},
        {"id":"hidden", "model_picker_enabled":false},
        {"id":"disabled", "policy":{"state":"disabled"}},
        {"id":"unknown-context"}
    ]})).unwrap();
    assert_eq!(models.len(), 4);
    assert_eq!(
        models["response-only"].protocol,
        Some(crate::models::Protocol::Response)
    );
    assert_eq!(
        models["both"].protocol,
        Some(crate::models::Protocol::Response)
    );
    assert_eq!(
        models["usable"].protocol,
        Some(crate::models::Protocol::Completion)
    );
    assert_eq!(
        models["unknown-context"].protocol,
        Some(crate::models::Protocol::Completion)
    );
    assert_eq!(models["usable"].context_window, Some(12345));
    assert_eq!(models["unknown-context"].context_window, None);
    let codex = discovered_models(
        Provider::OpenaiCodex,
        &json!({"models":[
            {"slug":"visible", "visibility":"list", "context_window":240000},
            {"slug":"hidden", "visibility":"hide"}
        ]}),
    )
    .unwrap();
    assert_eq!(codex.len(), 1);
    assert_eq!(codex["visible"].context_window, Some(240000));
    assert!(discovered_models(Provider::Glm, &json!({"data":[]})).is_err());
    assert!(
        discovered_models(
            Provider::Glm,
            &json!({"data":[
                {"id":"response-only", "supported_endpoints":["/responses"]}
            ]})
        )
        .is_err()
    );
    assert!(discovered_models(Provider::Zai, &json!({"private":"secret-marker"})).is_err());
    assert!(
        !discovered_models(
            Provider::KimiCode,
            &json!({"data":[{"id":"secret-marker\n"}]})
        )
        .err()
        .unwrap()
        .to_string()
        .contains("secret-marker")
    );
}

#[test]
fn auth_commandcode_discovery_selects_supported_protocols_and_filters_other_endpoints() {
    use crate::models::Protocol;

    let models = discovered_models(Provider::CommandCode, &json!({"object":"list", "data":[
        {"id":"deepseek/chat", "context_length":1000000, "supported_endpoints":["/chat/completions", "/responses"]},
        {"id":"chat-only", "supported_endpoints":["/chat/completions"]},
        {"id":"response-only", "supported_endpoints":["/responses"]},
        {"id":"claude", "supported_endpoints":["/messages"]},
        {"id":"decision", "supported_endpoints":["/systemone"]},
        {"id":"no-tools", "supports_tool_calls":false, "supported_endpoints":["/responses"]},
        {"id":"empty", "supported_endpoints":[]}
    ]})).unwrap();
    assert_eq!(models.len(), 3);
    assert_eq!(models["deepseek/chat"].protocol, Some(Protocol::Response));
    assert_eq!(models["deepseek/chat"].context_window, Some(1000000));
    assert_eq!(models["chat-only"].protocol, Some(Protocol::Completion));
    assert_eq!(models["chat-only"].context_window, None);
    assert_eq!(models["response-only"].protocol, Some(Protocol::Response));
    assert!(
        discovered_models(
            Provider::CommandCode,
            &json!({"data":[
                {"id":"claude", "supported_endpoints":["/messages"]}
            ]})
        )
        .is_err()
    );
}

#[test]
fn auth_discovery_roundtrip_preserves_model_protocols_and_manual_priority() {
    use crate::auth::{Store, models, service};
    use crate::cancellation::CancellationToken;
    use crate::models::Protocol;

    for provider in [Provider::GithubCopilot, Provider::CommandCode] {
        let id = provider.id();
        let dir = tempfile::tempdir().unwrap();
        service::commit_login(
            dir.path(),
            provider,
            Credential::api_key("private-fixture".into()),
            &CancellationToken::new(),
        )
        .unwrap();
        let store = Store::load(&dir.path().join("agent.db")).unwrap();
        let credential = &store.credentials[&provider];
        let (base, server) = mock(vec![(
            200,
            json!({"data":[
                {"id":"a-response", "supported_endpoints":["/responses"], "context_window":128000},
                {"id":"vendor/b-chat", "supported_endpoints":["/chat/completions"]},
                {"id":"c-response", "supported_endpoints":["/responses"]},
                {"id":"d-legacy"}
            ]}),
        )]);
        let discovered = runtime().block_on(async {
            discover_at(
                &client().unwrap(),
                provider,
                credential,
                &format!("{base}/models"),
            )
            .await
            .unwrap()
        });
        let cache_path = dir.path().join("models-sub.yml");
        let cache = models::Catalog {
            providers: std::collections::BTreeMap::from([(
                provider.id().into(),
                models::Entry {
                    auth: provider,
                    generation: credential.generation.clone(),
                    protocol: Protocol::Completion,
                    models: discovered,
                },
            )]),
        };
        cache.save(&cache_path).unwrap();
        let cache = models::Catalog::load(&cache_path).unwrap();
        assert_eq!(
            cache.providers[id].models["a-response"].protocol,
            Some(Protocol::Response)
        );
        let settings_path = dir.path().join("settings.yml");
        let models_path = dir.path().join("models.yml");
        let mut catalog =
            crate::config::ModelCatalog::from_files(&settings_path, &models_path).unwrap();
        assert_eq!(catalog.default_reference(), &format!("{id}/a-response"));
        assert_eq!(catalog.choices().len(), 4);
        for (id, protocol) in [
            ("a-response", Protocol::Response),
            ("vendor/b-chat", Protocol::Completion),
            ("c-response", Protocol::Response),
            ("d-legacy", Protocol::Completion),
        ] {
            let reference = format!("{}/{id}", provider.id());
            let config = catalog.resolve(&reference).unwrap();
            assert_eq!(config.protocol, protocol);
            assert_eq!(config.model, id);
            assert_eq!(config.auth.unwrap().generation, credential.generation);
            assert_eq!(
                catalog
                    .choices()
                    .iter()
                    .find(|choice| choice.reference == reference)
                    .unwrap()
                    .protocol,
                protocol
            );
        }
        assert_eq!(
            catalog
                .resolve(&format!("{id}/a-response"))
                .unwrap()
                .context_window,
            Some(128000)
        );
        std::fs::write(&models_path, format!("providers:\n  {id}:\n    api_key: manual-fixture\n    protocol: completion\n    base_url: https://example.com/v1\n    models:\n      a-response:\n        model: manual-model\n")).unwrap();
        catalog.reload_models().unwrap();
        let manual = catalog.resolve(&format!("{id}/a-response")).unwrap();
        assert_eq!(manual.protocol, Protocol::Completion);
        assert_eq!(manual.model, "manual-model");
        assert!(manual.auth.is_none());
        assert_eq!(catalog.choices().len(), 4);
        assert_eq!(
            catalog
                .resolve(&format!("{id}/c-response"))
                .unwrap()
                .protocol,
            Protocol::Response
        );
        assert_eq!(server.join().unwrap()[0].path, "/models");
        let update = service::logout(dir.path(), provider, &CancellationToken::new()).unwrap();
        assert!(update.logged.is_empty());
        assert!(
            Store::load(&dir.path().join("agent.db"))
                .unwrap()
                .credentials
                .is_empty()
        );
        assert!(
            models::Catalog::load(&cache_path)
                .unwrap()
                .providers
                .is_empty()
        );
        catalog.reload_models().unwrap();
        assert_eq!(catalog.choices().len(), 1);
        assert!(
            catalog
                .resolve(&format!("{id}/a-response"))
                .unwrap()
                .auth
                .is_none()
        );
    }
}

#[test]
fn auth_discovery_http_all_providers_use_auth_headers_and_redact_errors() {
    for provider in Provider::ALL {
        let data = if provider == Provider::OpenaiCodex {
            json!({"models":[{"slug":"remote-model"}]})
        } else {
            json!({"data":[{"id":"remote-model", "context_length":32768}]})
        };
        let (base, server) = mock(vec![
            (200, data),
            (404, json!({"secret":"private-body"})),
            (401, json!({"error":"private-body"})),
        ]);
        let mut credential = Credential::api_key("private-token".into());
        credential.account_id = Some("account-fixture".into());
        credential.device_id = Some("device-fixture".into());
        runtime().block_on(async {
            let client = client().unwrap();
            let models = discover_at(&client, provider, &credential, &format!("{base}/models"))
                .await
                .unwrap();
            assert!(models.contains_key("remote-model"));
            for _ in 0..2 {
                let error = discover_at(&client, provider, &credential, &format!("{base}/models"))
                    .await
                    .err()
                    .unwrap();
                assert!(!format!("{error:#}").contains("private"));
            }
        });
        let requests = server.join().unwrap();
        assert!(requests.iter().all(|request| {
            request
                .headers
                .iter()
                .any(|(name, value)| name == "authorization" && value == "Bearer private-token")
        }));
        if provider == Provider::OpenaiCodex {
            assert!(requests[0].headers.iter().any(|(name, value)| name == "chatgpt-account-id" && value == "account-fixture"));
        }
    }
}

#[test]
fn auth_discovery_is_cancellable_while_response_is_pending() {
    let server = Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}/models", server.server_addr());
    let token = crate::cancellation::CancellationToken::new();
    let cancel = token.clone();
    let worker = thread::spawn(move || {
        runtime().block_on(async {
            let client = client().unwrap();
            let credential = Credential::api_key("private-token".into());
            assert!(
                token
                    .run_until_cancelled(discover_at(
                        &client,
                        Provider::KimiCode,
                        &credential,
                        &url
                    ))
                    .await
                    .is_none()
            );
        });
    });
    let request = server
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    cancel.cancel();
    worker.join().unwrap();
    drop(request);
}

fn jwt(account: &str) -> String {
    format!(
        "e30.{}.signature",
        URL_SAFE_NO_PAD.encode(
            json!({"https://api.openai.com/auth":{"chatgpt_account_id":account}}).to_string()
        )
    )
}

#[test]
fn auth_kimi_device_login_polls_pending_and_refresh_rotates_tokens() {
    let (base, server) = mock(vec![
        (
            200,
            json!({"device_code":"device-private","user_code":"USER-CODE","verification_uri":"https://www.kimi.com/activate","interval":1,"expires_in":60}),
        ),
        (400, json!({"error":"authorization_pending"})),
        (
            200,
            json!({"access_token":"access-one","refresh_token":"refresh-one","expires_in":3600}),
        ),
        (
            200,
            json!({"access_token":"access-two","refresh_token":"refresh-two","expires_in":7200}),
        ),
    ]);
    let runtime = runtime();
    let credential = runtime.block_on(async {
        let client = client().unwrap();
        let first = device_login(
            &client,
            Provider::KimiCode,
            &format!("{base}/device"),
            &format!("{base}/token"),
            KIMI_CLIENT,
            "",
            |url, code| {
                assert_eq!(url, "https://www.kimi.com/activate");
                assert_eq!(code, "USER-CODE");
            },
        )
        .await
        .unwrap();
        assert!(first.device_id.is_some());
        refresh_at(
            &client,
            Provider::KimiCode,
            &first,
            &format!("{base}/token"),
            KIMI_CLIENT,
        )
        .await
        .unwrap()
    });
    assert_eq!(credential.access_token, "access-two");
    assert_eq!(credential.refresh_token.as_deref(), Some("refresh-two"));
    let requests = server.join().unwrap();
    assert!(requests[1].body.contains("device_code=device-private"));
    assert!(requests[3].body.contains("refresh_token=refresh-one"));
    let ids: Vec<_> = requests
        .iter()
        .map(|r| {
            r.headers
                .iter()
                .find(|(k, _)| k == "x-msh-device-id")
                .unwrap()
                .1
                .clone()
        })
        .collect();
    assert!(ids.iter().all(|id| id == &ids[0]));
}

#[test]
fn auth_copilot_login_discovers_subscription_endpoint() {
    let (base, server) = mock(vec![
        (
            200,
            json!({"device_code":"device","user_code":"USER","verification_uri":"https://github.com/login/device","interval":1,"expires_in":60}),
        ),
        (200, json!({"access_token":"github-token"})),
        (
            200,
            json!({"endpoints":{"api":"https://api.business.githubcopilot.com"}}),
        ),
    ]);
    let credential = runtime().block_on(async {
        device_login(
            &client().unwrap(),
            Provider::GithubCopilot,
            &format!("{base}/device"),
            &format!("{base}/token"),
            GITHUB_CLIENT,
            &format!("{base}/user"),
            |_, _| {},
        )
        .await
        .unwrap()
    });
    assert_eq!(
        credential.api_endpoint.as_deref(),
        Some("https://api.business.githubcopilot.com")
    );
    let requests = server.join().unwrap();
    assert!(requests[0].body.contains("scope=read%3Auser"));
    assert!(
        requests[2]
            .headers
            .iter()
            .any(|(k, v)| k == "authorization" && v == "token github-token")
    );
    assert_eq!(requests[2].path, "/user");
}

#[test]
fn auth_codex_device_login_exchanges_code_and_keeps_account_identity() {
    let token = jwt("account-fixture");
    let (base, server) = mock(vec![
        (
            200,
            json!({"device_auth_id":"device-id","user_code":"USER","interval":"1"}),
        ),
        (403, json!({"error":"pending"})),
        (
            200,
            json!({"authorization_code":"authorization-code","code_verifier":"private-verifier"}),
        ),
        (
            200,
            json!({"access_token":token,"refresh_token":"refresh-token","expires_in":3600}),
        ),
    ]);
    let credential = runtime().block_on(async {
        codex_login_at(
            &client().unwrap(),
            &format!("{base}/device"),
            &format!("{base}/poll"),
            &format!("{base}/exchange"),
            |url, code| {
                assert_eq!(url, "https://auth.openai.com/codex/device");
                assert_eq!(code, "USER");
            },
        )
        .await
        .unwrap()
    });
    assert_eq!(credential.account_id.as_deref(), Some("account-fixture"));
    let requests = server.join().unwrap();
    let body = &requests[3].body;
    assert!(body.contains("code_verifier=private-verifier"));
    assert!(body.contains("redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback"));
    assert!(body.contains("grant_type=authorization_code"));
}

#[test]
fn auth_refresh_errors_redact_bodies_and_retain_existing_tokens() {
    let (base, server) = mock(vec![(
        401,
        json!({"error_description":"private-credential-must-not-leak"}),
    )]);
    let previous = oauth_credential(
        &json!({"access_token":"old-access","refresh_token":"old-refresh","expires_in":3600}),
        Provider::KimiCode,
        None,
    )
    .unwrap();
    let error = runtime().block_on(async {
        refresh_at(
            &client().unwrap(),
            Provider::KimiCode,
            &previous,
            &base,
            KIMI_CLIENT,
        )
        .await
        .err()
        .unwrap()
    });
    assert!(error.to_string().contains("401"));
    assert!(!error.to_string().contains("private-credential"));
    assert_eq!(previous.refresh_token.as_deref(), Some("old-refresh"));
    server.join().unwrap();
    let refreshed = oauth_credential(
        &json!({"access_token":"new-access","expires_in":600}),
        Provider::KimiCode,
        Some(&previous),
    )
    .unwrap();
    assert_eq!(refreshed.refresh_token.as_deref(), Some("old-refresh"));
}

#[test]
fn auth_refresh_is_cancellable_during_network_wait() {
    let server = Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr());
    let cancellation = crate::cancellation::CancellationToken::new();
    let cancel = cancellation.clone();
    let (release, wait) = std::sync::mpsc::channel();
    let handle = thread::spawn(move || {
        let request = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        cancel.cancel();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(request);
    });
    let previous = oauth_credential(
        &json!({"access_token":"old","refresh_token":"refresh","expires_in":600}),
        Provider::KimiCode,
        None,
    )
    .unwrap();
    runtime().block_on(async {
        let client = client().unwrap();
        assert!(
            cancellation
                .run_until_cancelled(refresh_at(
                    &client,
                    Provider::KimiCode,
                    &previous,
                    &url,
                    KIMI_CLIENT
                ))
                .await
                .is_none()
        );
    });
    release.send(()).unwrap();
    handle.join().unwrap();
}

#[test]
fn auth_wire_adaptation_preserves_native_history_and_tool_pairs() {
    let native = json!({"model":"gpt-fixture","store":true,"stream":false,"max_output_tokens":10,"previous_response_id":"old","instructions":"rules","input":[
        {"id":"msg-1","role":"system","content":"policy"},
        {"id":"call-1","type":"function_call","call_id":"call-1","name":"read","arguments":"{}"},
        {"type":"function_call_output","call_id":"call-1","output":"ok"},
        {"id":"rs-1","type":"reasoning","encrypted_content":"encrypted"}
    ]});
    let wire =
        serde_json::to_value(wire_request(&native, Some(Provider::OpenaiCodex)).unwrap()).unwrap();
    assert_eq!(wire["store"], false);
    assert_eq!(wire["stream"], true);
    assert!(wire.get("max_output_tokens").is_none());
    assert!(wire.get("previous_response_id").is_none());
    assert_eq!(wire["input"][0]["role"], "developer");
    assert!(wire["input"][1].get("id").is_none());
    assert_eq!(wire["input"][1]["call_id"], wire["input"][2]["call_id"]);
    assert_eq!(wire["input"][3]["encrypted_content"], "encrypted");
    assert_eq!(native["input"][0]["role"], "system");
    let ordinary = serde_json::to_value(wire_request(&native, None).unwrap()).unwrap();
    assert_eq!(ordinary, native);
    let kimi = json!({"model":"kimi-for-coding","reasoning_effort":"none","messages":[{"role":"tool","content":"ok"}],"tools":[{"type":"function","function":{"name":"read","strict":true}}]});
    let adapted = wire_request(&kimi, Some(Provider::KimiCode)).unwrap();
    assert_eq!(request_metadata(&adapted), ("agent", false));
    let adapted = serde_json::to_value(adapted).unwrap();
    assert_eq!(adapted["thinking"]["type"], "disabled");
    assert!(adapted.get("reasoning_effort").is_none());
    assert!(adapted["tools"][0]["function"].get("strict").is_none());
}
