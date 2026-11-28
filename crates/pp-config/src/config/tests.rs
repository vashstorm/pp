use super::*;
use serde_json::json;

mod sandbox_cases;
mod settings_cases;
mod web_search_cases;

/// Deterministic interleaving control point for cross-process settings saves: syncs only after a writer thread
/// explicitly registers, and passes through with zero overhead otherwise. See the lock-guarded critical section in `update_settings_fields_returning`.
pub(crate) mod settings_interleave;

const VALID_SETTINGS: &str = "{}";
const VALID_MODELS: &str = r#"{"providers":{"fixture": {"api_key":"secret-marker-fixture-key",
        "models":{"primary": {"model":"fixture-model"}}
    }}}"#;

fn catalog(
    settings: Option<&str>,
    models: Option<&str>,
) -> Result<(tempfile::TempDir, ModelCatalog)> {
    let dir = tempfile::tempdir().unwrap();
    if let Some(settings) = settings {
        fs::write(dir.path().join("settings.yml"), settings).unwrap();
    }
    if let Some(models) = models {
        fs::write(dir.path().join("models.yml"), models).unwrap();
    }
    let catalog = ModelCatalog::from_files(
        &dir.path().join("settings.yml"),
        &dir.path().join("models.yml"),
    )?;
    Ok((dir, catalog))
}

/// Seeds preferences straight into agent.db before the first catalog is built; the caller keeps the TempDir alive.
fn seeded_catalog(
    settings: Option<&str>,
    models: Option<&str>,
    seed: impl FnOnce(&mut crate::agent_store::AgentStore),
) -> Result<(tempfile::TempDir, ModelCatalog)> {
    let dir = tempfile::tempdir().unwrap();
    if let Some(settings) = settings {
        fs::write(dir.path().join("settings.yml"), settings).unwrap();
    }
    if let Some(models) = models {
        fs::write(dir.path().join("models.yml"), models).unwrap();
    }
    crate::agent_store::initialize(dir.path(), &pp_core::cancellation::CancellationToken::new())?;
    {
        let mut store = crate::agent_store::AgentStore::open(&dir.path().join("agent.db")).unwrap();
        seed(&mut store);
    }
    let catalog = ModelCatalog::from_files(
        &dir.path().join("settings.yml"),
        &dir.path().join("models.yml"),
    )?;
    Ok((dir, catalog))
}

fn load(settings: Option<&str>, models: Option<&str>) -> Result<Config> {
    let (_dir, catalog) = catalog(settings, models)?;
    let config = catalog.resolve(catalog.default_reference())?;
    drop(catalog);
    Ok(config)
}

fn report(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

fn rejection(settings: Option<&str>, models: Option<&str>) -> String {
    let error = load(settings, models)
        .err()
        .expect("invalid config was accepted");
    let chain = report(&error);
    assert!(!chain.contains("secret-marker"), "{chain}");
    chain
}

#[test]
fn config_model_cost_and_thinking_validation() {
    {
        let mut models: serde_json::Value = serde_json::from_str(VALID_MODELS).unwrap();
        models["providers"]["fixture"]["models"]["primary"]["cost"] =
            json!({"input":0.8,"output":2.8,"cacheRead":0.23});
        let (_dir, configured) = catalog(None, Some(&models.to_string())).unwrap();
        let cost = configured.providers[0].models[0].cost.unwrap();
        assert_eq!(cost.input, 0.8);
        assert_eq!(cost.output, 2.8);
        assert_eq!(cost.cache_read, 0.23);
        assert_eq!(cost.cache_write, None);
        models["providers"]["fixture"]["models"]["primary"]["cost"]["cacheWrite"] = json!(1.25);
        let (_dir, with_cache_write) = catalog(None, Some(&models.to_string())).unwrap();
        assert_eq!(
            with_cache_write.providers[0].models[0]
                .cost
                .unwrap()
                .cache_write,
            Some(1.25)
        );
        for invalid in [
            json!(null),
            json!({"input":-1,"output":1,"cacheRead":0}),
            json!({"input":1,"output":1}),
            json!({"input":"secret-marker","output":1,"cacheRead":0}),
            json!({"input":1,"output":1,"cacheRead":0,"cacheWrite":-1}),
            json!({"input":1,"output":1,"cacheRead":0,"cacheWrite":null}),
            json!({"input":1,"output":1,"cacheRead":0,"cacheWrite":"secret-marker"}),
        ] {
            models["providers"]["fixture"]["models"]["unselected"] =
                json!({"model":"fixture-model","cost":invalid});
            rejection(None, Some(&models.to_string()));
        }
    }

    {
        for level in ["none", "default", "low", "medium", "high", "xhigh", "max"] {
            let parsed = level.parse::<ThinkLevel>().unwrap();
            let (dir, mut catalog) = seeded_catalog(None, Some(VALID_MODELS), |store| {
                store.set_default_effort(parsed, |_, _| Ok(())).unwrap();
            })
            .unwrap();
            assert_eq!(catalog.default_think_level(), parsed);
            catalog.save_default_think_level(ThinkLevel::Low).unwrap();
            let restored = ModelCatalog::from_files(
                &dir.path().join("settings.yml"),
                &dir.path().join("models.yml"),
            )
            .unwrap();
            assert_eq!(restored.default_think_level(), ThinkLevel::Low);
        }
        assert_eq!(
            catalog(None, Some(VALID_MODELS))
                .unwrap()
                .1
                .default_think_level(),
            ThinkLevel::None
        );
        for value in [
            json!(null),
            json!(0),
            json!(true),
            json!("HIGH"),
            json!(" high"),
            json!("ultra"),
            json!("no think"),
            json!("NONE"),
        ] {
            rejection(
                Some(&json!({"think_level":value}).to_string()),
                Some(VALID_MODELS),
            );
        }
        for level in ["HIGH", " high", "ultra", ""] {
            assert!(level.parse::<ThinkLevel>().is_err(), "{level}");
        }
    }

    {
        let make = |thinking: serde_json::Value| {
            let mut models: serde_json::Value = serde_json::from_str(VALID_MODELS).unwrap();
            models["providers"]["fixture"]["models"]["primary"]["thinking"] = thinking;
            models
        };
        for thinking in [
            json!({}),
            json!({"efforts":[]}),
            json!({"defaultLevel":"default"}),
        ] {
            let config = load(None, Some(&make(thinking).to_string())).unwrap();
            assert!(config.thinking.supports(ThinkLevel::None));
            assert_eq!(
                config.thinking.levels().collect::<Vec<_>>(),
                ThinkLevel::ALL
            );
        }
        let config = load(
            None,
            Some(
                &make(json!({"efforts":["high","low","high"],"defaultLevel":"high","extra":true}))
                    .to_string(),
            ),
        )
        .unwrap();
        let policy = config.thinking;
        assert_eq!(
            policy.levels().collect::<Vec<_>>(),
            [ThinkLevel::Low, ThinkLevel::High]
        );
        assert_eq!(policy.effective_default(ThinkLevel::Max), ThinkLevel::High);
        assert_eq!(policy.explicit_default(), Some(ThinkLevel::High));
        assert_eq!(policy.next(ThinkLevel::High), ThinkLevel::Low);
        assert_eq!(policy.next(ThinkLevel::Low), ThinkLevel::High);
        assert_eq!(policy.next(ThinkLevel::Max), ThinkLevel::Low);
        let config = load(
            None,
            Some(&make(json!({"efforts":["xhigh","medium"]})).to_string()),
        )
        .unwrap();
        assert_eq!(
            config.thinking.effective_default(ThinkLevel::Max),
            ThinkLevel::Medium
        );
        assert_eq!(
            config.thinking.effective_default(ThinkLevel::Xhigh),
            ThinkLevel::Xhigh
        );
        let config = load(None, Some(&make(json!({"efforts":["low"]})).to_string())).unwrap();
        assert_eq!(config.thinking.next(ThinkLevel::Low), ThinkLevel::Low);
        let mut invalid = vec![
            json!(null),
            json!(true),
            json!([]),
            json!("secret-marker"),
            json!({"efforts":["low"],"defaultLevel":"high"}),
        ];
        for value in [
            json!(null),
            json!(true),
            json!(1),
            json!(1.5),
            json!("secret-marker"),
            json!({}),
        ] {
            invalid.push(json!({"efforts":value}));
            invalid.push(json!({"defaultLevel":value}));
        }
        for value in [
            json!(null),
            json!(true),
            json!(1),
            json!("none"),
            json!("default"),
            json!("HIGH"),
            json!(" high"),
            json!("high "),
            json!("secret-marker"),
            json!([]),
        ] {
            invalid.push(json!({"efforts":[value]}));
        }
        for value in ["HIGH", " high", "high ", "secret-marker"] {
            invalid.push(json!({"defaultLevel":value}));
        }
        for thinking in invalid {
            let models = make(thinking);
            rejection(None, Some(&models.to_string()));
            let mut unselected: serde_json::Value = serde_json::from_str(VALID_MODELS).unwrap();
            unselected["providers"]["fixture"]["models"]["unselected"] =
                models["providers"]["fixture"]["models"]["primary"].clone();
            rejection(None, Some(&unselected.to_string()));
        }
        let error = rejection(None, Some(&make(json!({"efforts":["none"]})).to_string()));
        assert!(error.contains("thinking.efforts must contain only low, medium, high, xhigh, max"));
        let error = rejection(
            None,
            Some(&make(json!({"efforts":["low"],"defaultLevel":"high"})).to_string()),
        );
        assert!(error.contains("thinking.defaultLevel must be included in thinking.efforts"));
    }
}

#[test]
fn config_defaults_selection_and_edit_modes() {
    {
        let providers = r#"{"providers":{
                "z-first": {"protocol":"completion","api_key":"first-key","base_url":"https://a.example/v1","models":{
                    "z-first": {"model":"first-model"},
                    "a-second": {"model":"not-selected"}}},
                "a-second": {"api_key":"second-key","models":{
                    "primary": {"model":"also-not-selected"}}}}}"#;
        for settings in [None, Some("{}")] {
            let config = load(settings, Some(providers)).unwrap();
            assert_eq!(config.settings.edit, EditMode::Replace);
            assert_eq!(config.settings.request_timeout_seconds, 300);
            assert_eq!(config.settings.rate_limit_retry_budget_seconds, 1022);
            assert_eq!(config.api_key, "first-key");
            assert_eq!(config.model, "first-model");
            assert_eq!(config.base_url.as_str(), "https://a.example/v1");
            assert_eq!(config.protocol, models::Protocol::Completion);
        }
        rejection(None, None);
    }

    {
        let config = load(
                Some(r#"{"default_model":"second/secondary","request_timeout_seconds":7}"#),
                Some(
                    r#"{"providers":{
                        "first": {"protocol":"response","api_key":"first-key","base_url":"https://a.example/v1","models":{
                            "primary": {"model":"first-primary"},
                            "secondary": {"model":"first-secondary"}}},
                        "second": {"protocol":"completion","api_key":"second-key","base_url":"http://localhost:8080/custom/v1/","models":{
                            "primary": {"model":"second-primary"},
                            "secondary": {"model":"second-secondary"}}}}}"#,
                ),
            )
            .unwrap();
        assert_eq!(config.api_key, "second-key");
        assert_eq!(config.settings.request_timeout_seconds, 7);
        assert_eq!(config.model, "second-secondary");
        assert_eq!(config.base_url.as_str(), "http://localhost:8080/custom/v1");
        assert_eq!(config.protocol, models::Protocol::Completion);
    }

    {
        for (mode, expected) in [
            ("replace", EditMode::Replace),
            ("hashline", EditMode::Hashline),
        ] {
            let settings = json!({"edit": mode}).to_string();
            assert_eq!(
                load(Some(&settings), Some(VALID_MODELS))
                    .unwrap()
                    .settings
                    .edit,
                expected
            );
        }
        for value in [
            json!(""),
            json!("Replace"),
            json!("HASHLINE"),
            json!(" replace"),
            json!("hashline "),
            json!("secret-marker"),
            json!(null),
            json!(1),
            json!(true),
            json!(["secret-marker"]),
            json!({"secret-marker": true}),
        ] {
            let settings = json!({"edit": value}).to_string();
            rejection(Some(&settings), Some(VALID_MODELS));
        }
    }

    {
        assert_eq!(models::Protocol::default(), models::Protocol::Response);
        let providers = r#"{"providers":{
                "completion": {"protocol":"completion","api_key":"completion-key","models":{
                    "primary": {"model":"shared-model"}}},
                "default": {"api_key":"default-key","models":{
                    "primary": {"model":"shared-model"}}},
                "response": {"protocol":"response","api_key":"response-key","models":{
                    "primary": {"model":"shared-model"}}}}}"#;
        for (provider, expected) in [
            ("completion", models::Protocol::Completion),
            ("default", models::Protocol::Response),
            ("response", models::Protocol::Response),
        ] {
            let settings = json!({"default_model": format!("{provider}/primary")}).to_string();
            let config = load(Some(&settings), Some(providers)).unwrap();
            assert_eq!(config.protocol, expected);
            assert_eq!(config.model, "shared-model");
            assert_eq!(config.api_key, format!("{provider}-key"));
            assert_eq!(config.base_url.as_str(), "https://api.openai.com/v1");
        }
    }
}

#[test]
fn config_reference_matching_and_scope_constraints() {
    {
        for reference in [
            "",
            "  ",
            "primary",
            "fixture",
            "secret-marker",
            "/",
            "/secret-marker",
            "secret-marker/",
            " /secret-marker",
            "secret-marker/ \t",
        ] {
            rejection(
                Some(&json!({"default_model": reference}).to_string()),
                Some(VALID_MODELS),
            );
        }
    }

    {
        let models = r#"{"providers":{
                "first": {"api_key":"first-key","models":{
                    "primary": {"model":"first-model"}}},
                "second": {"api_key":"second-key","models":{
                    "secondary": {"model":"second-model"}}}}}"#;
        for reference in [
            "secret-marker/primary",
            "first/secret-marker",
            "second/primary",
        ] {
            let settings = json!({"default_model": reference}).to_string();
            let config = load(Some(&settings), Some(models)).unwrap();
            assert_eq!(config.api_key, "first-key");
            assert_eq!(config.model, "first-model");
        }
    }

    {
        let models = r#"{"providers":{
                " Fixture": {"api_key":"secret-marker","models":{
                    " Primary": {"model":"spaced"},
                    "team/model": {"model":"slashed"}}},
                "fixture": {"api_key":"secret-marker","models":{
                    "primary": {"model":"lowercase"}}},
                "Fixture": {"api_key":"secret-marker","models":{
                    "primary": {"model":"mixed-case-provider"},
                    " primary": {"model":"spaced-model"},
                    "PRIMARY": {"model":"uppercase-model"}}}}}"#;
        for (reference, expected) in [
            (" Fixture/ Primary", "spaced"),
            (" Fixture/team/model", "slashed"),
            ("fixture/primary", "lowercase"),
            ("Fixture/primary", "mixed-case-provider"),
            ("Fixture/ primary", "spaced-model"),
            ("Fixture/PRIMARY", "uppercase-model"),
        ] {
            let settings = json!({"default_model": reference}).to_string();
            assert_eq!(load(Some(&settings), Some(models)).unwrap().model, expected);
        }
        for reference in [
            "Fixture/ Primary",
            " fixture/ Primary",
            " Fixture/Primary",
            " Fixture/ primary",
            "FIXTURE/primary",
            "fixture/PRIMARY",
            "fixture/primary ",
            "fixture/team/model",
        ] {
            let settings = json!({"default_model": reference}).to_string();
            assert_eq!(load(Some(&settings), Some(models)).unwrap().model, "spaced");
        }
    }

    {
        let provider = json!({
            "api_key": "secret-marker",
            "models": {"primary": { "model": "m"}}
        });
        let mut cases = vec![json!({"providers": {}})];
        for id in ["", " \t", "secret-marker/provider"] {
            cases.push(json!({"providers": {id: provider.clone()}}));
        }
        for models in [
            json!([]),
            json!({}),
            json!({"": {"model": "secret-marker"}}),
            json!({" \t": {"model": "secret-marker"}}),
        ] {
            let mut invalid = provider.clone();
            invalid["models"] = models;
            cases.push(json!({"providers": {"fixture": invalid}}));
        }
        for models in cases {
            rejection(Some(VALID_SETTINGS), Some(&models.to_string()));
        }
    }
}

#[test]
fn config_yaml_diagnostics_and_redaction() {
    {
        for value in [
            "0",
            "-1",
            "1.5",
            "20.0",
            "\"secret-marker\"",
            "null",
            "1e30",
            "18446744073709551616",
        ] {
            let settings = format!(r#"{{"request_timeout_seconds":{value}}}"#);
            rejection(Some(&settings), Some(VALID_MODELS));
        }
    }

    {
        for (settings, location, reason) in [
            (
                "tui:\n  max_fps: [secret-marker\n",
                "line 3, column 1",
                "syntax",
            ),
            (
                "debug: true\ndebug: secret-marker",
                "line 1, column 1",
                "duplicate mapping key",
            ),
            (
                "tui: {max_fps: secret-marker}",
                "tui.max_fps",
                "expected u16",
            ),
            (
                "disabled_skills: [valid, {bad: secret-marker}]",
                "disabled_skills[1]",
                "invalid field type",
            ),
            ("edit: secret-marker", "`edit`", "unsupported enum value"),
        ] {
            let chain = rejection(Some(settings), Some(VALID_MODELS));
            assert!(chain.contains("settings.yml"), "{chain}");
            assert!(
                chain.contains(location) && chain.contains(reason),
                "{chain}"
            );
        }

        for (model, field, reason) in [
            (
                "{model: some-model, context_window: secret-marker}",
                "providers.fixture.models.primary.context_window",
                "invalid field type",
            ),
            (
                "{context_window: 2048}",
                "providers.fixture.models.primary.model",
                "missing required field",
            ),
        ] {
            let models = format!(
                "providers:\n  fixture:\n    api_key: secret-marker\n    models:\n      primary: {model}\n"
            );
            let chain = rejection(None, Some(&models));
            assert!(chain.contains("models.yml"), "{chain}");
            assert!(chain.contains(field), "{chain}");
            assert!(chain.contains(reason), "{chain}");
        }
    }

    {
        let models = r#"{"models":{"primary":{"api_key":"secret-marker","model":"m"}}}"#;
        let error = rejection(Some(VALID_SETTINGS), Some(models));
        assert!(error.contains("`providers`") && error.contains("missing required field"));
    }

    {
        for models in [
            r#"{"providers":{"fixture": {"api_key":["secret-marker"],"models":{"primary": {"model":"m"}}}}}"#,
            r#"{"providers":{"fixture": {"api_key":"secret-marker","models":{"primary": {"model":""}}}}}"#,
            r#"{"providers":{"fixture": {"api_key":" ","models":{"primary": {"model":"secret-marker"}}}}}"#,
            r#"{"providers":{"fixture": {"api_key":"secret-marker","base_url":"","models":{"primary": {"model":"m"}}}}}"#,
            r#"{"providers":{"fixture": {"api_key":"secret-marker","base_url":"ftp://secret-marker","models":{"primary": {"model":"m"}}}}}"#,
            r#"{"providers":{"fixture": {"api_key":"secret-marker" "models":{"primary": {"model":"m"}}}}}"#,
            r#"{"providers":[{"id":"fixture","api_key":"secret-marker", /* unterminated"#,
        ] {
            rejection(Some(VALID_SETTINGS), Some(models));
        }
        for settings in [
            r#"{"default_model":["secret-marker"]}"#,
            r#"{"default_model":true,"secret-marker":1}"#,
            r#"{"default_model" "secret-marker"}"#,
        ] {
            rejection(Some(settings), Some(VALID_MODELS));
        }
    }
}

#[test]
fn config_provider_protocol_base_url_and_unselected_validation() {
    {
        let valid: serde_json::Value = serde_json::from_str(VALID_MODELS).unwrap();
        for field in ["protocol", "base_url"] {
            for value in [
                json!(null),
                json!(true),
                json!({"completion": null}),
                json!({"response": null}),
            ] {
                let mut invalid = valid.clone();
                invalid["providers"]["fixture"][field] = value;
                rejection(Some(VALID_SETTINGS), Some(&invalid.to_string()));
            }
        }
        for protocol in [
            "",
            "openai",
            "anthropic",
            "Completion",
            "completion ",
            "secret-marker",
        ] {
            let mut invalid = valid.clone();
            invalid["providers"]["fixture"]["protocol"] = json!(protocol);
            rejection(Some(VALID_SETTINGS), Some(&invalid.to_string()));
        }
    }

    {
        let valid: serde_json::Value = serde_json::from_str(VALID_MODELS).unwrap();
        let other = json!({
            "api_key": "secret-marker",
            "models": {"primary": { "model": "other-model"}}
        });
        let mut invalid_providers = Vec::new();
        for (field, value) in [
            ("api_key", json!(" \t")),
            ("protocol", json!("openai")),
            ("protocol", json!("anthropic")),
            ("protocol", json!("secret-marker")),
            ("protocol", json!(null)),
            ("protocol", json!({"completion": null})),
            ("protocol", json!({"response": null})),
            ("base_url", json!("not-a-url-secret-marker")),
            (
                "base_url",
                json!("https://secret-marker.example/v1?token=secret-marker"),
            ),
            ("models", json!([])),
            ("models", json!({"primary": {"model": " \t"}})),
            ("models", json!({"": {"model": "secret-marker"}})),
        ] {
            let mut invalid = other.clone();
            invalid[field] = value;
            invalid_providers.push(invalid);
        }
        for provider in invalid_providers {
            let mut invalid = valid.clone();
            invalid["providers"]["other"] = provider;
            for settings in [
                VALID_SETTINGS,
                r#"{"default_model":"secret-marker/missing"}"#,
            ] {
                rejection(Some(settings), Some(&invalid.to_string()));
            }
        }
        let mut invalid = valid.clone();
        invalid["providers"]["fixture"]["models"]["other"] = json!({"model": " \t"});
        rejection(Some(VALID_SETTINGS), Some(&invalid.to_string()));
    }

    {
        for base_url in [None, Some("https://secret-marker.example/custom")] {
            let mut anthropic = json!({
                "protocol": "anthropic", "api_key": "secret-marker",
                "models": {"primary": { "model": "secret-marker-model"}}
            });
            if let Some(base_url) = base_url {
                anthropic["base_url"] = json!(base_url);
            }
            let valid: serde_json::Value = serde_json::from_str(VALID_MODELS).unwrap();
            let models = json!({"providers": {"anthropic": anthropic, "fixture": valid["providers"]["fixture"]}}).to_string();
            for settings in [
                None,
                Some("{}"),
                Some(VALID_SETTINGS),
                Some(r#"{"default_model":"anthropic/primary"}"#),
                Some(r#"{"default_model":"anthropic/secret-marker"}"#),
            ] {
                rejection(settings, Some(&models));
            }
        }
    }
}
