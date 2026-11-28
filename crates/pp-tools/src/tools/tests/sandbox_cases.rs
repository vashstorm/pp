mod integration {
    use super::super::*;
    use pp_config::config::{SandboxMode, SandboxSettings};
    use pp_permission::PermissionDecision;
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
    fn sandbox_session_read_and_credential_denylist() {
        {
            let root = tempfile::tempdir().unwrap();
            let home = root.path().canonicalize().unwrap();
            let target = home.join(".pp/agent/sessions/sample.subagents");
            fs::create_dir_all(&target).unwrap();
            let workspace = tempfile::tempdir().unwrap();
            let mut tools = tools_with_sandbox(
                &workspace,
                &home,
                SandboxMode::WorkspaceWrite,
                ProbeResult::Available,
            );
            tools.permission = pp_permission::Gate::for_process(
                &home.join(".pp/agent"),
                Some(pp_config::config::PermissionMode::WorkspaceWrite),
                false,
            );
            let marker = workspace.path().join("started");
            let observed_marker = marker.clone();
            let observed_path = target.clone();
            tools.permission.set_handler(Box::new(move |request, _| {
                assert_eq!(request.tool, "bash");
                assert!(request.reason.contains(&*observed_path.to_string_lossy()));
                assert!(request.session_grant && request.persistent_grant);
                assert!(!observed_marker.exists());
                Ok(PermissionDecision::Deny)
            }));

            for arguments in [
                json!({"command": format!("printf started > started; ls -la -- '{}'", target.display())}),
                json!({
                    "command": format!("printf started > started; python3 - '{}' <<'PY'\nimport os, sys\nprint(os.listdir(sys.argv[1]))\nPY", target.display()),
                    "sandbox_permissions": {"read": [target]}
                }),
            ] {
                let result = execute(&tools, "bash", arguments);
                assert_eq!(result["ok"], false, "{result}");
                assert!(
                    result["error"]
                        .as_str()
                        .is_some_and(|error| error.contains("denied by the user")),
                    "{result}"
                );
                assert!(!marker.exists());
            }
        }

        {
            use pp_config::config::{PermissionMode, PermissionPolicy, ToolsSettings};
            use std::os::unix::fs::symlink;

            let home = tempfile::tempdir().unwrap();
            let home = home.path().canonicalize().unwrap();
            let agent = home.join(".pp/agent");
            let sessions = agent.join("sessions/run.subagents");
            fs::create_dir_all(&sessions).unwrap();
            let target = sessions.join("a2.events.jsonl");
            let credential = agent.join("models.yml");
            fs::write(&target, "first event\nsecond event\n").unwrap();
            fs::write(&credential, "credential fixture\n").unwrap();
            let workspace = tempfile::tempdir().unwrap();
            let mut tools = tools_with_sandbox(
                &workspace,
                &home,
                SandboxMode::WorkspaceWrite,
                ProbeResult::Available,
            );
            tools.permission = pp_permission::Gate::for_process(
                &agent,
                Some(PermissionMode::WorkspaceWrite),
                true,
            );
            let read = |tools: &Tools, path: String| execute(tools, "read", json!({"path": path}));
            let request_path = format!("{}:2-2:raw", target.display());
            let result = read(&tools, request_path.clone());
            assert_eq!(result["ok"], false, "{result}");
            assert!(
                result["error"]
                    .as_str()
                    .unwrap()
                    .contains("requires permission")
            );

            tools.permission.set_config(ToolsSettings {
                permission: [("read".into(), PermissionPolicy::Allow)].into(),
                ..Default::default()
            });
            let expected_target = target.clone();
            tools.permission.set_handler(Box::new(move |request, _| {
                assert_eq!(request.tool, "read");
                assert!(request.reason.contains(&*expected_target.to_string_lossy()));
                assert!(request.session_grant && request.persistent_grant);
                Ok(PermissionDecision::Approve)
            }));
            let result = read(&tools, request_path.clone());
            assert_eq!(result["content"], "second event\n", "{result}");
            let denied = read(&tools, credential.display().to_string());
            assert_eq!(denied["ok"], false, "{denied}");
            assert!(
                denied["error"]
                    .as_str()
                    .unwrap()
                    .contains("blocked by sandbox policy")
            );

            let changed_target = target.clone();
            tools.permission.set_handler(Box::new(move |_, _| {
                fs::remove_file(&changed_target)?;
                symlink(&credential, &changed_target)?;
                Ok(PermissionDecision::Approve)
            }));
            let changed = read(&tools, request_path);
            assert_eq!(changed["ok"], false, "{changed}");
            assert!(changed["error"].as_str().unwrap().contains("path changed"));
        }

        {
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
    }

    #[test]
    fn sandbox_permission_modes_and_overrides() {
        {
            use pp_config::config::{PermissionMode, ToolsSettings};

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
                    Some(PermissionMode::WorkspaceWrite)
                };
                tools.permission =
                    pp_permission::Gate::for_process(&agent_dir, startup, mode_override.is_some());
                tools.permission.set_config(ToolsSettings {
                    sandbox: sandbox_settings,
                    ..Default::default()
                });
                assert_eq!(tools.permission.snapshot(), PermissionMode::WorkspaceWrite);

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
                    let result = execute(&tools, tool, arguments);
                    assert_eq!(result["ok"], true, "{case} / {tool}: {result}");
                    assert!(marker_path.exists(), "{case} / {tool}: {result}");
                }

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
            tools.permission = pp_permission::Gate::for_process(
                &agent_dir,
                Some(PermissionMode::WorkspaceWrite),
                false,
            );
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

        {
            use pp_config::config::{PermissionMode, PermissionPolicy, ToolsSettings};
            let dir = tempfile::tempdir().unwrap();
            let tools =
                tools_with_sandbox(&dir, dir.path(), SandboxMode::Auto, ProbeResult::Available);
            tools.permission.set_config(ToolsSettings {
                permission: [("bash".into(), PermissionPolicy::Deny)].into(),
                ..Default::default()
            });
            tools
                .permission
                .select(Some(PermissionMode::WorkspaceWrite));
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
                    .contains("permission policy")
            );
            assert!(!dir.path().join("denied").exists());
        }

        {
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
                pp_permission::Gate::for_process(&agent_dir, Some(PermissionMode::ReadOnly), false);
            tools.permission.set_config(ToolsSettings::default());

            let first = execute(&tools, "write", json!({"path": "a.txt", "content": "one"}));
            assert_eq!(first["ok"], false, "{first}");

            tools
                .permission
                .select(Some(PermissionMode::DangerFullAccess));
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

        {
            use pp_config::config::{PermissionMode, ToolsSettings};

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
                pp_permission::Gate::for_process(&agent_dir, Some(PermissionMode::ReadOnly), true);
            tools.permission.set_config(ToolsSettings::default());

            let gate = tools.permission.clone();
            let sbx = Arc::clone(&sandbox);
            tools.permission.set_handler(Box::new(move |_, _| {
                gate.select(Some(PermissionMode::WorkspaceWrite));
                sbx.set_mode_override(None);
                Ok(PermissionDecision::Approve)
            }));

            let first = execute(
                &tools,
                "write",
                json!({"path": "frozen.txt", "content": "1"}),
            );
            assert_eq!(first["ok"], true, "{first}");
            assert_eq!(
                fs::read_to_string(dir.path().join("frozen.txt")).unwrap(),
                "1"
            );

            tools.permission.set_handler(Box::new(|_, _| {
                panic!("workspace-write should not prompt for ordinary workspace write")
            }));
            let second = execute(&tools, "write", json!({"path": "next.txt", "content": "2"}));
            assert_eq!(second["ok"], true, "{second}");
            assert_eq!(
                fs::read_to_string(dir.path().join("next.txt")).unwrap(),
                "2"
            );
        }
    }

    #[test]
    fn sandbox_url_and_bash_escalation_gates() {
        {
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
                    &mut |_, _| {
                        unreachable!("web handler must not be called when sandbox blocks URL read")
                    },
                );
                let val: Value = serde_json::from_str(&exec.output).unwrap();
                assert_eq!(val["ok"], false, "{val}");
            }
        }

        {
            use pp_config::config::{PermissionMode, ToolsSettings};
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
            tools.permission.set_config(ToolsSettings::default());
            tools
                .permission
                .select(Some(PermissionMode::WorkspaceWrite));

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
                    let completed =
                        background_wait(&tools, id, |snapshot| snapshot["status"] != "running");
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

        {
            use pp_config::config::{PermissionMode, ToolsSettings};

            let dir = tempfile::tempdir().unwrap();
            let agent_dir = dir.path().join("agent");
            fs::create_dir_all(&agent_dir).unwrap();

            let mut yolo_sandboxed =
                Tools::new(dir.path().to_path_buf(), EditMode::Replace).unwrap();
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
            yolo_sandboxed.permission = pp_permission::Gate::for_process(
                &agent_dir,
                Some(PermissionMode::DangerFullAccess),
                true,
            );
            yolo_sandboxed
                .permission
                .set_config(ToolsSettings::default());
            let _ = execute(
                &yolo_sandboxed,
                "bash",
                json!({
                    "command": "printf ok",
                    "sandbox_permissions": {"network": true},
                }),
            );

            for (mode, override_mode, probe, startup) in [
                (
                    SandboxMode::WorkspaceWrite,
                    Some(SandboxMode::Off),
                    ProbeResult::Available,
                    Some(PermissionMode::WorkspaceWrite),
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
                assert_eq!(tools.permission.snapshot(), PermissionMode::WorkspaceWrite);
                let res = execute(
                    &tools,
                    "bash",
                    json!({
                        "command": "printf should-not-run > nope.txt",
                        "sandbox_permissions": {"network": true},
                    }),
                );
                assert_eq!(res["ok"], false, "{res}");
                assert!(!dir.path().join("nope.txt").exists());
            }
        }
    }

    #[test]
    fn sandbox_workspace_protection_mcp_and_home_read() {
        {
            use pp_config::config::{PermissionMode, ToolsSettings};

            let root = outside_dir();
            let ws = root.path().join("ws");
            let outside = root.path().join("outside");
            let agent_dir = root.path().join("agent");
            fs::create_dir_all(ws.join(".git")).unwrap();
            fs::create_dir_all(ws.join(".agents")).unwrap();
            fs::create_dir_all(&outside).unwrap();
            fs::create_dir_all(&agent_dir).unwrap();

            let mut tools = Tools::new(ws.clone(), EditMode::Replace).unwrap();
            tools.set_sandbox(Arc::new(Sandbox::with_probe_for_test(
                SandboxSettings {
                    mode: SandboxMode::WorkspaceWrite,
                    network: false,
                    ..Default::default()
                },
                &ws,
                None,
                Some(Path::new("/tmp")),
                ProbeResult::Available,
            )));
            tools.permission = pp_permission::Gate::for_process(
                &agent_dir,
                Some(PermissionMode::WorkspaceWrite),
                false,
            );
            tools.permission.set_config(ToolsSettings::default());

            for path in [
                ws.join(".agents/skill.md"),
                ws.join(".git/config"),
                outside.join("ext.txt"),
            ] {
                let res = execute(&tools, "write", json!({"path": path, "content": "x"}));
                assert_eq!(res["ok"], false, "{res}");
                assert!(!path.exists());
            }

            let approved_agents = ws.join(".agents/skill.md");
            let external_file = outside.join("ext.txt");
            tools
                .permission
                .set_handler(Box::new(|_, _| Ok(PermissionDecision::Approve)));
            for path in [&approved_agents, &external_file] {
                let res = execute(&tools, "write", json!({"path": path, "content": "ok"}));
                assert_eq!(res["ok"], true, "{res}");
                assert_eq!(fs::read_to_string(path).unwrap(), "ok");
            }

            tools.permission.set_handler(Box::new(|_, _| {
                panic!("Full Access must not prompt for outside write")
            }));
            tools
                .permission
                .select(Some(PermissionMode::DangerFullAccess));
            let yolo_ext = outside.join("yolo.txt");
            let res = execute(
                &tools,
                "write",
                json!({"path": yolo_ext, "content": "yolo"}),
            );
            assert_eq!(res["ok"], true, "{res}");
            assert_eq!(fs::read_to_string(&yolo_ext).unwrap(), "yolo");
        }

        {
            use pp_config::config::{PermissionMode, ToolsSettings};

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

            let mut tools =
                Tools::with_mcp(root.clone(), EditMode::Replace, Arc::clone(&mcp)).unwrap();
            tools.permission = pp_permission::Gate::for_process(
                &agent_dir,
                Some(PermissionMode::WorkspaceWrite),
                false,
            );
            tools.permission.set_config(ToolsSettings::default());
            tools.permission.set_handler(Box::new(|request, _| {
                assert!(request.persistent_grant);
                Ok(PermissionDecision::ApprovePersistently)
            }));

            let first = execute(&tools, "mcp__svc__echo", json!({"text": "hi"}));
            assert_eq!(first["ok"], true, "{first}");

            let mut reused =
                Tools::with_mcp(root.clone(), EditMode::Replace, Arc::clone(&mcp)).unwrap();
            reused.permission = pp_permission::Gate::for_process(
                &agent_dir,
                Some(PermissionMode::WorkspaceWrite),
                false,
            );
            reused.permission.set_config(ToolsSettings::default());
            let second = execute(&reused, "mcp__svc__echo", json!({"text": "again"}));
            assert_eq!(second["ok"], true, "{second}");

            write_mcp("server-two");
            mcp.manage(&pp_mcp::Action::Reload, &token, pp_mcp::Access::Full)
                .unwrap();
            let after_reload = execute(&reused, "mcp__svc__echo", json!({"text": "still-allowed"}));
            assert_eq!(after_reload["ok"], true, "{after_reload}");

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

        {
            use pp_config::config::{PermissionMode, ToolsSettings};
            use std::os::unix::fs::symlink;

            let root = tempfile::tempdir().unwrap();
            let root = root.path().canonicalize().unwrap();
            let home = root.join("home");
            let agent = home.join(".pp/agent");
            let outside_dir = home.join("notes");
            let outside_file = outside_dir.join("secret.rs");
            let swapped = outside_dir.join("swapped.rs");
            let credential = home.join(".ssh/id_ed25519");
            fs::create_dir_all(&agent).unwrap();
            fs::create_dir_all(&outside_dir).unwrap();
            fs::create_dir_all(credential.parent().unwrap()).unwrap();
            fs::write(&outside_file, "fn secret_needle() {}\n").unwrap();
            fs::write(&swapped, "safe\n").unwrap();
            fs::write(&credential, "SSH-KEY").unwrap();

            let workspace = tempfile::tempdir().unwrap();
            let mut tools = tools_with_sandbox(
                &workspace,
                &home,
                SandboxMode::WorkspaceWrite,
                ProbeResult::Available,
            );
            tools.permission = pp_permission::Gate::for_process(
                &agent,
                Some(PermissionMode::WorkspaceWrite),
                true,
            );
            tools.permission.set_config(ToolsSettings::default());

            let run_policy = |tools: &Tools, name: &str, args: Value, policy: ToolPolicy<'_>| {
                let token = CancellationToken::new();
                let execution = tools.execute(
                    name,
                    &args.to_string(),
                    "test-call",
                    policy,
                    &token,
                    &mut |_| {},
                );
                assert!(execution.stop_error.is_none());
                serde_json::from_str::<Value>(&execution.output).unwrap()
            };

            let document = pp_core::plan::PlanDocument::new().unwrap();
            for (name, args) in [
                ("read", json!({"path": outside_file.display().to_string()})),
                (
                    "rg",
                    json!({"pattern": "secret_needle", "path": outside_dir.display().to_string()}),
                ),
                (
                    "glob",
                    json!({"pattern": "*.rs", "path": outside_dir.display().to_string()}),
                ),
                (
                    "lsp",
                    json!({"action": "definition", "file": outside_file.display().to_string(), "line": 1, "column": 1}),
                ),
            ] {
                let exec_res = run_policy(
                    &tools,
                    name,
                    args.clone(),
                    ToolPolicy::Execute {
                        workflow: &WorkflowState::default(),
                    },
                );
                assert_eq!(exec_res["ok"], false, "{name}: {exec_res}");

                let review_res = run_policy(&tools, name, args.clone(), ToolPolicy::Review);
                assert_eq!(review_res["ok"], false, "{name} review: {review_res}");

                if matches!(name, "read" | "rg" | "glob") {
                    let plan_res = run_policy(
                        &tools,
                        name,
                        args,
                        ToolPolicy::Plan {
                            document: &document,
                            sealed: false,
                            input: None,
                        },
                    );
                    assert_eq!(plan_res["ok"], false, "{name} plan: {plan_res}");
                }
            }

            tools.permission.set_handler(Box::new(|_, _| {
                panic!("immutable credential must not prompt")
            }));
            let cred_lsp = execute(
                &tools,
                "lsp",
                json!({"action": "definition", "file": credential.display().to_string(), "line": 1, "column": 1}),
            );
            assert_eq!(cred_lsp["ok"], false, "{cred_lsp}");

            tools
                .permission
                .set_handler(Box::new(|_, _| Ok(PermissionDecision::Approve)));
            assert_eq!(
                execute(
                    &tools,
                    "read",
                    json!({"path": outside_file.display().to_string()})
                )["ok"],
                true
            );
            assert_eq!(
                execute(
                    &tools,
                    "rg",
                    json!({"pattern": "secret_needle", "path": outside_dir.display().to_string()})
                )["ok"],
                true
            );
            let glob_plan = run_policy(
                &tools,
                "glob",
                json!({"pattern": "*.rs", "path": outside_dir.display().to_string()}),
                ToolPolicy::Plan {
                    document: &document,
                    sealed: false,
                    input: None,
                },
            );
            assert_eq!(glob_plan["ok"], true, "{glob_plan}");

            let lsp_after_approve = execute(
                &tools,
                "lsp",
                json!({"action": "definition", "file": outside_file.display().to_string(), "line": 1, "column": 1}),
            );
            assert!(
                !lsp_after_approve["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("permission")
                    && !lsp_after_approve["error"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("sandbox"),
                "{lsp_after_approve}"
            );

            let toctou_target = swapped.clone();
            let toctou_cred = credential.clone();
            tools.permission.set_handler(Box::new(move |_, _| {
                fs::remove_file(&toctou_target).unwrap();
                symlink(&toctou_cred, &toctou_target).unwrap();
                Ok(PermissionDecision::Approve)
            }));
            let toctou_res = execute(
                &tools,
                "read",
                json!({"path": swapped.display().to_string()}),
            );
            assert_eq!(toctou_res["ok"], false, "{toctou_res}");
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn sandbox_live_bash_and_eval_enforcement() {
        {
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

            let defs = tools.all_definitions(WorkMode::Execute);
            let bash_def = defs.iter().find(|d| d.name == "bash").unwrap();
            let bash_desc = bash_def.description.as_deref().unwrap();
            assert!(!bash_desc.contains("No filesystem sandbox"), "{bash_desc}");
            assert!(
                bash_desc.contains("Sandboxed (workspace-write"),
                "{bash_desc}"
            );
            assert!(
                bash_def.parameters.as_ref().unwrap()["properties"]
                    .get("sandbox_permissions")
                    .is_none(),
                "YOLO mode must not expose sandbox_permissions"
            );
            tools
                .permission
                .select(Some(pp_config::config::PermissionMode::WorkspaceWrite));
            let non_yolo_defs = tools.all_definitions(WorkMode::Execute);
            let non_yolo_bash = non_yolo_defs.iter().find(|d| d.name == "bash").unwrap();
            assert!(
                non_yolo_bash.parameters.as_ref().unwrap()["properties"]
                    .get("sandbox_permissions")
                    .is_some(),
                "active sandbox in non-YOLO mode must expose sandbox_permissions"
            );

            let mut off_tools =
                tools_with_sandbox(&root, &home, SandboxMode::Off, ProbeResult::Available);
            off_tools.permission = pp_permission::Gate::for_process(&home, None, false);
            let off_defs = off_tools.all_definitions(WorkMode::Execute);
            let off_bash = off_defs.iter().find(|d| d.name == "bash").unwrap();
            assert!(
                off_bash.parameters.as_ref().unwrap()["properties"]
                    .get("sandbox_permissions")
                    .is_none(),
                "disabled sandbox must not expose sandbox_permissions"
            );

            let mut ro_tools =
                tools_with_sandbox(&root, &home, SandboxMode::ReadOnly, ProbeResult::Available);
            ro_tools.eval_py_available = true;
            let ro_defs = ro_tools.all_definitions(WorkMode::Execute);
            for name in ["bash", "eval"] {
                let desc = ro_defs
                    .iter()
                    .find(|d| d.name == name)
                    .and_then(|d| d.description.as_deref())
                    .unwrap();
                assert!(desc.contains("Sandboxed (read-only"), "{name}: {desc}");
                assert!(desc.contains("workspace is read-only"), "{name}: {desc}");
                assert!(
                    !desc.contains("writes restricted to workspace/tmp"),
                    "{name}: {desc}"
                );
            }

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

        {
            use pp_config::config::PermissionMode;

            let root = tempfile::tempdir().unwrap();
            let root = root.path().canonicalize().unwrap();
            let home = root.join("home");
            let note = home.join("private_note.txt");
            fs::create_dir_all(&home).unwrap();
            fs::write(&note, "python-secret-note").unwrap();

            let workspace = tempfile::tempdir().unwrap();
            if Sandbox::new(SandboxSettings::default(), workspace.path())
                .status()
                .effective
                != pp_sandbox::EffectiveMode::Off
            {
                let mut tools = tools_with_sandbox(
                    &workspace,
                    &home,
                    SandboxMode::WorkspaceWrite,
                    ProbeResult::Available,
                );
                tools.permission = pp_permission::Gate::for_process(
                    &home.join(".pp/agent"),
                    Some(PermissionMode::WorkspaceWrite),
                    true,
                );
                tools
                    .permission
                    .set_handler(Box::new(|_, _| Ok(PermissionDecision::Approve)));

                let py_cmd = format!(
                    "python3 -c 'import sys; print(open(sys.argv[1]).read())' '{}'",
                    note.display()
                );
                let blocked = execute(&tools, "bash", json!({ "command": py_cmd }));
                assert_eq!(blocked["ok"], false, "{blocked}");

                let allowed = execute(
                    &tools,
                    "bash",
                    json!({
                        "command": py_cmd,
                        "sandbox_permissions": {
                            "read": [note.display().to_string()]
                        }
                    }),
                );
                assert_eq!(allowed["ok"], true, "{allowed}");
                assert!(
                    allowed["output"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("python-secret-note"),
                    "{allowed}"
                );
            }
        }

        {
            use pp_config::config::{EvalSettings, PermissionMode};

            let root = tempfile::tempdir().unwrap();
            let root = root.path().canonicalize().unwrap();
            let home = root.join("home");
            let outside = home.join("eval_secret.txt");
            fs::create_dir_all(home.join(".pp/agent")).unwrap();
            fs::write(&outside, "from-eval-bridge").unwrap();

            let workspace = tempfile::tempdir().unwrap();
            if Sandbox::new(SandboxSettings::default(), workspace.path())
                .status()
                .effective
                != pp_sandbox::EffectiveMode::Off
            {
                let mut tools = tools_with_sandbox(
                    &workspace,
                    &home,
                    SandboxMode::WorkspaceWrite,
                    ProbeResult::Available,
                );
                tools.configure_eval(&EvalSettings::default());
                tools.permission = pp_permission::Gate::for_process(
                    &home.join(".pp/agent"),
                    Some(PermissionMode::WorkspaceWrite),
                    true,
                );

                let direct = execute(
                    &tools,
                    "eval",
                    json!({ "language": "python", "code": format!("print(open({:?}).read())", outside.to_string_lossy()) }),
                );
                assert_eq!(direct["ok"], false, "{direct}");
                assert_eq!(direct["sandbox"]["blocked"], true, "{direct}");

                let bridge_denied = execute(
                    &tools,
                    "eval",
                    json!({ "language": "python", "code": format!("r = tool.read(path={:?})\nassert r['ok'], r", outside.to_string_lossy()) }),
                );
                assert_eq!(bridge_denied["ok"], false, "{bridge_denied}");

                tools
                    .permission
                    .set_handler(Box::new(|_, _| Ok(PermissionDecision::Approve)));
                let bridge_allowed = execute(
                    &tools,
                    "eval",
                    json!({ "language": "python", "code": format!("r = tool.read(path={:?})\nprint(r['ok'], 'from-eval-bridge' in r['content'])", outside.to_string_lossy()) }),
                );
                assert_eq!(bridge_allowed["ok"], true, "{bridge_allowed}");
                assert!(
                    bridge_allowed["output"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("True True"),
                    "{bridge_allowed}"
                );
            }
        }
    }
}
