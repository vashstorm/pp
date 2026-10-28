use super::*;

#[test]
fn interactive_config_errors_keep_session_available_without_model_requests() {
    let server = server();
    let valid = json!({
        "providers": {"fixture": {
            "protocol": "completion",
            "api_key": "secret-marker",
            "base_url": format!("http://{}/v1", server.server_addr()),
            "models": {"primary": { "model": "fixture-model"}}
        }}
    });
    let valid_models = valid.to_string();
    let mut cases = vec![
        (Some(fixture_settings().to_owned()), None, "models.yml"),
        (
            Some("{ broken secret-marker settings".to_owned()),
            Some(valid_models.clone()),
            "settings.yml",
        ),
        (
            Some(fixture_settings().to_owned()),
            Some(r#"{"providers":["secret-marker""#.to_owned()),
            "models.yml",
        ),
    ];
    for settings in [
        json!({"default_model": "fixture/primary", "unknown": "secret-marker"}),
        json!({"default_model": "fixture/primary", "request_timeout_seconds": 0}),
        json!({"default_model": ["secret-marker"]}),
        json!({"default_model": "secret-marker"}),
        json!({"default_model": "   "}),
        json!({"default_model": "/secret-marker"}),
        json!({"default_model": "secret-marker/ "}),
    ] {
        cases.push((
            Some(settings.to_string()),
            Some(valid_models.clone()),
            "settings.yml",
        ));
    }
    let mut invalid_models = vec![
        json!({"providers": {}}),
        // The old flat format is deliberately rejected, not a valid fallback.
        json!({"models": {"fixture": { "api_key": "secret-marker", "model": "m"}}}),
    ];
    for (field, value) in [
        ("id", json!(["secret-marker"])),
        ("id", json!(" ")),
        ("id", json!("secret-marker/provider")),
        ("api_key", json!(" ")),
        ("protocol", json!("secret-marker")),
        ("protocol", json!("openai")),
        ("protocol", json!("Completion")),
        ("protocol", json!(" response ")),
        ("protocol", Value::Null),
        ("base_url", Value::Null),
        ("base_url", json!("secret-marker")),
        ("models", json!([])),
    ] {
        let mut invalid = valid.clone();
        invalid["providers"]["fixture"][field] = value;
        invalid_models.push(invalid);
    }
    for (field, value) in [
        ("id", json!(" ")),
        ("model", json!(" ")),
        ("api_key", json!("secret-marker")),
        ("base_url", json!("http://secret-marker.invalid")),
        ("protocol", json!("completion")),
    ] {
        let mut invalid = valid.clone();
        invalid["providers"]["fixture"]["models"]["primary"][field] = value;
        invalid_models.push(invalid);
    }
    for invalid_provider in [
        json!({
                "api_key": "secret-marker",
            "base_url": "secret-marker",
            "models": {"primary": { "model": "unused-model"}}
        }),
        json!({
                "api_key": "secret-marker",
            "models": {}
        }),
        json!({
                "protocol": "anthropic",
            "api_key": "secret-marker",
            "models": {"primary": { "model": " "}}
        }),
    ] {
        let mut invalid = valid.clone();
        invalid["providers"]["unused"] = invalid_provider;
        invalid_models.push(invalid);
    }
    let mut invalid_unselected_model = valid.clone();
    invalid_unselected_model["providers"]["fixture"]["models"]["unused"] = json!({"model": " "});
    invalid_models.push(invalid_unselected_model);
    for models in invalid_models {
        cases.push((
            Some(fixture_settings().to_owned()),
            Some(models.to_string()),
            "models.yml",
        ));
    }
    // A non-blank selected key reaches the existing client header boundary.
    let mut invalid_header = valid.clone();
    invalid_header["providers"]["fixture"]["api_key"] = json!("secret-marker\n");
    cases.push((
        Some(fixture_settings().to_owned()),
        Some(invalid_header.to_string()),
        "invalid API key header",
    ));
    for (settings, models, failing) in cases {
        let dir = tempfile::tempdir().unwrap();
        if let Some(settings) = settings {
            std::fs::write(config_path(dir.path(), "settings.yml"), settings).unwrap();
        }
        if let Some(models) = models {
            std::fs::write(config_path(dir.path(), "models.yml"), models).unwrap();
        }
        let mut cli = Cli::start_with_existing_files(dir.path(), None);
        cli.input("\n/new\ncreate must-not-exist file\n/quit\n");
        let output = cli.finish();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{failing}: {error}");
        assert!(output.stdout.is_empty());
        assert!(error.contains(failing), "{error}");
        assert!(!error.contains("secret-marker"), "{error}");
        assert!(!dir.path().join("must-not-exist").exists());
    }
    no_more(&server);
}

#[test]
fn interactive_unreadable_configs_do_not_prevent_quit() {
    for failing in ["settings.yml", "models.yml"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(config_path(dir.path(), "settings.yml"), fixture_settings()).unwrap();
        if failing == "settings.yml" {
            std::fs::remove_file(config_path(dir.path(), failing)).unwrap();
            std::fs::create_dir(config_path(dir.path(), failing)).unwrap();
        } else {
            std::fs::write(config_path(dir.path(), failing), [0xff, 0xfe]).unwrap();
        }
        let mut cli = Cli::start_with_existing_files(dir.path(), None);
        cli.input("/quit\n");
        let output = cli.finish();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{error}");
        assert!(output.stdout.is_empty());
        assert!(error.contains(failing), "{error}");
    }
}

#[test]
fn missing_or_malformed_config_fails_without_environment_fallback_or_secret_leak() {
    let valid_settings = fixture_settings();
    let valid_models = "{\n\"providers\":[{\"id\":\"fixture\",\"protocol\":\"completion\",\"api_key\":\"secret-marker\",\"models\":[{\"id\":\"primary\",\"model\":\"fixture-model\"}]}],\n}\n";
    let cases = [
        (Some(valid_settings), None, "models.yml"),
        (
            Some("{ broken secret-marker settings"),
            Some(valid_models),
            "settings.yml",
        ),
        (
            Some(valid_settings),
            Some(
                "{\"providers\": [{\"id\": [\"secret-marker\"], \"api_key\": \"secret-marker\", \"models\": [{\"id\":\"primary\", \"model\":\"m\"}]}]}",
            ),
            "models.yml",
        ),
    ];
    for (settings, models, failing) in cases {
        let dir = tempfile::tempdir().unwrap();
        // A valid legacy single-file config must not rescue startup.
        std::fs::write(
            dir.path().join("pp.jsonc"),
            "{ \"api_key\": \"legacy-key\", \"model\": \"legacy-model\" }",
        )
        .unwrap();
        if let Some(settings) = settings {
            std::fs::write(config_path(dir.path(), "settings.yml"), settings).unwrap();
        }
        if let Some(models) = models {
            std::fs::write(config_path(dir.path(), "models.yml"), models).unwrap();
        }
        let cli = Cli::spawn(
            dir.path(),
            &["-p", "hello"],
            &[
                ("OPENAI_API_KEY", "secret-marker"),
                ("OPENAI_MODEL", "environment-model"),
            ],
        );
        let output = cli.finish();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains(failing), "{error}");
        assert!(!error.contains("secret-marker"), "{error}");
        assert!(!error.contains("legacy-key"), "{error}");
    }
}
