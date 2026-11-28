use super::*;
use axum::{body::Body, response::Response};
use pp_test_support::TestServer;
use std::sync::Arc;
use std::thread;

struct Captured {
    path: String,
    body: String,
    headers: Vec<(String, String)>,
}

fn mock(
    replies: Vec<(u16, Value)>,
) -> (Arc<TestServer>, String, thread::JoinHandle<Vec<Captured>>) {
    let server = Arc::new(TestServer::start().unwrap());
    let url = format!("http://{}", server.addr());
    let responder = Arc::clone(&server);
    let handle = thread::spawn(move || {
        replies
            .into_iter()
            .map(|(status, body)| {
                let request = responder
                    .recv_timeout(Duration::from_secs(15))
                    .unwrap()
                    .expect("认证请求必须到达");
                let captured = Captured {
                    path: request.target().to_owned(),
                    body: String::from_utf8_lossy(request.body()).into_owned(),
                    headers: request
                        .headers()
                        .iter()
                        .map(|(name, value)| {
                            (name.as_str().to_owned(), value.to_str().unwrap().to_owned())
                        })
                        .collect(),
                };
                request
                    .respond(
                        Response::builder()
                            .status(status)
                            .header("Content-Type", "application/json")
                            .body(Body::from(body.to_string()))
                            .unwrap(),
                    )
                    .unwrap();
                captured
            })
            .collect()
    });
    (server, url, handle)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
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
fn auth_discovery_filters_and_wire_adaptation() {
    {
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

    {
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

    {
        let native = json!({"model":"gpt-fixture","store":true,"stream":false,"max_output_tokens":10,"previous_response_id":"old","instructions":"rules","input":[
            {"id":"msg-1","role":"system","content":"policy"},
            {"id":"call-1","type":"function_call","call_id":"call-1","name":"read","arguments":"{}"},
            {"type":"function_call_output","call_id":"call-1","output":"ok"},
            {"id":"rs-1","type":"reasoning","encrypted_content":"encrypted"}
        ]});
        let wire =
            serde_json::to_value(wire_request(&native, Some(Provider::OpenaiCodex)).unwrap())
                .unwrap();
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
        for provider in [Provider::OpencodeZen, Provider::OpencodeGo] {
            let req = json!({"model":"gpt-5","reasoning_effort":"none","messages":[{"role":"user","content":"hi"}]});
            let adapted =
                serde_json::to_value(wire_request(&req, Some(provider)).unwrap()).unwrap();
            assert!(adapted.get("reasoning_effort").is_none());
            let high_req = json!({"model":"gpt-5","reasoning_effort":"high","messages":[{"role":"user","content":"hi"}]});
            let high_adapted =
                serde_json::to_value(wire_request(&high_req, Some(provider)).unwrap()).unwrap();
            assert_eq!(high_adapted["reasoning_effort"], "high");
            let hdrs = headers(provider, None, Some("pp-session-1")).unwrap();
            assert_eq!(hdrs["x-opencode-session"], "pp-session-1");
            assert_eq!(hdrs["x-opencode-client"], "pp");
            assert!(headers(provider, None, None).is_err());
        }
    }

    {
        use crate::models::Protocol;

        for provider in [Provider::OpencodeZen, Provider::OpencodeGo] {
            let models = discovered_models(
                provider,
                &json!({"object":"list", "data":[
                    {"id":"gpt-5", "type":"chat", "limit":{"context":400000}, "capabilities":{"tools":{"supported":true}}, "api":{"responses":{"base_url":"https://opencode.ai/inference/openai/v1"}}},
                    {"id":"big-pickle", "type":"chat", "limit":{"context":200000}, "capabilities":{"tools":{"supported":true}}, "api":{"chat-completions":{"base_url":"https://opencode.ai/inference/openai/v1"}}},
                    {"id":"claude-sonnet-4-6", "type":"chat", "capabilities":{"tools":{"supported":true}}, "api":{"messages":{"base_url":"https://opencode.ai/inference/anthropic/v1"}}},
                    {"id":"gemini-4-argon", "type":"chat", "capabilities":{"tools":{"supported":true}}, "api":{"generate-content":{"base_url":"https://opencode.ai/inference/google/v1beta"}}},
                    {"id":"no-tools", "type":"chat", "capabilities":{"tools":{"supported":false}}, "api":{"chat-completions":{}}},
                    {"id":"embed", "type":"embedding", "capabilities":{"tools":{"supported":true}}, "api":{"chat-completions":{}}},
                    {"id":"empty-api", "type":"chat", "api":{}}
                ]}),
            )
            .unwrap();
            assert_eq!(models.len(), 2);
            assert_eq!(models["gpt-5"].protocol, Some(Protocol::Response));
            assert_eq!(models["gpt-5"].context_window, Some(400_000));
            assert_eq!(models["big-pickle"].protocol, Some(Protocol::Completion));
            assert_eq!(models["big-pickle"].context_window, Some(200_000));
            assert!(
                discovered_models(
                    provider,
                    &json!({"data":[
                        {"id":"claude", "type":"chat", "api":{"messages":{}}},
                        {"id":"gemini", "type":"chat", "api":{"generate-content":{}}}
                    ]})
                )
                .is_err()
            );
        }
    }
}

mod integration {
    use super::*;

    #[test]
    fn auth_http_discovery_roundtrip_and_cancellation() {
        {
            use crate::auth::{Store, models, service};
            use crate::models::Protocol;
            use pp_core::cancellation::CancellationToken;

            for provider in [
                Provider::GithubCopilot,
                Provider::CommandCode,
                Provider::OpencodeZen,
                Provider::OpencodeGo,
            ] {
                let id = provider.id();
                let dir = tempfile::tempdir().unwrap();
                let mut fixture = Credential::api_key("private-fixture".into());
                if matches!(provider, Provider::OpencodeZen | Provider::OpencodeGo) {
                    fixture.device_id = Some("pp-fixture-session".into());
                }
                service::commit_login(dir.path(), provider, fixture, &CancellationToken::new())
                    .unwrap();
                let store = Store::load(&dir.path().join("agent.db")).unwrap();
                let credential = &store.credentials[&provider];
                let (_owner, base, server) = mock(vec![(
                    200,
                    if matches!(provider, Provider::OpencodeZen | Provider::OpencodeGo) {
                        json!({"data":[
                            {"id":"a-response", "api":{"responses":{}}, "context_window":128000},
                            {"id":"vendor/b-chat", "api":{"chat-completions":{}}},
                            {"id":"c-response", "api":{"responses":{}}},
                            {"id":"d-legacy"}
                        ]})
                    } else {
                        json!({"data":[
                            {"id":"a-response", "supported_endpoints":["/responses"], "context_window":128000},
                            {"id":"vendor/b-chat", "supported_endpoints":["/chat/completions"]},
                            {"id":"c-response", "supported_endpoints":["/responses"]},
                            {"id":"d-legacy"}
                        ]})
                    },
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
                    assert_eq!(config.thinking, crate::models::ModelThinking::default());
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
                let update =
                    service::logout(dir.path(), provider, &CancellationToken::new()).unwrap();
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

        {
            for provider in Provider::ALL {
                let data = if provider == Provider::OpenaiCodex {
                    json!({"models":[{"slug":"remote-model"}]})
                } else {
                    json!({"data":[{"id":"remote-model", "context_length":32768}]})
                };
                let (_owner, base, server) = mock(vec![
                    (200, data),
                    (404, json!({"secret":"private-body"})),
                    (401, json!({"error":"private-body"})),
                ]);
                let mut credential = Credential::api_key("private-token".into());
                credential.account_id = Some("account-fixture".into());
                credential.device_id = Some("device-fixture".into());
                runtime().block_on(async {
                    let client = client().unwrap();
                    let models =
                        discover_at(&client, provider, &credential, &format!("{base}/models"))
                            .await
                            .unwrap();
                    assert!(models.contains_key("remote-model"));
                    for _ in 0..2 {
                        let error =
                            discover_at(&client, provider, &credential, &format!("{base}/models"))
                                .await
                                .err()
                                .unwrap();
                        assert!(!format!("{error:#}").contains("private"));
                    }
                });
                let requests = server.join().unwrap();
                assert!(requests.iter().all(|request| {
                    request.headers.iter().any(|(name, value)| {
                        name == "authorization" && value == "Bearer private-token"
                    })
                }));
                if provider == Provider::OpenaiCodex {
                    assert!(
                        requests[0]
                            .headers
                            .iter()
                            .any(|(name, value)| name == "chatgpt-account-id"
                                && value == "account-fixture")
                    );
                }
                if matches!(provider, Provider::OpencodeZen | Provider::OpencodeGo) {
                    assert!(
                        requests[0]
                            .headers
                            .iter()
                            .any(|(name, value)| name == "x-opencode-session"
                                && value == "device-fixture")
                    );
                    assert!(
                        requests[0]
                            .headers
                            .iter()
                            .any(|(name, value)| name == "x-opencode-client" && value == "pp")
                    );
                }
            }
        }

        {
            let server = TestServer::start().unwrap();
            let url = format!("http://{}/models", server.addr());
            let token = pp_core::cancellation::CancellationToken::new();
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
    }

    #[test]
    fn auth_http_device_logins_and_token_refresh() {
        {
            let (_owner, base, server) = mock(vec![
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
                let first = device_login_with_poll_unit(
                    &client,
                    Provider::KimiCode,
                    &format!("{base}/device"),
                    &format!("{base}/token"),
                    KIMI_CLIENT,
                    "",
                    Duration::from_millis(5),
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

        {
            let (_owner, base, server) = mock(vec![
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
                device_login_with_poll_unit(
                    &client().unwrap(),
                    Provider::GithubCopilot,
                    &format!("{base}/device"),
                    &format!("{base}/token"),
                    GITHUB_CLIENT,
                    &format!("{base}/user"),
                    Duration::from_millis(5),
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

        {
            let token = jwt("account-fixture");
            let (_owner, base, server) = mock(vec![
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
                codex_login_at_with_poll_unit(
                    &client().unwrap(),
                    &format!("{base}/device"),
                    &format!("{base}/poll"),
                    &format!("{base}/exchange"),
                    Duration::from_millis(5),
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
            assert!(
                body.contains("redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback")
            );
            assert!(body.contains("grant_type=authorization_code"));
        }
    }

    #[test]
    fn auth_http_refresh_redaction_and_cancellation() {
        {
            let (_owner, base, server) = mock(vec![(
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

        {
            let server = Arc::new(TestServer::start().unwrap());
            let url = format!("http://{}", server.addr());
            let cancellation = pp_core::cancellation::CancellationToken::new();
            let cancel = cancellation.clone();
            let (release, wait) = std::sync::mpsc::channel();
            let responder = Arc::clone(&server);
            let handle = thread::spawn(move || {
                let request = responder
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
    }
}
