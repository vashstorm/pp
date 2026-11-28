mod integration {
    use super::super::*;
    use crate::tools::permission::PermissionDecision;
    use crate::tools::permission::PermissionHandler;
    use pp_config::config::PermissionMode;
    use pp_config::config::ToolsSettings;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Install an already enabled Registry that never starts servers on its own: the user-level config is an empty object,
    /// only built-in definitions are merged, and `lazy=true` guarantees a rejected call spawns no process.
    fn tools_with_lsp(dir: &tempfile::TempDir) -> Tools {
        let mut tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let user = dir.path().join("lsp.json");
        fs::write(&user, "{}").unwrap();
        let registry = pp_lsp::Registry::new(
            dir.path().into(),
            Some(user),
            pp_config::config::LspSettings {
                enabled: true,
                lazy: true,
                ..Default::default()
            },
        );
        tools.install_lsp(registry);
        tools
    }

    fn run_lsp(tools: &Tools, arguments: Value, policy: ToolPolicy<'_>) -> Value {
        let execution = tools.execute(
            "lsp",
            &arguments.to_string(),
            "lsp-call",
            policy,
            &CancellationToken::new(),
            &mut |_| {},
        );
        assert!(
            execution.stop_error.is_none(),
            "unexpected cleanup failure: {:?}",
            execution.stop_error
        );
        serde_json::from_str(&execution.output).unwrap()
    }

    fn deny_handler(prompts: &Arc<AtomicUsize>) -> PermissionHandler {
        let prompts = Arc::clone(prompts);
        Box::new(move |_request, _cancellation| {
            prompts.fetch_add(1, Ordering::SeqCst);
            Ok(PermissionDecision::Deny)
        })
    }

    fn rename_arguments() -> Value {
        json!({
            "action":"rename","file":"src/lib.rs","line":1,
            "symbol":"value","new_name":"number","apply":false
        })
    }

    #[test]
    fn lsp_schema_sharing_and_mode_gating() {
        {
            let dir = tempfile::tempdir().unwrap();
            let plain = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
            for mode in [WorkMode::Execute, WorkMode::Plan] {
                assert!(
                    !plain
                        .all_definitions(mode)
                        .iter()
                        .any(|definition| definition.name == "lsp"),
                    "lsp schema must not be exposed without an installed Registry"
                );
            }
            assert!(
                !plain
                    .review_definitions()
                    .iter()
                    .any(|definition| definition.name == "lsp")
            );

            let tools = tools_with_lsp(&dir);
            for mode in [WorkMode::Execute, WorkMode::Plan] {
                assert!(
                    tools
                        .all_definitions(mode)
                        .iter()
                        .any(|definition| definition.name == "lsp"),
                    "{mode:?} should expose lsp"
                );
            }
            assert!(
                tools
                    .review_definitions()
                    .iter()
                    .any(|definition| definition.name == "lsp"),
                "Review should expose the restricted lsp"
            );

            // With enabled=false the install is ignored: no schema and no cleanup right.
            let disabled = pp_lsp::Registry::new(
                dir.path().into(),
                None,
                pp_config::config::LspSettings {
                    enabled: false,
                    ..Default::default()
                },
            );
            let mut disabled_tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
            disabled_tools.install_lsp(disabled);
            assert!(disabled_tools.lsp_registry().is_none());
            assert!(disabled_tools.lsp_cleanup().is_none());
            assert!(
                !disabled_tools
                    .all_definitions(WorkMode::Execute)
                    .iter()
                    .any(|definition| definition.name == "lsp")
            );

            // When not installed or disabled, an explicit call is just an ordinary tool failure and never discovers or starts a server.
            let workflow = WorkflowState::default();
            let result = run_lsp(
                &plain,
                json!({"action":"status"}),
                ToolPolicy::Execute {
                    workflow: &workflow,
                },
            );
            assert_eq!(result["ok"], false, "{result}");
        }

        {
            let dir = tempfile::tempdir().unwrap();
            let owner = tools_with_lsp(&dir);
            assert!(owner.lsp_cleanup().is_some());

            let mut child = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
            child.install_shared_lsp(owner.lsp_registry().unwrap().clone());
            // A subagent sees the tool and shares the same clients but never gets the handle that stops the parent Registry.
            assert!(child.lsp_registry().is_some());
            assert!(child.lsp_cleanup().is_none());
            assert!(
                child
                    .all_definitions(WorkMode::Execute)
                    .iter()
                    .any(|definition| definition.name == "lsp")
            );
        }

        {
            let dir = tempfile::tempdir().unwrap();
            let tools = tools_with_lsp(&dir);
            let cases = vec![
                json!({"action":"rename","file":"src/lib.rs","line":1,"symbol":"a","new_name":"b","apply":true}),
                json!({"action":"reload","file":"src/lib.rs"}),
                json!({"action":"request","query":"textDocument/hover"}),
                json!({"action":"diagnostics","file":"*"}),
                json!({"action":"bogus"}),
                json!({"unknown":"field"}),
                json!("not an object"),
            ];
            for arguments in cases {
                let result = run_lsp(&tools, arguments.clone(), ToolPolicy::Review);
                assert_eq!(result["ok"], false, "{arguments}: {result}");
                assert!(result.get("cancelled").is_none(), "{arguments}: {result}");
                assert!(
                    result
                        .get("error")
                        .and_then(Value::as_str)
                        .is_some_and(|error| !error.is_empty()),
                    "{arguments}: {result}"
                );
            }
            // A Review rejection must happen before any server start or file change.
            assert!(!tools.lsp_registry().unwrap().has_started());
            assert!(!dir.path().join("src/main.rs").exists());
        }

        {
            let dir = tempfile::tempdir().unwrap();
            let tools = tools_with_lsp(&dir);
            let document = plan_document();
            let cases = vec![
                json!({"action":"rename","file":"src/lib.rs","line":1,"symbol":"a","new_name":"b","apply":true}),
                json!({"action":"reload","file":"src/lib.rs"}),
                json!({"action":"request","query":"textDocument/hover"}),
                json!({"action":"diagnostics","file":"*"}),
            ];
            for arguments in cases {
                let result = run_lsp(
                    &tools,
                    arguments.clone(),
                    ToolPolicy::Plan {
                        document: &document,
                        sealed: false,
                        input: None,
                    },
                );
                assert_eq!(result["ok"], false, "{arguments}: {result}");
            }
            assert!(!tools.lsp_registry().unwrap().has_started());
            assert!(!dir.path().join("src/main.rs").exists());
        }
    }

    #[test]
    fn lsp_execute_permission_and_readonly_tiers() {
        {
            let dir = tempfile::tempdir().unwrap();
            let workflow = WorkflowState::default();
            let prompts = Arc::new(AtomicUsize::new(0));
            let mut tools = tools_with_lsp(&dir);
            tools.set_lsp_readonly(true);
            tools.permission.set_config(ToolsSettings::default());
            tools.permission.select(Some(PermissionMode::ReadOnly));
            tools.permission.set_handler(deny_handler(&prompts));

            let result = run_lsp(
                &tools,
                rename_arguments(),
                ToolPolicy::Execute {
                    workflow: &workflow,
                },
            );
            assert_eq!(result["ok"], false, "{result}");
            // Read-only enforcement precedes the permission gate: no permission prompt appears and it is not counted as a "user denial".
            let error = result["error"].as_str().unwrap_or_default();
            assert!(!error.contains("denied by the user"), "{result}");
            assert_eq!(prompts.load(Ordering::SeqCst), 0);
            assert!(!tools.lsp_registry().unwrap().has_started());
        }

        {
            let dir = tempfile::tempdir().unwrap();
            let workflow = WorkflowState::default();
            let prompts = Arc::new(AtomicUsize::new(0));
            let mut tools = tools_with_lsp(&dir);
            tools.permission.set_config(ToolsSettings::default());
            tools.permission.select(Some(PermissionMode::ReadOnly));
            tools.permission.set_handler(deny_handler(&prompts));

            // reload is write tier: under always-ask it must go through a permission prompt, and the prompt happens before dispatch.
            let result = run_lsp(
                &tools,
                json!({"action":"reload"}),
                ToolPolicy::Execute {
                    workflow: &workflow,
                },
            );
            assert_eq!(result["ok"], false, "{result}");
            assert!(
                result["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("lsp was denied by the user."),
                "{result}"
            );
            assert_eq!(prompts.load(Ordering::SeqCst), 1);
            assert!(!tools.lsp_registry().unwrap().has_started());
        }

        {
            let dir = tempfile::tempdir().unwrap();
            let workflow = WorkflowState::default();
            let prompts = Arc::new(AtomicUsize::new(0));
            let mut tools = tools_with_lsp(&dir);
            tools.permission.set_config(ToolsSettings::default());
            tools.permission.select(Some(PermissionMode::ReadOnly));
            tools.permission.set_handler(deny_handler(&prompts));

            // An extension without a matching server: the action is read tier, approval is skipped and the tool returns a no-server result.
            let result = run_lsp(
                &tools,
                json!({"action":"hover","file":"notes.zzz","line":1}),
                ToolPolicy::Execute {
                    workflow: &workflow,
                },
            );
            assert_eq!(prompts.load(Ordering::SeqCst), 0, "{result}");
            assert_eq!(result["ok"], false, "{result}");
            assert!(!tools.lsp_registry().unwrap().has_started());
        }
    }
}
