use super::*;
use serde_json::json;

#[test]
fn grep_context_settings_are_strict_and_default_when_absent() {
    let settings: Settings = yaml::parse("{}", "settings").unwrap();
    assert_eq!(
        (settings.grep.context_before, settings.grep.context_after),
        (1, 3)
    );
    let settings: Settings =
        yaml::parse("grep: {contextBefore: 0, contextAfter: 5}", "settings").unwrap();
    assert_eq!(
        (
            settings.tool_limits().grep.context_before,
            settings.tool_limits().grep.context_after
        ),
        (0, 5)
    );
    for input in [
        "grep: null",
        "grep: {contextBefore: -1}",
        "grep: {contextAfter: null}",
        "grep: {extra: 1}",
    ] {
        assert!(
            yaml::parse::<Settings>(input, "settings").is_err(),
            "{input}"
        );
    }
    let limits: ToolLimits = serde_json::from_value(
        json!({"read_max_bytes": 10, "read_max_lines": 10, "bash_timeout_seconds": 10}),
    )
    .unwrap();
    assert_eq!(
        (limits.grep.context_before, limits.grep.context_after),
        (1, 3)
    );
}

#[test]
fn read_preview_defaults_and_strict_nested_settings() {
    for text in ["{}", "read: {}", "read: {toolResultPreview: false}"] {
        let settings: Settings = yaml::parse(text, "settings").unwrap();
        assert!(!settings.read.tool_result_preview);
    }
    let settings: Settings = yaml::parse("read: {toolResultPreview: true}", "settings").unwrap();
    assert!(settings.read.tool_result_preview);
    for text in [
        "read: null",
        "read: true",
        "read: {toolResultPreview: null}",
        "read: {toolResultPreview: 1}",
        "read: {toolResultPreview: 'true'}",
        "read: {unknown: true}",
        "read: {toolResultPreview: true, toolResultPreview: false}",
    ] {
        assert!(yaml::parse::<Settings>(text, "settings").is_err(), "{text}");
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.yml");
    fs::write(&path, "read:\n  toolResultPreview: true\n").unwrap();
    update_settings(&path, |_| {
        Ok(("edit", serde_yaml::Value::String("hashline".into())))
    })
    .unwrap();
    let settings = read_settings(&path).unwrap();
    assert!(settings.read.tool_result_preview);
    assert_eq!(settings.edit, EditMode::Hashline);
}

#[test]
fn favorite_models_default_validate_and_preserve_unavailable_references() {
    for settings in [None, Some("{}")] {
        let (_dir, catalog) = catalog(settings, Some(VALID_MODELS)).unwrap();
        assert!(catalog.favorite_models().is_empty());
    }
    // 旧 settings.yml 的收藏经一次性迁移进入 agent.db；已移除模型的引用继续保留，
    // 旧字段不再是 settings.yml 落点。
    let (dir, catalog) = catalog(
        Some("favorite_models: [removed/team/model, fixture/primary]"),
        Some(VALID_MODELS),
    )
    .unwrap();
    assert_eq!(
        catalog.favorite_models(),
        ["removed/team/model", "fixture/primary"]
    );
    let text = fs::read_to_string(dir.path().join("settings.yml")).unwrap();
    assert!(!text.contains("favorite_models"));
    // 结构校验沿用迁移前规则：收藏必须是复合引用。
    for favorites in [
        vec![""],
        vec!["secret-marker"],
        vec!["/secret-marker"],
        vec!["secret-marker/"],
    ] {
        let preferences = ModelPreferences {
            favorite_models: favorites.iter().map(|item| item.to_string()).collect(),
            ..Default::default()
        };
        assert!(preferences.validate().is_err(), "{favorites:?}");
    }
    // 非法类型的旧字段让启动失败，不会被静默当作空收藏。
    rejection(Some("favorite_models: secret-marker"), Some(VALID_MODELS));
}

#[test]
fn favorite_models_save_reload_toggle_and_fail_without_changing_state() {
    let (dir, mut catalog) = catalog(None, Some(VALID_MODELS)).unwrap();
    let settings_path = dir.path().join("settings.yml");
    let models_path = dir.path().join("models.yml");
    fs::write(&settings_path, "# 保留注释\nedit: hashline\n").unwrap();
    assert_eq!(
        catalog.toggle_favorite_model("fixture/primary").unwrap(),
        ["fixture/primary"]
    );
    // 偏好写入 agent.db；settings.yml 不再承载收藏，无关字段与注释保持原样。
    let saved = fs::read_to_string(&settings_path).unwrap();
    assert!(saved.contains("# 保留注释"));
    assert!(saved.contains("edit: hashline"));
    assert!(!saved.contains("favorite_models"));
    // 重启后从 agent.db 恢复。
    let restored = ModelCatalog::from_files(&settings_path, &models_path).unwrap();
    assert_eq!(restored.favorite_models(), ["fixture/primary"]);
    // 不可用模型不能收藏，失败不改变已提交状态。
    assert!(catalog.toggle_favorite_model("unknown/model").is_err());
    assert_eq!(catalog.favorite_models(), ["fixture/primary"]);
    assert_eq!(fs::read_to_string(&models_path).unwrap(), VALID_MODELS);
    // agent.db 不可用时保存失败，catalog 内存快照保持已提交状态。
    let agent_db = dir.path().join("agent.db");
    fs::remove_file(&agent_db).unwrap();
    fs::create_dir(&agent_db).unwrap();
    assert!(catalog.toggle_favorite_model("fixture/primary").is_err());
    assert_eq!(catalog.favorite_models(), ["fixture/primary"]);
}

#[test]
fn notification_options_validate_overrides() {
    let settings = load(
        Some(r#"{"notification_show_countdown":false,"notification_timeout_seconds":3}"#),
        Some(VALID_MODELS),
    )
    .unwrap()
    .settings;
    assert!(!settings.notification_options().show_countdown);
    assert_eq!(settings.notification_options().timeout_seconds, 3);
    for value in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!(null),
        json!(true),
        json!("secret-marker"),
        json!(u64::MAX),
    ] {
        rejection(
            Some(&json!({"notification_timeout_seconds":value}).to_string()),
            Some(VALID_MODELS),
        );
    }
    rejection(
        Some(r#"{"notification_timeout_seconds":18446744073709551616}"#),
        Some(VALID_MODELS),
    );
    for value in [json!(0), json!(null), json!("secret-marker")] {
        rejection(
            Some(&json!({"notification_show_countdown":value}).to_string()),
            Some(VALID_MODELS),
        );
    }
}

#[test]
fn tool_limits_validate_positive_overrides() {
    let settings = load(
        Some(r#"{"read_max_bytes":17,"read_max_lines":3,"bash_timeout_seconds":2}"#),
        Some(VALID_MODELS),
    )
    .unwrap()
    .settings;
    assert_eq!(settings.read_max_bytes, 17);
    assert_eq!(settings.read_max_lines, 3);
    assert_eq!(settings.bash_timeout_seconds, 2);
    for field in ["read_max_bytes", "read_max_lines", "bash_timeout_seconds"] {
        for value in [
            json!(0),
            json!(-1),
            json!(null),
            json!(true),
            json!(1.5),
            json!("secret-marker-invalid"),
        ] {
            let settings = json!({field:value}).to_string();
            rejection(Some(&settings), Some(VALID_MODELS));
        }
    }
}

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

/// 在首次构建 catalog 前直接向 agent.db 播种偏好；TempDir 生命周期由调用方持有。
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
    crate::agent_store::initialize(dir.path(), &crate::cancellation::CancellationToken::new())?;
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

#[test]
fn model_cost_accepts_existing_rates_and_rejects_invalid_unselected_models() {
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
        json!({"input":1,"output":1,"cacheRead":0,"typo":0}),
        json!({"input":1,"output":1,"cacheRead":0,"cacheWrite":-1}),
        json!({"input":1,"output":1,"cacheRead":0,"cacheWrite":null}),
        json!({"input":1,"output":1,"cacheRead":0,"cacheWrite":"secret-marker"}),
    ] {
        models["providers"]["fixture"]["models"]["unselected"] =
            json!({"model":"fixture-model","cost":invalid});
        rejection(None, Some(&models.to_string()));
    }
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
fn think_level_config_accepts_exact_levels_and_rejects_invalid_values() {
    for level in ["none", "default", "low", "medium", "high", "xhigh", "max"] {
        let parsed = level.parse::<ThinkLevel>().unwrap();
        let (dir, mut catalog) = seeded_catalog(None, Some(VALID_MODELS), |store| {
            store.set_default_effort(parsed, |_, _| Ok(())).unwrap();
        })
        .unwrap();
        assert_eq!(catalog.default_think_level(), parsed);
        // 保存经 agent.db 持久化，重启后保持。
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
    // 解析后的结构校验同样拒绝非法级别。
    for level in ["HIGH", " high", "ultra", ""] {
        assert!(level.parse::<ThinkLevel>().is_err(), "{level}");
    }
}

#[test]
fn model_thinking_validates_sets_defaults_and_unselected_models() {
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
        Some(&make(json!({"efforts":["high","low","high"],"defaultLevel":"high"})).to_string()),
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
        json!({"unknown":"secret-marker"}),
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

#[test]
fn missing_or_empty_settings_default_to_first_provider_first_model() {
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
        assert_eq!(config.api_key, "first-key");
        assert_eq!(config.model, "first-model");
        assert_eq!(config.base_url.as_str(), "https://a.example/v1");
        assert_eq!(config.protocol, models::Protocol::Completion);
    }
    rejection(None, None);
}

#[test]
fn explicit_values_select_named_provider_model() {
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

#[test]
fn edit_modes_are_strict_and_redacted() {
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

#[test]
fn protocol_defaults_and_explicit_values_follow_selected_provider() {
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

#[test]
fn yaml_comments_and_multiline_strings_preserve_string_contents() {
    let config = load(
        Some("# 应用设置\ndefault_model: fixture/primary\n"),
        Some(
            r#"# 同一供应商的模型共享凭证
providers:
  fixture:
    api_key: |-
      key//with/*literal*/"quote # literal
    base_url: http://localhost:8080/custom/v1/ # 行尾注释
    models:
      primary:
        model: tool-model
"#,
        ),
    )
    .unwrap();
    assert_eq!(config.api_key, "key//with/*literal*/\"quote # literal");
    assert_eq!(config.model, "tool-model");
    assert_eq!(config.base_url.as_str(), "http://localhost:8080/custom/v1");
}

#[test]
fn malformed_composite_references_are_rejected_without_values() {
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

#[test]
fn unmatched_composite_references_fall_back_to_first_provider_first_model() {
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

#[test]
fn provider_and_model_ids_match_exactly_and_split_only_at_first_slash() {
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

#[test]
fn provider_and_model_scope_constraints_reject_whole_configuration() {
    let provider = json!({
        "api_key": "secret-marker",
        "models": {"primary": { "model": "m"}}
    });
    let mut cases = vec![
        json!({"providers": {}}),
        json!({"providers": {"fixture": provider.clone()}, "id": "secret-marker"}),
    ];
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

#[test]
fn invalid_settings_limits_and_numeric_types_are_rejected() {
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

#[test]
fn unknown_and_misplaced_fields_and_old_format_are_rejected() {
    for settings in [
        r#"{"secret-marker":true}"#,
        r#"{"api_key":"secret-marker"}"#,
        r#"{"models":["secret-marker"]}"#,
    ] {
        rejection(Some(settings), Some(VALID_MODELS));
    }
    let valid: serde_json::Value = serde_json::from_str(VALID_MODELS).unwrap();
    let mut cases = vec![
        // The removed flat format is intentionally accepted nowhere.
        json!({"models": {"primary": { "api_key": "secret-marker", "model": "m"}}}),
    ];
    for level in 0..3 {
        let mut invalid = valid.clone();
        let object = match level {
            0 => &mut invalid,
            1 => &mut invalid["providers"]["fixture"],
            _ => &mut invalid["providers"]["fixture"]["models"]["primary"],
        };
        object["secret-marker"] = json!(true);
        cases.push(invalid);
    }
    for (field, value) in [
        ("api_key", json!("secret-marker")),
        ("base_url", json!("https://secret-marker.example/v1")),
        ("protocol", json!("completion")),
    ] {
        let mut invalid = valid.clone();
        invalid["providers"]["fixture"]["models"]["primary"][field] = value;
        cases.push(invalid);
    }
    for models in cases {
        rejection(Some(VALID_SETTINGS), Some(&models.to_string()));
    }
}

#[test]
fn protocol_and_base_url_reject_null_wrong_types_and_unknown_protocols() {
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

#[test]
fn syntax_type_and_semantic_errors_do_not_disclose_values() {
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

#[test]
fn unselected_providers_and_models_are_validated_before_selection() {
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
        ("id", json!("")),
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
    for model in [
        json!({"model": " \t"}),
        json!({"id": "other", "model": "secret-marker"}),
    ] {
        let mut invalid = valid.clone();
        invalid["providers"]["fixture"]["models"]["other"] = model;
        rejection(Some(VALID_SETTINGS), Some(&invalid.to_string()));
    }
}

#[test]
fn anthropic_is_rejected_whether_selected_or_not() {
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

#[test]
fn missing_and_unparseable_files_are_reported_per_file() {
    rejection(Some(VALID_SETTINGS), None);
    for settings in ["{", "{\"default_model\" \"secret-marker\"}", ""] {
        rejection(Some(settings), Some(VALID_MODELS));
    }
    for models in ["{", "{\"providers\": \"secret-marker\"", ""] {
        let chain = rejection(Some(VALID_SETTINGS), Some(models));
        assert!(chain.contains("invalid YAML models config"), "{chain}");
    }
}
#[test]
fn skill_settings_default_empty_lists_and_strict_field_types() {
    let defaults = load(None, Some(VALID_MODELS)).unwrap().settings;
    assert_eq!(defaults.skill_dirs, ["~/.pp/agent/skills"]);
    assert!(defaults.disabled_skills.is_empty());
    assert!(defaults.skill_sources.is_empty());
    let settings = load(
        Some(r#"{"skill_dirs":[],"disabled_skills":[]}"#),
        Some(VALID_MODELS),
    )
    .unwrap()
    .settings;
    assert!(settings.skill_dirs.is_empty());
    for field in ["skill_dirs", "disabled_skills", "skill_sources"] {
        for value in [
            json!(null),
            json!("secret-marker"),
            json!([null]),
            json!([5]),
            json!([""]),
            json!(["  "]),
        ] {
            rejection(Some(&json!({field:value}).to_string()), Some(VALID_MODELS));
        }
    }
    rejection(Some("skill_sources: [unknown]"), Some(VALID_MODELS));
    let settings = load(Some("skill_sources: [claude, codex]"), Some(VALID_MODELS))
        .unwrap()
        .settings;
    assert_eq!(settings.skill_sources, crate::skills::SkillSource::ALL);
}

#[test]
fn context_window_omission_means_unknown_and_explicit_values_propagate() {
    let (_dir, default_catalog) = catalog(None, Some(VALID_MODELS)).unwrap();
    assert!(
        default_catalog
            .resolve(default_catalog.default_reference())
            .unwrap()
            .context_window
            .is_none()
    );
    assert!(
        default_catalog
            .choices()
            .iter()
            .all(|choice| choice.context_window.is_none())
    );
    let models = r#"{"providers":{"fixture": {"api_key":"fixture-key",
            "models":{
                "primary": {"model":"primary-model","context_window":32768},
                "secondary": {"model":"secondary-model"}}
        }}}"#;
    let (_dir, catalog) = catalog(
        Some(r#"{"default_model":"fixture/secondary"}"#),
        Some(models),
    )
    .unwrap();
    // The unselected model keeps its explicit capacity; the selected one
    // stays unknown, so capacities are per model and never inferred.
    assert_eq!(
        catalog
            .resolve(catalog.default_reference())
            .unwrap()
            .context_window,
        None
    );
    assert_eq!(
        catalog.resolve("fixture/primary").unwrap().context_window,
        Some(32768)
    );
    let choices = catalog.choices();
    let window = |reference: &str| {
        choices
            .iter()
            .find(|choice| choice.reference == reference)
            .unwrap()
            .context_window
    };
    assert_eq!(window("fixture/primary"), Some(32768));
    assert_eq!(window("fixture/secondary"), None);
    let models = r#"{"providers":{"fixture": {"api_key":"fixture-key",
            "models":{"primary": {"model":"m","context_window":18446744073709551615}}}}}"#;
    assert_eq!(
        load(Some(VALID_SETTINGS), Some(models))
            .unwrap()
            .context_window,
        Some(u64::MAX)
    );
}

#[test]
fn context_window_rejects_invalid_values_including_unselected_models() {
    for value in [
        "null",
        "0",
        "-1",
        "1.5",
        "20.0",
        "true",
        "\"secret-marker\"",
        "[]",
        "{}",
        "1e30",
        "18446744073709551616",
    ] {
        let selected = format!(
            r#"{{"providers":{{"fixture": {{"api_key":"fixture-key","models":{{"primary": {{"model":"m","context_window":{value}}}}}}}}}}}"#
        );
        rejection(Some(VALID_SETTINGS), Some(&selected));
        let unselected_model = format!(
            r#"{{"providers":{{"fixture": {{"api_key":"fixture-key","models":{{"primary": {{"model":"m"}},"other": {{"model":"m","context_window":{value}}}}}}}}}}}"#
        );
        rejection(Some(VALID_SETTINGS), Some(&unselected_model));
        let unselected_provider = format!(
            r#"{{"providers":{{"fixture": {{"api_key":"fixture-key","models":{{"primary": {{"model":"m"}}}}}},"other": {{"api_key":"fixture-key","models":{{"primary": {{"model":"m","context_window":{value}}}}}}}}}}}"#
        );
        rejection(Some(VALID_SETTINGS), Some(&unselected_provider));
    }
    // Zero reaches semantic validation, so the fixed diagnostic names it.
    let models = r#"{"providers":{"fixture": {"api_key":"fixture-key",
            "models":{"primary": {"model":"m","context_window":0}}}}}"#;
    let chain = rejection(Some(VALID_SETTINGS), Some(models));
    assert!(
        chain.contains("context_window must be a positive integer"),
        "{chain}"
    );
}

#[test]
fn context_auto_compact_defaults_on_at_80_with_strict_bounds() {
    for settings in [None, Some("{}")] {
        let settings = load(settings, Some(VALID_MODELS)).unwrap().settings;
        assert!(settings.auto_compact);
        assert_eq!(settings.auto_compact_threshold_percent, 80);
    }
    let settings = load(
        Some(r#"{"auto_compact":false,"auto_compact_threshold_percent":42}"#),
        Some(VALID_MODELS),
    )
    .unwrap()
    .settings;
    assert!(!settings.auto_compact);
    assert_eq!(settings.auto_compact_threshold_percent, 42);
    for threshold in [1u8, 99] {
        let settings = json!({"auto_compact_threshold_percent": threshold}).to_string();
        assert_eq!(
            load(Some(&settings), Some(VALID_MODELS))
                .unwrap()
                .settings
                .auto_compact_threshold_percent,
            threshold
        );
    }
    for value in [
        "0",
        "100",
        "255",
        "256",
        "-1",
        "1.5",
        "\"secret-marker\"",
        "null",
        "true",
        "1e2",
        "18446744073709551616",
    ] {
        let settings = format!(r#"{{"auto_compact_threshold_percent":{value}}}"#);
        rejection(Some(&settings), Some(VALID_MODELS));
    }
    for value in ["\"secret-marker\"", "null", "0", "1"] {
        let settings = format!(r#"{{"auto_compact":{value}}}"#);
        rejection(Some(&settings), Some(VALID_MODELS));
    }
    // u8 范围内的无效值返回固定语义校验提示，类型错误由 YAML 解析器统一脱敏。
    let settings = r#"{"auto_compact_threshold_percent":100}"#;
    let chain = rejection(Some(settings), Some(VALID_MODELS));
    assert!(
        chain.contains("auto_compact_threshold_percent must be between 1 and 99"),
        "{chain}"
    );
}

#[test]
fn legacy_settings_preferences_migrate_into_agent_db_and_are_stripped() {
    let (dir, catalog) = catalog(
        Some("# 保留注释\ndefault_model: fixture/primary\nfavorite_models: [fixture/primary]\nthink_level: high\nedit: hashline\n"),
        Some(VALID_MODELS),
    )
    .unwrap();
    assert_eq!(catalog.default_reference(), "fixture/primary");
    assert_eq!(catalog.favorite_models(), ["fixture/primary"]);
    assert_eq!(catalog.default_think_level(), ThinkLevel::High);
    // 旧四字段不再是 settings.yml 落点；注释与无关字段保持原样。
    let text = fs::read_to_string(dir.path().join("settings.yml")).unwrap();
    assert!(text.contains("# 保留注释") && text.contains("edit: hashline"));
    assert!(!text.contains("default_model"));
    assert!(!text.contains("favorite_models"));
    assert!(!text.contains("think_level"));
    // 重启后偏好仍来自 agent.db。
    let restored = ModelCatalog::from_files(
        &dir.path().join("settings.yml"),
        &dir.path().join("models.yml"),
    )
    .unwrap();
    assert_eq!(restored.default_reference(), "fixture/primary");
    assert_eq!(restored.favorite_models(), ["fixture/primary"]);
    assert_eq!(restored.default_think_level(), ThinkLevel::High);
}

#[test]
fn sqlite_concurrent_preferences_preserve_all_writers_and_failures_keep_memory() {
    let models = r#"{"providers":{"fixture": {"api_key":"secret-marker-fixture-key",
            "models":{"primary": {"model":"fixture-model"},"secondary": {"model":"fixture-model-2"}}}}}"#;
    let (dir, mut first) = catalog(None, Some(models)).unwrap();
    let mut second = ModelCatalog::from_files(
        &dir.path().join("settings.yml"),
        &dir.path().join("models.yml"),
    )
    .unwrap();
    // 两个实例分别改不同偏好；每次保存重读当前 DB 值，互不丢失。
    first.save_default_model("fixture/secondary").unwrap();
    assert_eq!(first.default_reference(), "fixture/secondary");
    assert_eq!(second.default_reference(), "fixture/primary");
    second.toggle_favorite_model("fixture/primary").unwrap();
    first.save_default_think_level(ThinkLevel::High).unwrap();
    let reloaded = ModelCatalog::from_files(
        &dir.path().join("settings.yml"),
        &dir.path().join("models.yml"),
    )
    .unwrap();
    assert_eq!(reloaded.default_reference(), "fixture/secondary");
    assert_eq!(reloaded.favorite_models(), ["fixture/primary"]);
    assert_eq!(reloaded.default_think_level(), ThinkLevel::High);
    // agent.db 不可用时保存失败，两个 catalog 的内存快照保持各自已提交状态。
    let agent_db = dir.path().join("agent.db");
    fs::remove_file(&agent_db).unwrap();
    fs::create_dir(&agent_db).unwrap();
    assert!(second.save_default_model("fixture/primary").is_err());
    assert_eq!(second.default_reference(), "fixture/primary");
    assert!(second.toggle_favorite_model("fixture/secondary").is_err());
    assert_eq!(second.favorite_models(), ["fixture/primary"]);
    assert!(first.save_default_think_level(ThinkLevel::Low).is_err());
    assert_eq!(first.default_think_level(), ThinkLevel::High);
    assert_eq!(first.default_reference(), "fixture/secondary");
}
