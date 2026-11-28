use super::*;
use serde_json::json;

#[test]
fn settings_output_rg_read_and_eval_limits() {
    {
        let defaults = ToolLimits::default();
        let mut old = serde_json::to_value(defaults).unwrap();
        old.as_object_mut().unwrap().remove("output");
        let restored: ToolLimits = serde_json::from_value(old).unwrap();
        assert_eq!(restored.output.artifact_spill_threshold, 51200);
        assert_eq!(restored.output.artifact_max_bytes, 16777216);
        let settings = load(Some(r#"{"tools":{"artifactSpillThreshold":128,"artifactHeadBytes":100,"outputMaxColumns":0,"artifactMaxBytes":0,"artifactHeadByte":10}}"#),Some(VALID_MODELS)).unwrap().settings;
        assert_eq!(settings.tool_limits().output.artifact_spill_threshold, 128);
        assert_eq!(settings.tool_limits().output.artifact_head_bytes, 100);
        assert_eq!(settings.tool_limits().output.output_max_columns, 0);
        for bad in [
            r#"{"tools":{"artifactSpillThreshold":0}}"#,
            r#"{"tools":{"artifactMaxBytes":-1}}"#,
            r#"{"tools":{"artifactMaxBytes":1.5}}"#,
            r#"{"tools":{"artifactHeadBytes":null}}"#,
        ] {
            assert!(load(Some(bad), Some(VALID_MODELS)).is_err(), "{bad}");
        }
    }

    {
        let settings: Settings = yaml::parse("{}", "settings").unwrap();
        assert_eq!(
            (settings.rg.context_before, settings.rg.context_after),
            (1, 3)
        );
        let settings: Settings = yaml::parse(
            "grep: {contextBefore: 99}\nrg: {contextBefore: 0, contextAfter: 5, extra: 1}",
            "settings",
        )
        .unwrap();
        assert_eq!(
            (
                settings.tool_limits().rg.context_before,
                settings.tool_limits().rg.context_after
            ),
            (0, 5)
        );
        for input in [
            "rg: null",
            "rg: {contextBefore: -1}",
            "rg: {contextAfter: null}",
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
        assert_eq!((limits.rg.context_before, limits.rg.context_after), (1, 3));

        // checkpoint 契约：`ToolLimits` 的序列化键跟随字段名，必须是 `rg`。
        let value = serde_json::to_value(ToolLimits::default()).unwrap();
        assert!(value.get("rg").is_some(), "{value}");
        assert!(value.get("grep").is_none(), "{value}");
        let restored: ToolLimits = serde_json::from_value(json!({
            "read_max_bytes": 10,
            "read_max_lines": 10,
            "bash_timeout_seconds": 10,
            "rg": {"contextBefore": 2, "contextAfter": 6}
        }))
        .unwrap();
        assert_eq!(
            (restored.rg.context_before, restored.rg.context_after),
            (2, 6)
        );
    }

    {
        for text in ["{}", "read: {}", "read: {toolResultPreview: false}"] {
            let settings: Settings = yaml::parse(text, "settings").unwrap();
            assert!(!settings.read.tool_result_preview);
        }
        let settings: Settings =
            yaml::parse("read: {toolResultPreview: true, extra: [1]}", "settings").unwrap();
        assert!(settings.read.tool_result_preview);
        for text in [
            "read: null",
            "read: true",
            "read: {toolResultPreview: null}",
            "read: {toolResultPreview: 1}",
            "read: {toolResultPreview: 'true'}",
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

    {
        let defaults: Settings = yaml::parse("{}", "settings").unwrap();
        assert!(defaults.eval.enabled);
        assert_eq!(defaults.eval.interpreter, None);
        assert_eq!(defaults.eval.js_runtime, None);
        assert_eq!(defaults.eval.timeout_seconds, 30);
        assert_eq!(defaults.eval.max_timeout_seconds, 600);

        let custom = load(
            Some("eval:\n  enabled: false\n  interpreter: /usr/bin/python3\n  js_runtime: node\n  timeout_seconds: 15\n  max_timeout_seconds: 120\n  extra: 1\ntools:\n  permission:\n    eval: prompt\n"),
            Some(VALID_MODELS),
        )
        .unwrap();
        assert!(!custom.settings.eval.enabled);
        assert_eq!(
            custom.settings.eval.interpreter.as_deref(),
            Some("/usr/bin/python3")
        );
        assert_eq!(custom.settings.eval.js_runtime.as_deref(), Some("node"));
        assert_eq!(custom.settings.tool_limits().eval_timeout_seconds, 15);
        assert_eq!(custom.settings.tool_limits().eval_max_timeout_seconds, 120);
        assert_eq!(
            custom.settings.tools.permission.get("eval"),
            Some(&PermissionPolicy::Prompt)
        );

        for bad in [
            "eval: {interpreter: '   '}",
            "eval: {js_runtime: '   '}",
            "eval: {timeout_seconds: 0}",
            "eval: {max_timeout_seconds: 0}",
            "eval: {timeout_seconds: 120, max_timeout_seconds: 60}",
        ] {
            assert!(load(Some(bad), Some(VALID_MODELS)).is_err(), "{bad}");
        }

        let legacy: ToolLimits = serde_json::from_value(json!({
            "read_max_bytes": 1024,
            "read_max_lines": 100,
            "bash_timeout_seconds": 60
        }))
        .unwrap();
        assert_eq!(legacy.eval_timeout_seconds, 30);
        assert_eq!(legacy.eval_max_timeout_seconds, 600);
    }
}

#[test]
fn settings_legacy_preferences_migration_and_sqlite_concurrency() {
    {
        for settings in [None, Some("{}")] {
            let (_dir, catalog) = catalog(settings, Some(VALID_MODELS)).unwrap();
            assert!(catalog.favorite_models().is_empty());
        }
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
        rejection(Some("favorite_models: secret-marker"), Some(VALID_MODELS));
    }

    {
        let (dir, mut catalog) = catalog(None, Some(VALID_MODELS)).unwrap();
        let settings_path = dir.path().join("settings.yml");
        let models_path = dir.path().join("models.yml");
        fs::write(&settings_path, "# 保留注释\nedit: hashline\n").unwrap();
        assert_eq!(
            catalog.toggle_favorite_model("fixture/primary").unwrap(),
            ["fixture/primary"]
        );
        let saved = fs::read_to_string(&settings_path).unwrap();
        assert!(saved.contains("# 保留注释"));
        assert!(saved.contains("edit: hashline"));
        assert!(!saved.contains("favorite_models"));
        let restored = ModelCatalog::from_files(&settings_path, &models_path).unwrap();
        assert_eq!(restored.favorite_models(), ["fixture/primary"]);
        assert!(catalog.toggle_favorite_model("unknown/model").is_err());
        assert_eq!(catalog.favorite_models(), ["fixture/primary"]);
        assert_eq!(fs::read_to_string(&models_path).unwrap(), VALID_MODELS);
        let agent_db = dir.path().join("agent.db");
        fs::remove_file(&agent_db).unwrap();
        fs::create_dir(&agent_db).unwrap();
        assert!(catalog.toggle_favorite_model("fixture/primary").is_err());
        assert_eq!(catalog.favorite_models(), ["fixture/primary"]);
    }

    {
        let (dir, catalog) = catalog(
            Some("# 保留注释\ndefault_model: fixture/primary\nfavorite_models: [fixture/primary]\nthink_level: high\nedit: hashline\n"),
            Some(VALID_MODELS),
        )
        .unwrap();
        assert_eq!(catalog.default_reference(), "fixture/primary");
        assert_eq!(catalog.favorite_models(), ["fixture/primary"]);
        assert_eq!(catalog.default_think_level(), ThinkLevel::High);
        let text = fs::read_to_string(dir.path().join("settings.yml")).unwrap();
        assert!(text.contains("# 保留注释") && text.contains("edit: hashline"));
        assert!(!text.contains("default_model"));
        assert!(!text.contains("favorite_models"));
        assert!(!text.contains("think_level"));
        let restored = ModelCatalog::from_files(
            &dir.path().join("settings.yml"),
            &dir.path().join("models.yml"),
        )
        .unwrap();
        assert_eq!(restored.default_reference(), "fixture/primary");
        assert_eq!(restored.favorite_models(), ["fixture/primary"]);
        assert_eq!(restored.default_think_level(), ThinkLevel::High);
    }

    {
        let models = r#"{"providers":{"fixture": {"api_key":"secret-marker-fixture-key",
                "models":{"primary": {"model":"fixture-model"},"secondary": {"model":"fixture-model-2"}}}}}"#;
        let (dir, mut first) = catalog(None, Some(models)).unwrap();
        let mut second = ModelCatalog::from_files(
            &dir.path().join("settings.yml"),
            &dir.path().join("models.yml"),
        )
        .unwrap();
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
}

#[test]
fn settings_timeouts_hooks_skills_and_validation_bounds() {
    {
        let settings = load(
            Some(r#"{"notification_show_countdown":false,"notification_timeout_seconds":3}"#),
            Some(VALID_MODELS),
        )
        .unwrap()
        .settings;
        assert!(!settings.notification_show_countdown);
        assert_eq!(settings.notification_timeout_seconds, 3);
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

    {
        assert_eq!(
            load(None, Some(VALID_MODELS))
                .unwrap()
                .settings
                .bash_timeout_seconds,
            60
        );
        let settings = load(
            Some(r#"{"read_max_bytes":17,"read_max_lines":3,"bash_timeout_seconds":2}"#),
            Some(VALID_MODELS),
        )
        .unwrap()
        .settings;
        assert_eq!(settings.read_max_bytes, 17);
        assert_eq!(settings.read_max_lines, 3);
        assert_eq!(settings.bash_timeout_seconds, 2);
        for field in [
            "read_max_bytes",
            "read_max_lines",
            "bash_timeout_seconds",
            "bash_max_timeout_seconds",
        ] {
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

    {
        let defaults = load(None, Some(VALID_MODELS))
            .unwrap()
            .settings
            .tool_limits();
        assert_eq!(defaults.bash_timeout_seconds, 60);
        assert_eq!(defaults.bash_max_timeout_seconds, 600);
        let settings = load(
            Some("bash_timeout_seconds: 1200\nbash_max_timeout_seconds: 7200"),
            Some(VALID_MODELS),
        )
        .unwrap()
        .settings;
        let limits = settings.tool_limits();
        assert_eq!(limits.bash_timeout_seconds, 1200);
        assert_eq!(limits.bash_max_timeout_seconds, 7200);
        for input in [
            "bash_timeout_seconds: 601",
            "bash_max_timeout_seconds: 59",
            "bash_max_timeout_seconds: 18446744073709551615",
            "bash_timeout_seconds: 1\nbash_max_timeout_seconds: 18446744073709551615",
        ] {
            rejection(Some(input), Some(VALID_MODELS));
        }
        for (timeout, maximum) in [(300, 600), (1200, 1200)] {
            let legacy: ToolLimits = serde_json::from_value(json!({
                "read_max_bytes": 65536,
                "read_max_lines": 2000,
                "bash_timeout_seconds": timeout
            }))
            .unwrap();
            legacy.validate().unwrap();
            assert_eq!(legacy.bash_timeout_seconds, timeout);
            assert_eq!(legacy.bash_max_timeout_seconds, maximum);
        }
        let invalid = json!({
            "read_max_bytes": 65536, "read_max_lines": 2000,
            "bash_timeout_seconds": 1200, "bash_max_timeout_seconds": 600
        });
        let limits: ToolLimits = serde_json::from_value(invalid.clone()).unwrap();
        assert!(limits.validate().is_err());
        let mut invalid = invalid;
        invalid["bash_max_timeout_seconds"] = json!(null);
        assert!(serde_json::from_value::<ToolLimits>(invalid).is_err());
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("hooks.yml");
        let project = dir.path().join(".pp/hooks.yml");
        let hooks = load_hooks(&user, &project).unwrap();
        assert!(hooks.is_empty());
        assert!(!project.exists());
        fs::create_dir_all(project.parent().unwrap()).unwrap();
        fs::write(
            &user,
            "PreToolUse:\n  - matcher: read\n    hooks:\n      - type: command\n        command: /bin/user\n",
        )
        .unwrap();
        fs::write(
            &project,
            "PreToolUse:\n  - matcher: write\n    hooks:\n      - type: command\n        command: /bin/project\nStop:\n  - hooks:\n      - type: command\n        command: /bin/stop\n",
        )
        .unwrap();
        let hooks = load_hooks(&user, &project).unwrap();
        let commands: Vec<&str> = hooks
            .pre_tool_use
            .iter()
            .flat_map(|group| group.hooks.iter().map(|hook| hook.command.as_str()))
            .collect();
        assert_eq!(commands, ["/bin/user", "/bin/project"]);
        assert_eq!(hooks.stop.len(), 1);
        fs::write(
            &project,
            "PreToolUse:\n  - hooks:\n      - type: prompt\n        command: secret-marker\n",
        )
        .unwrap();
        let message = format!("{:#}", load_hooks(&user, &project).unwrap_err());
        assert!(message.contains(".pp/hooks.yml"), "{message}");
        assert!(!message.contains("secret-marker"), "{message}");
        for blank in ["", "\n\n", "# hooks disabled\n"] {
            fs::write(&project, blank).unwrap();
            let hooks = load_hooks(&user, &project).unwrap();
            assert_eq!(hooks.pre_tool_use.len(), 1);
            assert!(hooks.stop.is_empty());
        }
    }

    {
        rejection(Some(VALID_SETTINGS), None);
        for settings in ["{", "{\"default_model\" \"secret-marker\"}", ""] {
            rejection(Some(settings), Some(VALID_MODELS));
        }
        for models in ["{", "{\"providers\": \"secret-marker\"", ""] {
            let chain = rejection(Some(VALID_SETTINGS), Some(models));
            assert!(chain.contains("invalid YAML models config"), "{chain}");
        }
    }

    {
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
}

#[test]
fn settings_context_window_auto_compact_tui_and_budgets() {
    {
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

    {
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
        let models = r#"{"providers":{"fixture": {"api_key":"fixture-key",
                "models":{"primary": {"model":"m","context_window":0}}}}}"#;
        let chain = rejection(Some(VALID_SETTINGS), Some(models));
        assert!(
            chain.contains("context_window must be a positive integer"),
            "{chain}"
        );
    }

    {
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
        let settings = r#"{"auto_compact_threshold_percent":100}"#;
        let chain = rejection(Some(settings), Some(VALID_MODELS));
        assert!(
            chain.contains("auto_compact_threshold_percent must be between 1 and 99"),
            "{chain}"
        );
    }

    {
        let settings: Settings = yaml::parse("{}", "settings").unwrap();
        assert_eq!(settings.tui.max_fps, 60);
        for fps in [1, 30, 60, 120] {
            let settings: Settings =
                yaml::parse(&format!("tui: {{max_fps: {fps}}}"), "settings").unwrap();
            settings.validate().unwrap();
        }
        for text in ["tui: {max_fps: 0}", "tui: {max_fps: 121}"] {
            let settings: Settings = yaml::parse(text, "settings").unwrap();
            assert!(settings.validate().is_err());
        }
        for text in ["tui: null", "tui: {max_fps: 1.5}", "tui: {max_fps: '60'}"] {
            assert!(yaml::parse::<Settings>(text, "settings").is_err());
        }
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.yml");
        assert_eq!(read_settings(&path).unwrap().task.max_concurrency, 4);
        for text in ["{}", "task: {}", "task: {max_concurreny: 1}"] {
            fs::write(&path, text).unwrap();
            assert_eq!(
                read_settings(&path).unwrap().task.max_concurrency,
                4,
                "{text}"
            );
        }
        for value in [1, 2, 32] {
            fs::write(&path, format!("task: {{max_concurrency: {value}}}")).unwrap();
            assert_eq!(read_settings(&path).unwrap().task.max_concurrency, value);
        }
        for text in [
            "task: {max_concurrency: 0}",
            "task: {max_concurrency: 33}",
            "task: {max_concurrency: -1}",
            "task: {max_concurrency: 1.5}",
            "task: {max_concurrency: '4'}",
            "task: {max_concurrency: null}",
        ] {
            fs::write(&path, text).unwrap();
            assert!(read_settings(&path).is_err(), "{text}");
        }
    }

    {
        let default = load(None, Some(VALID_MODELS)).unwrap();
        assert_eq!(default.settings.task.token_budget, None);
        assert_eq!(default.settings.task.cost_budget_usd, None);

        let configured = load(
            Some("task:\n  token_budget: 50000\n  cost_budget_usd: 1.25\n"),
            Some(VALID_MODELS),
        )
        .unwrap();
        assert_eq!(configured.settings.task.token_budget, Some(50_000));
        assert_eq!(configured.settings.task.cost_budget_usd, Some(1.25));

        for bad in [
            "task:\n  token_budget: 0\n",
            "task:\n  cost_budget_usd: 0\n",
            "task:\n  cost_budget_usd: -0.5\n",
            "task:\n  cost_budget_usd: .nan\n",
            "task:\n  cost_budget_usd: .inf\n",
        ] {
            assert!(load(Some(bad), Some(VALID_MODELS)).is_err(), "{bad}");
        }
    }

    {
        let default = load(None, Some(VALID_MODELS)).unwrap();
        assert_eq!(
            default.settings.tools.sandbox.read_scope,
            SandboxReadScope::Workspace
        );

        for (raw, expected) in [
            (
                "tools:\n  sandbox:\n    readScope: workspace\n",
                SandboxReadScope::Workspace,
            ),
            (
                "tools:\n  sandbox:\n    readScope: unrestricted\n",
                SandboxReadScope::Unrestricted,
            ),
        ] {
            let loaded = load(Some(raw), Some(VALID_MODELS)).unwrap();
            assert_eq!(loaded.settings.tools.sandbox.read_scope, expected, "{raw}");
        }

        assert!(
            load(
                Some("tools:\n  sandbox:\n    readScope: all\n"),
                Some(VALID_MODELS)
            )
            .is_err()
        );
    }
}

#[test]
fn settings_model_selection_ranking_favorites_and_efforts() {
    {
        let models = r#"{"providers":{"tier":{"api_key":"secret-marker-tier-key","models":{
                "o-n-e": {"model":"tier-a"},
                "team-one": {"model":"tier-b"},
                "one-prefix": {"model":"tier-c"},
                "one": {"model":"tier-d"},
                "p-r-e-f": {"model":"tier-e"},
                "team-pref": {"model":"tier-f"},
                "pref-model": {"model":"tier-g"},
                "s-u-b": {"model":"tier-h"},
                "team-sub": {"model":"tier-i"},
                "s-q": {"model":"tier-z"},
                "internal": {"model":"wire-only"}}}}}"#;
        let (_dir, catalog) = catalog(None, Some(models)).unwrap();
        assert_eq!(
            catalog.select("one").unwrap(),
            ModelSelection {
                reference: "tier/one".into(),
                effort: ThinkLevel::None,
            }
        );
        assert_eq!(catalog.select("pref").unwrap().reference, "tier/pref-model");
        assert_eq!(catalog.select("sub").unwrap().reference, "tier/team-sub");
        assert_eq!(catalog.select("sq").unwrap().reference, "tier/s-q");
        assert_eq!(
            catalog.select("wire-only").unwrap().reference,
            "tier/internal"
        );
    }

    {
        let models = r#"{"providers":{
                "first": {"api_key":"secret-marker-first-key","models":{
                    "flash": {"model":"first-flash"},
                    "solo": {"model":"first-solo"}}},
                "second": {"api_key":"secret-marker-second-key","models":{
                    "flash": {"model":"second-flash"},
                    "flash-lite": {"model":"second-flash-lite"},
                    "team-solo": {"model":"second-team-solo"}}},
                "third": {"api_key":"secret-marker-third-key","models":{
                    "unique": {"model":"third-unique"}}}}}"#;
        let (dir, catalog) = seeded_catalog(None, Some(models), |store| {
            for reference in ["second/flash", "second/team-solo"] {
                store.toggle_favorite(reference).unwrap();
            }
        })
        .unwrap();
        assert_eq!(
            catalog.favorite_models(),
            ["second/flash", "second/team-solo"]
        );
        let agent_db = fs::read(dir.path().join("agent.db")).unwrap();
        assert_eq!(catalog.select("flash").unwrap().reference, "second/flash");
        assert_eq!(catalog.select("solo").unwrap().reference, "first/solo");
        assert_eq!(catalog.select("unique").unwrap().reference, "third/unique");
        assert_eq!(fs::read(dir.path().join("agent.db")).unwrap(), agent_db);
        assert_eq!(
            catalog.favorite_models(),
            ["second/flash", "second/team-solo"]
        );
        assert_eq!(catalog.default_think_level(), ThinkLevel::None);
        assert_eq!(catalog.default_reference(), "first/flash");
    }

    {
        let models = r#"{"providers":{"fixture":{"api_key":"secret-marker-fixture-key","models":{
                "primary": {"model":"fixture-model","thinking":{"efforts":["low","high"],"defaultLevel":"low"}}}}}}"#;
        let (_dir, catalog) = catalog(None, Some(models)).unwrap();
        assert_eq!(
            catalog.select("fixture/primary:high").unwrap(),
            ModelSelection {
                reference: "fixture/primary".into(),
                effort: ThinkLevel::High,
            }
        );
        assert_eq!(
            catalog.select(" FIXTURE/PRIMARY:HIGH ").unwrap(),
            ModelSelection {
                reference: "fixture/primary".into(),
                effort: ThinkLevel::High,
            }
        );
        let error = catalog.select("primary:default").unwrap_err();
        assert!(
            format!("{error:#}").starts_with("no configured model matches"),
            "{error:#}"
        );
        let error = catalog.select("fixture/primary:medium").unwrap_err();
        let message = format!("{error:#}");
        assert_eq!(
            message,
            "model fixture/primary does not support effort medium; options: low, high"
        );
        assert!(!message.contains("secret-marker"), "{message}");
        assert!(!message.contains("fixture-model"), "{message}");
    }

    {
        let models = r#"{"providers":{"levels":{"api_key":"secret-marker-levels-key","models":{
                "explicit": {"model":"levels-a","thinking":{"efforts":["low","high"],"defaultLevel":"high"}},
                "inherit": {"model":"levels-b","thinking":{}},
                "narrow": {"model":"levels-c","thinking":{"efforts":["high","max"]}}}}}}"#;
        let (_dir, catalog) = seeded_catalog(None, Some(models), |store| {
            store
                .set_default_effort(ThinkLevel::Low, |_, _| Ok(()))
                .unwrap();
        })
        .unwrap();
        assert_eq!(catalog.default_think_level(), ThinkLevel::Low);
        assert_eq!(catalog.select("explicit").unwrap().effort, ThinkLevel::High);
        assert_eq!(catalog.select("inherit").unwrap().effort, ThinkLevel::Low);
        assert_eq!(catalog.select("narrow").unwrap().effort, ThinkLevel::High);
    }

    {
        let models = r#"{"providers":{"gateway":{"api_key":"secret-marker-gateway-key","models":{
                "team/one": {"model":"gateway-wire"}}}}}"#;
        let (_dir, catalog) = catalog(None, Some(models)).unwrap();
        let short = catalog.select("one").unwrap();
        assert_eq!(short.reference, "gateway/team/one");
        assert_eq!(catalog.select("gateway/team/one").unwrap(), short);
        assert_eq!(catalog.select("team/one").unwrap(), short);
    }

    {
        let (_dir, catalog) = catalog(None, Some(VALID_MODELS)).unwrap();
        for query in ["", "   "] {
            let error = catalog.select(query).unwrap_err();
            assert_eq!(format!("{error:#}"), "model must not be empty", "{query:?}");
        }
        let error = catalog.select("fixture/").unwrap_err();
        assert_eq!(format!("{error:#}"), "model must not be empty");
        for (query, echoed) in [
            ("nope", "nope"),
            ("  nope  ", "  nope  "),
            ("fixture/nope", "fixture/nope"),
        ] {
            let error = catalog.select(query).unwrap_err();
            let message = format!("{error:#}");
            assert_eq!(
                message,
                format!(
                    "no configured model matches \"{echoed}\"; use /model to list configured models"
                )
            );
            assert!(!message.contains("secret-marker"), "{message}");
            assert!(!message.contains("fixture-model"), "{message}");
        }
    }
}

#[test]
fn settings_pricing_and_cost_display_validation() {
    {
        let (_dir, catalog) = catalog(
            None,
            Some(
                r#"
providers:
  first:
    api_key: fixture
    models:
      alias:
        model: gateway/OpenAI/GPT-6.1-SOL
      free:
        model: gpt-6.1-sol
        cost: {input: 0, cacheRead: 0, output: 0}
      gpt-6.1-sol:
        model: gateway/gpt-6.1-sol-custom
  second:
    api_key: fixture
    models:
      free:
        model: gpt-6.1-sol
"#,
            ),
        )
        .unwrap();
        let builtin = catalog.cost("first/alias").unwrap();
        assert_eq!(builtin.input, 2.0);
        assert_eq!(builtin.cache_write, Some(2.5));
        let free = catalog.cost("first/free").unwrap();
        assert_eq!(free.input, 0.0);
        assert_eq!(free.output, 0.0);
        assert_eq!(free.cache_write, None);
        assert_eq!(catalog.cost("second/free").unwrap().input, builtin.input);
        assert!(catalog.cost("first/gpt-6.1-sol").is_none());
    }

    {
        for text in [None, Some("{}")] {
            let settings = load(text, Some(VALID_MODELS)).unwrap().settings;
            assert_eq!(settings.cost.currency, "USD");
            assert_eq!(settings.cost.exchange_rate, 1.0);
        }
        for currency in ["USD", "EUR", "JPY", "KRW", "CNY", "BRL"] {
            let text = format!("cost: {{currency: {currency}, exchange_rate: 7.2, icon: f157}}");
            let settings = load(Some(&text), Some(VALID_MODELS)).unwrap().settings;
            assert_eq!(settings.cost.currency, currency);
            assert_eq!(settings.cost.exchange_rate, 7.2);
        }
        let Err(error) = load(
            Some("cost: {currency: GBP, exchange_rate: 1}"),
            Some(VALID_MODELS),
        ) else {
            panic!("unsupported currency must be rejected");
        };
        assert!(
            report(&error).contains("cost.currency must be one of USD, EUR, JPY, KRW, CNY, BRL"),
            "{}",
            report(&error)
        );
        for invalid in [
            "currency: cny, exchange_rate: 1",
            "currency: CNY, exchange_rate: 0",
            "currency: CNY, exchange_rate: .inf",
            "currency: CNY",
        ] {
            let settings = format!("cost: {{{invalid}}}");
            assert!(
                load(Some(&settings), Some(VALID_MODELS)).is_err(),
                "{settings}"
            );
        }
        let settings = load(
            Some("tui: {cost: {currency: CNY, exchange_rate: 7.2}}"),
            Some(VALID_MODELS),
        )
        .unwrap()
        .settings;
        assert_eq!(settings.cost, CostDisplay::default());
    }
}
