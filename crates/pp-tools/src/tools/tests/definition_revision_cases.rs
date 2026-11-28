//! `Tools::definitions_revision`: the cross-crate key of the tool definition inputs.
//!
//! The table proves that every documented mutation entry point moves the key (a stale key would keep the
//! agent on an outdated tool table), that a read-only query never does, and that a key change is not
//! reported for inputs the tool table ignores.
use super::*;
use pp_config::config::SandboxSettings;
use pp_sandbox::{ProbeResult, Sandbox};
use std::sync::Arc;

/// Subagent port double: only the definition list takes part in the key.
struct StubHost;

impl SubagentHost for StubHost {
    fn definitions(&self, _: WorkMode) -> Vec<FunctionObject> {
        vec![FunctionObject {
            name: "task".into(),
            description: Some("task".into()),
            parameters: Some(json!({"type": "object"})),
            strict: None,
        }]
    }
    fn pending(&self) -> bool {
        false
    }
    fn plan_mode(&self) -> bool {
        false
    }
    fn is_root(&self) -> bool {
        true
    }
    fn bind_session(&self, _: &Path, _: &str) {}
    fn execute_task(&self, _: &str, _: bool, _: &CancellationToken) -> Result<Value> {
        unreachable!("the key test never dispatches")
    }
    fn uri(&self, _: &str, _: &str, _: Option<&str>, _: &CancellationToken) -> Result<Value> {
        unreachable!("the key test never dispatches")
    }
    fn read_agent_result(&self, _: &str, _: &CancellationToken) -> Result<(Value, Option<Value>)> {
        unreachable!("the key test never dispatches")
    }
    fn wait_jobs(&self, _: &[String]) -> Result<Vec<pp_workspace::WaitJob>> {
        Ok(Vec::new())
    }
    fn acknowledge(&self, _: &[String]) -> Result<()> {
        Ok(())
    }
    fn check_lsp_cleanup(&self) -> Result<()> {
        Ok(())
    }
    fn cancel_runs(&self, _: &[String]) -> Result<()> {
        Ok(())
    }
    fn reap_runs(&self, _: &[String]) -> Result<()> {
        Ok(())
    }
    fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

/// A workspace-write sandbox with a fixed platform probe: no unit test starts a real sandbox process.
fn test_sandbox(dir: &Path) -> Arc<Sandbox> {
    Arc::new(Sandbox::with_probe_for_test(
        SandboxSettings {
            mode: pp_config::config::SandboxMode::WorkspaceWrite,
            ..Default::default()
        },
        dir,
        None,
        None,
        ProbeResult::Available,
    ))
}

/// One mutation entry point, the directory the fixture needs, and whether the tool table itself changes.
type Change = fn(&mut Tools, &Path);

#[test]
fn definitions_revision_tracks_every_definition_input() {
    let dir = tempfile::tempdir().unwrap();
    let changes: Vec<(&str, Change, bool)> = vec![
        (
            "limits",
            |tools, _| {
                let mut limits = tools.limits;
                limits.read_max_bytes = 4_096;
                tools.set_limits(limits);
            },
            true,
        ),
        (
            "web search settings",
            |tools, _| {
                tools.configure_web_search(pp_config::config::WebSearchSettings {
                    enabled: false,
                    ..Default::default()
                });
            },
            true,
        ),
        (
            "eval settings",
            |tools, _| {
                tools.configure_eval(&pp_config::config::EvalSettings {
                    enabled: false,
                    ..Default::default()
                });
            },
            false,
        ),
        (
            "mcp settings",
            |tools, _| {
                tools.configure_mcp(pp_config::config::McpSettings {
                    proxy: pp_config::config::McpProxyMode::Always,
                    ..Default::default()
                });
            },
            false,
        ),
        (
            "sandbox",
            |tools, dir| tools.set_sandbox(test_sandbox(dir)),
            true,
        ),
        (
            "lsp availability",
            |tools, _| tools.set_lsp_readonly(true),
            false,
        ),
        ("mcp access", |tools, _| tools.set_mcp_readonly(true), false),
        (
            "allowed tools",
            |tools, _| {
                tools.set_allowed_tools(Some(
                    crate::AllowedTools::parse_cli(&["Read".to_owned()]).unwrap(),
                ));
            },
            true,
        ),
        (
            "yield schema",
            |tools, _| tools.set_yield_schema(Some(json!({"type": "object"}))),
            true,
        ),
        (
            "ask handler",
            |tools, _| {
                tools.set_ask_handler(Some(Arc::new(|_, _| Ok(Vec::new()))));
            },
            true,
        ),
        (
            "subagent definitions",
            |tools, _| tools.set_subagent(Some(Arc::new(StubHost))),
            true,
        ),
        (
            "permission preset",
            |tools, _| {
                tools
                    .permission
                    .select(Some(pp_config::config::PermissionMode::DangerFullAccess))
            },
            false,
        ),
        (
            "mcp manager",
            |tools, _| tools.set_mcp(Arc::new(pp_mcp::McpManager::empty())),
            false,
        ),
        (
            "permission gate",
            |tools, _| tools.set_permission(permission::Gate::new()),
            false,
        ),
        (
            "skills",
            |tools, dir| tools.set_skills(pp_config::skills::Skills::from_dir(dir).shared()),
            false,
        ),
        (
            "lsp registry",
            |tools, dir| {
                tools.install_lsp(pp_lsp::Registry::new(
                    dir.to_path_buf(),
                    Some(dir.join("lsp.json")),
                    pp_config::config::LspSettings {
                        enabled: false,
                        ..Default::default()
                    },
                ));
            },
            false,
        ),
        (
            "explicit invalidation",
            |tools, _| tools.invalidate_definitions(),
            false,
        ),
    ];

    for (name, change, table_changes) in changes {
        let mut tools = Tools::new(dir.path().to_path_buf(), EditMode::Replace).unwrap();
        let before = tools.definitions_revision(WorkMode::Execute);
        let before_table = serde_json::to_value(tools.all_definitions(WorkMode::Execute)).unwrap();
        // A read-only query keeps the key and the table where they are.
        assert_eq!(
            tools.definitions_revision(WorkMode::Execute),
            before,
            "{name}: reading the key must not advance it"
        );
        change(&mut tools, dir.path());
        let after = tools.definitions_revision(WorkMode::Execute);
        let after_table = serde_json::to_value(tools.all_definitions(WorkMode::Execute)).unwrap();
        assert_ne!(after, before, "{name} must change the definitions key");
        assert_eq!(
            after_table != before_table,
            table_changes,
            "{name}: unexpected tool table change"
        );
        assert_eq!(
            tools.definitions_revision(WorkMode::Execute),
            after,
            "{name}: the key must be stable after the change"
        );
    }
}

#[test]
fn definitions_revision_separates_modes_and_live_sandbox_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = Tools::new(dir.path().to_path_buf(), EditMode::Replace).unwrap();
    let execute = tools.definitions_revision(WorkMode::Execute);
    assert_ne!(
        tools.definitions_revision(WorkMode::Plan),
        execute,
        "the work mode is part of the key"
    );
    // The permission preset decides the sandbox mode that trims schemas: reading it live keeps a preset
    // cycle visible without any explicit invalidation.
    tools
        .permission
        .select(Some(pp_config::config::PermissionMode::ReadOnly));
    tools.set_sandbox(test_sandbox(dir.path()));
    let restricted = tools.definitions_revision(WorkMode::Execute);
    assert_ne!(
        restricted, execute,
        "a permission preset change must be visible"
    );
    assert_eq!(
        restricted.sandbox,
        tools.sandbox_status(),
        "the key carries the live sandbox status"
    );
}
