use super::*;
use pp_config::config::{SandboxMode, SandboxSettings};
use pp_sandbox::{ProbeResult, Sandbox};
use std::sync::Arc;

fn tools_with_sandbox(
    dir: &tempfile::TempDir,
    home: &Path,
    mode: SandboxMode,
    probe: ProbeResult,
) -> Tools {
    let mut tools = Tools::new(dir.path().to_path_buf(), EditMode::Replace).unwrap();
    tools.set_sandbox(Arc::new(Sandbox::with_probe_for_test(
        SandboxSettings {
            mode,
            allow_read: vec!["~/.ssh/known_hosts".into()],
            ..Default::default()
        },
        dir.path(),
        Some(home),
        Some(Path::new("/tmp")),
        probe,
    )));
    tools
}

#[test]
fn sandbox_read_rg_and_glob_enforce_credential_denylist() {
    let root = tempfile::tempdir().unwrap();
    let root_canon = fs::canonicalize(root.path()).unwrap();
    let ws = root_canon.join("ws");
    let home = root_canon.join("home");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(home.join(".ssh")).unwrap();
    fs::write(ws.join("ok.txt"), "TOKEN_FIND_ME\n").unwrap();
    fs::write(home.join("public.txt"), "TOKEN_FIND_ME\n").unwrap();
    fs::write(home.join(".ssh/id_ed25519"), "TOKEN_FIND_ME\n").unwrap();
    fs::write(home.join(".ssh/known_hosts"), "KNOWN_HOST\n").unwrap();

    let ws_dir = tempfile::Builder::new().tempdir_in(&ws).unwrap();
    let tools = tools_with_sandbox(
        &ws_dir,
        &home,
        SandboxMode::WorkspaceWrite,
        ProbeResult::Available,
    );

    // read: 拒绝凭据、放行 allowRead、正常读取普通文件
    let denied = execute(
        &tools,
        "read",
        json!({"path": home.join(".ssh/id_ed25519")}),
    );
    assert_eq!(denied["ok"], false, "{denied}");
    assert!(
        denied["error"]
            .as_str()
            .unwrap()
            .contains("blocked by sandbox policy"),
        "{denied}"
    );

    let allowed = execute(
        &tools,
        "read",
        json!({"path": home.join(".ssh/known_hosts")}),
    );
    assert_eq!(allowed["ok"], true, "{allowed}");

    // rg: 直接搜拒绝路径失败；搜父目录时跳过 .ssh，不泄漏 id_ed25519 内容
    let rg_denied = execute(
        &tools,
        "rg",
        json!({"pattern": "TOKEN_FIND_ME", "path": home.join(".ssh/id_ed25519")}),
    );
    assert_eq!(rg_denied["ok"], false, "{rg_denied}");

    let rg_parent = execute(
        &tools,
        "rg",
        json!({"pattern": "TOKEN_FIND_ME", "path": home}),
    );
    assert_eq!(rg_parent["ok"], true, "{rg_parent}");
    let rg_text = rg_parent.to_string();
    assert!(rg_text.contains("public.txt"), "{rg_parent}");
    assert!(!rg_text.contains("id_ed25519"), "{rg_parent}");

    // glob: 直接搜 .ssh 失败；搜父目录跳过 .ssh
    let glob_denied = execute(
        &tools,
        "glob",
        json!({"pattern": "*", "path": home.join(".ssh")}),
    );
    assert_eq!(glob_denied["ok"], false, "{glob_denied}");

    let glob_parent = execute(&tools, "glob", json!({"pattern": "**/*", "path": home}));
    assert_eq!(glob_parent["ok"], true, "{glob_parent}");
    let glob_text = glob_parent["content"].as_str().unwrap_or_default();
    assert!(glob_text.contains("public.txt"), "{glob_parent}");
    assert!(!glob_text.contains("id_ed25519"), "{glob_parent}");
}

#[test]
fn accept_edits_reflects_real_sandbox_policy_per_tool_tier() {
    use pp_config::config::{PermissionMode, ToolsSettings};
    use pp_permission::PermissionDecision;

    let root = tempfile::tempdir().unwrap();
    let agent_dir = root.path().join("agent");
    fs::create_dir_all(&agent_dir).unwrap();
    let ws = root.path().join("ws");
    fs::create_dir_all(&ws).unwrap();

    // (说明, 配置模式, 覆盖, 探针, 写工具, 期望放行)
    // 可用 workspace-write: accept-edits 下 write/edit/bash 自动执行;
    // off 与 auto 降级: 同一批写工具必须回到审批 (无 handler 时 fail-closed);
    // 显式 workspace-write + 不可用探针: 直接 fail-closed。
    for (case, configured, mode_override, probe) in [
        (
            "available workspace-write",
            SandboxMode::WorkspaceWrite,
            None,
            ProbeResult::Available,
        ),
        (
            "explicit /sandbox off override",
            SandboxMode::WorkspaceWrite,
            Some(SandboxMode::Off),
            ProbeResult::Available,
        ),
        (
            "auto degraded to off",
            SandboxMode::Auto,
            None,
            ProbeResult::Unavailable {
                reason: "degraded".into(),
            },
        ),
    ] {
        let ws_dir = tempfile::Builder::new().tempdir_in(&ws).unwrap();
        fs::write(ws_dir.path().join("edit-target.txt"), "before\n").unwrap();
        let mut tools = Tools::new(ws_dir.path().to_path_buf(), EditMode::Replace).unwrap();
        // auto 场景还原 settings.yml 显式配置 tools.sandbox 的生产路径：
        // 无启动档位/覆盖（manage_sandbox=false），view 回退到 configured=auto，
        // 探针不可用时降级为 off；权限档位来自 set_config 后的默认 AcceptEdits。
        // 其余场景保持默认启动路径（--permission-mode / /sandbox 覆盖）。
        let sandbox_settings = SandboxSettings {
            mode: configured,
            network: !case.contains("auto degraded"),
            ..Default::default()
        };
        let sbx = Arc::new(Sandbox::with_probe_for_test(
            sandbox_settings.clone(),
            ws_dir.path(),
            None,
            Some(Path::new("/tmp")),
            probe,
        ));
        if let Some(mode) = mode_override {
            sbx.set_mode_override(Some(mode));
        }
        tools.set_sandbox(sbx);
        let startup = if case.contains("auto degraded") {
            None
        } else {
            Some(PermissionMode::AcceptEdits)
        };
        tools.permission =
            pp_permission::Gate::for_process(&agent_dir, startup, mode_override.is_some());
        tools.permission.set_config(ToolsSettings {
            sandbox: sandbox_settings,
            ..Default::default()
        });
        assert_eq!(tools.permission.snapshot(), PermissionMode::AcceptEdits);

        let expect_auto = case.contains("available workspace-write");
        for (tool, arguments, marker) in [
            (
                "write",
                json!({"path": "w.txt", "content": "written"}),
                "w.txt",
            ),
            (
                "edit",
                json!({"path": "edit-target.txt", "edits": [
                    {"oldText": "before", "newText": "after"}
                ]}),
                "edit-target.txt",
            ),
        ] {
            let marker_path = ws_dir.path().join(marker);
            let before = fs::read_to_string(&marker_path).ok();
            let result = execute(&tools, tool, arguments);
            assert_eq!(result["ok"], expect_auto, "{case} / {tool}: {result}");
            if !expect_auto {
                assert!(
                    result["error"]
                        .as_str()
                        .is_some_and(|error| error.contains("requires permission")),
                    "{case} / {tool}: {result}"
                );
            }
            if expect_auto {
                assert!(marker_path.exists(), "{case} / {tool}: {result}");
            } else {
                assert_eq!(before, fs::read_to_string(&marker_path).ok());
            }
        }

        // 普通写 Bash: 同一沙盒判定决定免确认或 fail-closed, marker 不落盘。
        let bash_marker = ws_dir.path().join("bash-marker");
        let bash = execute(
            &tools,
            "bash",
            json!({"command": format!("printf x > {}", bash_marker.display())}),
        );
        assert_eq!(bash["ok"], expect_auto, "{case} / bash: {bash}");
        assert_eq!(bash_marker.exists(), expect_auto);
    }

    // 显式 workspace-write + 平台不可用: 在审批之前 fail-closed。
    let ws_dir = tempfile::Builder::new().tempdir_in(&ws).unwrap();
    let mut tools = Tools::new(ws_dir.path().to_path_buf(), EditMode::Replace).unwrap();
    tools.set_sandbox(Arc::new(Sandbox::with_probe_for_test(
        SandboxSettings {
            mode: SandboxMode::WorkspaceWrite,
            ..Default::default()
        },
        ws_dir.path(),
        None,
        Some(Path::new("/tmp")),
        ProbeResult::Unavailable {
            reason: "nested".into(),
        },
    )));
    tools.permission =
        pp_permission::Gate::for_process(&agent_dir, Some(PermissionMode::AcceptEdits), false);
    tools.permission.set_config(ToolsSettings::default());
    tools.permission.set_handler(Box::new(|_, _| {
        panic!("显式开启但不可用必须直接 fail-closed, 不进入审批")
    }));
    let blocked = execute(
        &tools,
        "bash",
        json!({"command": format!("printf x > {}", ws_dir.path().join("nope").display())}),
    );
    assert_eq!(blocked["ok"], false, "{blocked}");
    assert!(
        blocked["error"]
            .as_str()
            .unwrap()
            .contains("is enabled but unavailable"),
        "{blocked}"
    );
    assert!(!ws_dir.path().join("nope").exists());

    // 有 handler 时降级场景走正常审批, 批准后正常执行。
    let ws_dir = tempfile::Builder::new().tempdir_in(&ws).unwrap();
    let mut tools = Tools::new(ws_dir.path().to_path_buf(), EditMode::Replace).unwrap();
    let sandbox_settings = SandboxSettings {
        mode: SandboxMode::Auto,
        network: false,
        ..Default::default()
    };
    tools.set_sandbox(Arc::new(Sandbox::with_probe_for_test(
        sandbox_settings.clone(),
        ws_dir.path(),
        None,
        Some(Path::new("/tmp")),
        ProbeResult::Unavailable {
            reason: "degraded".into(),
        },
    )));
    tools.permission = pp_permission::Gate::for_process(&agent_dir, None, false);
    tools.permission.set_config(ToolsSettings {
        sandbox: sandbox_settings,
        ..Default::default()
    });
    tools.permission.set_handler(Box::new(|request, _| {
        assert!(
            request.reason.contains("outside the workspace sandbox"),
            "{reason}",
            reason = request.reason
        );
        Ok(PermissionDecision::Approve)
    }));
    let approved = execute(
        &tools,
        "write",
        json!({"path": "approved.txt", "content": "ok"}),
    );
    assert_eq!(approved["ok"], true, "{approved}");
    assert_eq!(
        fs::read_to_string(ws_dir.path().join("approved.txt")).unwrap(),
        "ok"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn sandbox_bash_tool_reports_sandbox_metadata_and_unavailable_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(root.path().join(".pp")).unwrap();
    fs::create_dir_all(&home).unwrap();
    let tools = tools_with_sandbox(
        &root,
        &home,
        SandboxMode::WorkspaceWrite,
        ProbeResult::Available,
    );

    let live_supported = Sandbox::new(SandboxSettings::default(), root.path())
        .status()
        .effective
        != pp_sandbox::EffectiveMode::Off;
    if live_supported {
        let blocked = execute(&tools, "bash", json!({"command": "touch .pp/hooks.yml"}));
        assert_eq!(blocked["ok"], false, "{blocked}");
        assert_eq!(blocked["sandbox"]["blocked"], true, "{blocked}");

        let allowed = execute(&tools, "bash", json!({"command": "echo hi > ok.txt"}));
        assert_eq!(allowed["ok"], true, "{allowed}");
        assert!(allowed.get("sandbox").is_none(), "{allowed}");
    }

    // 工具定义随状态更新，不再保留 "No filesystem sandbox"
    let defs = tools.all_definitions(WorkMode::Execute);
    let bash_desc = defs
        .iter()
        .find(|d| d.name == "bash")
        .and_then(|d| d.description.as_deref())
        .unwrap();
    assert!(!bash_desc.contains("No filesystem sandbox"), "{bash_desc}");
    assert!(
        bash_desc.contains("Sandboxed (workspace-write"),
        "{bash_desc}"
    );

    // 显式开启但平台不可用时 fail-closed
    let fail_closed = tools_with_sandbox(
        &root,
        &home,
        SandboxMode::WorkspaceWrite,
        ProbeResult::Unavailable {
            reason: "nested".into(),
        },
    );
    let err = execute(&fail_closed, "bash", json!({"command": "echo hi"}));
    assert_eq!(err["ok"], false, "{err}");
    assert!(
        err["error"].as_str().unwrap().contains("unavailable"),
        "{err}"
    );
}

#[test]
fn sandbox_read_url_respects_network_disabled_and_unavailable_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let workflow = WorkflowState::default();

    for (mode, network, probe) in [
        (SandboxMode::ReadOnly, false, ProbeResult::Available),
        (
            SandboxMode::WorkspaceWrite,
            true,
            ProbeResult::Unavailable {
                reason: "nested".into(),
            },
        ),
    ] {
        let mut tools = Tools::new(root.path().to_path_buf(), EditMode::Replace).unwrap();
        tools.set_sandbox(Arc::new(Sandbox::with_probe_for_test(
            SandboxSettings {
                mode,
                network,
                ..Default::default()
            },
            root.path(),
            Some(&home),
            Some(Path::new("/tmp")),
            probe,
        )));
        let exec = tools.execute_with_web(
            "read",
            &json!({"path": "https://example.com/secret"}).to_string(),
            "read-url-sbx",
            ToolPolicy::Execute {
                workflow: &workflow,
            },
            None,
            &CancellationToken::new(),
            &mut |_| {},
            &mut |_, _| unreachable!("web handler must not be called when sandbox blocks URL read"),
        );
        let val: Value = serde_json::from_str(&exec.output).unwrap();
        assert_eq!(val["ok"], false, "{val}");
    }
}

#[test]
fn accept_edits_authorizes_bash_before_start_and_does_not_replay_failed_calls() {
    use pp_config::config::{PermissionMode, ToolsSettings};
    use pp_permission::PermissionDecision;
    let dir = tempfile::tempdir().unwrap();
    let mut tools = Tools::new(dir.path().to_path_buf(), EditMode::Replace).unwrap();
    tools.set_sandbox(Arc::new(Sandbox::with_probe_for_test(
        SandboxSettings {
            network: false,
            ..Default::default()
        },
        dir.path(),
        None,
        Some(Path::new("/tmp")),
        ProbeResult::Available,
    )));
    tools.permission.set_config(ToolsSettings {
        permission_mode: Some(PermissionMode::AcceptEdits),
        ..Default::default()
    });

    for (mode, decision, marker, expected) in [
        ("foreground", PermissionDecision::Deny, "denied", false),
        ("foreground", PermissionDecision::Approve, "approved", true),
        (
            "background",
            PermissionDecision::Deny,
            "denied-background",
            false,
        ),
        (
            "background",
            PermissionDecision::Approve,
            "approved-background",
            true,
        ),
    ] {
        let target = dir.path().join(marker);
        let before = target.clone();
        tools.permission.set_handler(Box::new(move |request, _| {
            assert!(!before.exists(), "命令在授权前启动");
            assert!(request.reason.contains("outbound network"));
            Ok(decision)
        }));
        let response = execute(
            &tools,
            "bash",
            json!({
                "mode": mode,
                "command": format!("printf done > {marker}"),
                "sandbox_permissions": {"network": true},
            }),
        );
        assert_eq!(response["ok"], expected, "{response}");
        if expected && mode == "background" {
            let id = response["task_id"].as_str().unwrap();
            let completed = background_wait(&tools, id, |snapshot| snapshot["status"] != "running");
            assert_eq!(completed["exit_code"], 0, "{completed}");
        }
        assert_eq!(target.exists(), expected);
    }

    tools
        .permission
        .set_handler(Box::new(|_, _| panic!("普通命令失败不应打开沙盒权限询问")));
    let failed = execute(
        &tools,
        "bash",
        json!({
            "command": "printf once >> failed; printf 'Permission denied'; exit 1",
        }),
    );
    assert_eq!(failed["ok"], false, "{failed}");
    assert_eq!(
        fs::read_to_string(dir.path().join("failed")).unwrap(),
        "once"
    );
}

#[test]
fn accept_edits_applies_ordinary_policy_before_sandbox_grants() {
    use pp_config::config::{PermissionMode, PermissionPolicy, ToolsSettings};
    let dir = tempfile::tempdir().unwrap();
    let tools = tools_with_sandbox(&dir, dir.path(), SandboxMode::Auto, ProbeResult::Available);
    tools.permission.set_config(ToolsSettings {
        permission_mode: Some(PermissionMode::AcceptEdits),
        permission: [("bash".into(), PermissionPolicy::Deny)].into(),
        ..Default::default()
    });
    let result = execute(
        &tools,
        "bash",
        json!({
            "command": "printf done > denied",
            "sandbox_permissions": {"network": true},
        }),
    );
    assert_eq!(result["ok"], false, "{result}");
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("tools.permission policy")
    );
    assert!(!dir.path().join("denied").exists());
}

#[test]
fn permission_switch_takes_effect_on_next_tool_call_and_tolerates_sandbox_permissions_in_yolo() {
    use pp_config::config::{PermissionMode, ToolsSettings};
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let mut tools = Tools::new(dir.path().to_path_buf(), EditMode::Replace).unwrap();
    tools.set_sandbox(Arc::new(Sandbox::with_probe_for_test(
        SandboxSettings {
            mode: SandboxMode::Auto,
            network: false,
            ..Default::default()
        },
        dir.path(),
        None,
        Some(Path::new("/tmp")),
        ProbeResult::Available,
    )));
    tools.permission =
        pp_permission::Gate::for_process(&agent_dir, Some(PermissionMode::AlwaysAsk), false);
    tools.permission.set_config(ToolsSettings::default());

    // 1. Always-ask 下写操作拒绝（无交互 handler）。
    let first = execute(&tools, "write", json!({"path": "a.txt", "content": "one"}));
    assert_eq!(first["ok"], false, "{first}");

    // 2. 运行中切换到 YOLO 后，下一次工具调用（含带 sandbox_permissions 的 bash）立即放行。
    tools.permission.select(Some(PermissionMode::Yolo));
    let second = execute(&tools, "write", json!({"path": "a.txt", "content": "one"}));
    assert_eq!(second["ok"], true, "{second}");
    let third = execute(
        &tools,
        "bash",
        json!({
            "command": "printf ok > b.txt",
            "sandbox_permissions": {"network": true},
        }),
    );
    assert_eq!(third["ok"], true, "{third}");
    assert_eq!(fs::read_to_string(dir.path().join("b.txt")).unwrap(), "ok");
}

#[test]
fn approval_wait_freezes_sandbox_override_until_next_call() {
    use pp_config::config::{PermissionMode, ToolsSettings};
    use pp_permission::PermissionDecision;

    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    fs::create_dir_all(&agent_dir).unwrap();
    let mut tools = Tools::new(dir.path().to_path_buf(), EditMode::Replace).unwrap();
    let sandbox = Arc::new(Sandbox::with_probe_for_test(
        SandboxSettings {
            mode: SandboxMode::WorkspaceWrite,
            ..Default::default()
        },
        dir.path(),
        None,
        Some(Path::new("/tmp")),
        ProbeResult::Available,
    ));
    sandbox.set_mode_override(Some(SandboxMode::ReadOnly));
    tools.set_sandbox(Arc::clone(&sandbox));
    tools.permission =
        pp_permission::Gate::for_process(&agent_dir, Some(PermissionMode::AlwaysAsk), true);
    tools.permission.set_config(ToolsSettings::default());

    let gate = tools.permission.clone();
    let sbx = Arc::clone(&sandbox);
    tools.permission.set_handler(Box::new(move |_, _| {
        // 模拟用户在 Always-ask 确认弹窗期间按 F6 切换到 Accept-edits（清除 /sandbox read-only 覆盖）。
        gate.select(Some(PermissionMode::AcceptEdits));
        sbx.set_mode_override(None);
        Ok(PermissionDecision::Approve)
    }));

    // 当前调用在进入审批前已冻结 read-only，审批通过后不得放宽为 workspace-write。
    let first = execute(
        &tools,
        "write",
        json!({"path": "frozen.txt", "content": "1"}),
    );
    assert_eq!(first["ok"], false, "{first}");
    assert!(!dir.path().join("frozen.txt").exists());

    // 下一次调用才应用切换后的 Accept-edits + workspace-write。
    let second = execute(&tools, "write", json!({"path": "next.txt", "content": "2"}));
    assert_eq!(second["ok"], true, "{second}");
    assert_eq!(
        fs::read_to_string(dir.path().join("next.txt")).unwrap(),
        "2"
    );
}

#[test]
fn bash_sandbox_permissions_requires_approval_when_sandbox_active_and_rejects_unavailable_in_non_yolo()
 {
    use pp_config::config::{PermissionMode, ToolsSettings};
    use pp_permission::PermissionDecision;

    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    fs::create_dir_all(&agent_dir).unwrap();

    // 1. YOLO 叠加显式沙盒覆盖（network=false）时，扩权请求仍必须经过审批。
    let mut yolo_sandboxed = Tools::new(dir.path().to_path_buf(), EditMode::Replace).unwrap();
    let sandbox = Arc::new(Sandbox::with_probe_for_test(
        SandboxSettings {
            mode: SandboxMode::WorkspaceWrite,
            network: false,
            ..Default::default()
        },
        dir.path(),
        None,
        Some(Path::new("/tmp")),
        ProbeResult::Available,
    ));
    sandbox.set_mode_override(Some(SandboxMode::ReadOnly));
    yolo_sandboxed.set_sandbox(sandbox);
    yolo_sandboxed.permission =
        pp_permission::Gate::for_process(&agent_dir, Some(PermissionMode::Yolo), true);
    yolo_sandboxed
        .permission
        .set_config(ToolsSettings::default());
    let denied = execute(
        &yolo_sandboxed,
        "bash",
        json!({
            "command": "printf ok",
            "sandbox_permissions": {"network": true},
        }),
    );
    assert_eq!(denied["ok"], false, "{denied}");
    assert!(
        denied["error"]
            .as_str()
            .unwrap()
            .contains("no interactive UI"),
        "{denied}"
    );
    yolo_sandboxed
        .permission
        .set_handler(Box::new(|request, _| {
            assert!(request.reason.contains("outbound network"));
            Ok(PermissionDecision::Approve)
        }));
    let approved = execute(
        &yolo_sandboxed,
        "bash",
        json!({
            "command": "printf ok",
            "sandbox_permissions": {"network": true},
        }),
    );
    assert_eq!(approved["ok"], true, "{approved}");

    // 2. Accept-edits 下显式 /sandbox off 或 auto 降级时，带 sandbox_permissions 必须返回 sandbox is unavailable。
    for (mode, override_mode, probe, startup) in [
        (
            SandboxMode::WorkspaceWrite,
            Some(SandboxMode::Off),
            ProbeResult::Available,
            Some(PermissionMode::AcceptEdits),
        ),
        (
            SandboxMode::Auto,
            None,
            ProbeResult::Unavailable {
                reason: "degraded".into(),
            },
            None,
        ),
    ] {
        let mut tools = Tools::new(dir.path().to_path_buf(), EditMode::Replace).unwrap();
        let settings = SandboxSettings {
            mode,
            network: false,
            ..Default::default()
        };
        let sbx = Arc::new(Sandbox::with_probe_for_test(
            settings.clone(),
            dir.path(),
            None,
            Some(Path::new("/tmp")),
            probe,
        ));
        if let Some(ov) = override_mode {
            sbx.set_mode_override(Some(ov));
        }
        tools.set_sandbox(sbx);
        tools.permission =
            pp_permission::Gate::for_process(&agent_dir, startup, override_mode.is_some());
        tools.permission.set_config(ToolsSettings {
            sandbox: settings,
            ..Default::default()
        });
        assert_eq!(tools.permission.snapshot(), PermissionMode::AcceptEdits);
        let res = execute(
            &tools,
            "bash",
            json!({
                "command": "printf should-not-run > nope.txt",
                "sandbox_permissions": {"network": true},
            }),
        );
        // 降级后审批反映真实策略：写命令先被权限门拦截（无 handler 时
        // fail-closed），不再落到 dispatch 层的 "sandbox is unavailable"。
        assert_eq!(res["ok"], false, "{res}");
        assert!(
            res["error"]
                .as_str()
                .unwrap()
                .contains("requires permission and no interactive UI"),
            "{res}"
        );
        assert!(!dir.path().join("nope.txt").exists());
    }
}

#[test]
fn mcp_persistent_grant_persists_across_restarts_and_revokes() {
    use pp_config::config::{PermissionMode, ToolsSettings};
    use pp_permission::PermissionDecision;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let agent_dir = root.join("agent");
    fs::create_dir_all(&agent_dir).unwrap();
    let mcp_path = agent_dir.join("mcp.json");
    let fixture = format!(
        "{}/../../tests/fixtures/mcp_server.sh",
        env!("CARGO_MANIFEST_DIR")
    );
    let write_mcp = |value: &str| {
        fs::write(
            &mcp_path,
            json!({
                "mcpServers": {
                    "svc": {
                        "command": "bash",
                        "args": [&fixture],
                        "env": { "MCP_VALUE": value }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
    };
    write_mcp("server-one");

    let mcp = Arc::new(pp_mcp::McpManager::new_isolated(
        mcp_path.clone(),
        root.clone(),
    ));
    let token = CancellationToken::new();
    mcp.connect_enabled(&token, pp_mcp::Access::Full).unwrap();

    let mut tools = Tools::with_mcp(root.clone(), EditMode::Replace, Arc::clone(&mcp)).unwrap();
    tools.permission =
        pp_permission::Gate::for_process(&agent_dir, Some(PermissionMode::AcceptEdits), false);
    tools.permission.set_config(ToolsSettings::default());
    tools.permission.set_handler(Box::new(|request, _| {
        assert!(request.persistent_grant);
        Ok(PermissionDecision::ApprovePersistently)
    }));

    let first = execute(&tools, "mcp__svc__echo", json!({"text": "hi"}));
    assert_eq!(first["ok"], true, "{first}");

    // 同一工具名在无交互 handler 下直接复用全局持久授权（即使服务重载也不依赖服务指纹）。
    let mut reused = Tools::with_mcp(root.clone(), EditMode::Replace, Arc::clone(&mcp)).unwrap();
    reused.permission =
        pp_permission::Gate::for_process(&agent_dir, Some(PermissionMode::AcceptEdits), false);
    reused.permission.set_config(ToolsSettings::default());
    let second = execute(&reused, "mcp__svc__echo", json!({"text": "again"}));
    assert_eq!(second["ok"], true, "{second}");

    write_mcp("server-two");
    mcp.manage(&pp_mcp::Action::Reload, &token, pp_mcp::Access::Full)
        .unwrap();
    let after_reload = execute(&reused, "mcp__svc__echo", json!({"text": "still-allowed"}));
    assert_eq!(after_reload["ok"], true, "{after_reload}");

    // 按工具名撤销后恢复拦截。
    reused.permission.revoke_rule("mcp__svc__echo").unwrap();
    let after_revoke = execute(&reused, "mcp__svc__echo", json!({"text": "blocked"}));
    assert_eq!(after_revoke["ok"], false, "{after_revoke}");
    assert!(
        after_revoke["error"]
            .as_str()
            .unwrap()
            .contains("no interactive UI"),
        "{after_revoke}"
    );
    mcp.shutdown().unwrap();
}
