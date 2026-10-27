// Real binary + loopback provider + PTY coverage. No production Agent mocks.

const PLAN_TUI_BODY: &str = "# Create canary\n\nCreate canary.txt with the exact bytes `approved bytes`.\n\n- [ ] Create canary.txt and read it back as approved bytes.\n- [ ] Verify the failure boundary without marking an unverified task complete.\n";

fn plan_start(server: &Server, home: &Path, project: &Path, protocol: &str) -> Pty {
    std::fs::write(
        config_path(home, "settings.yml"),
        r#"{"default_model":"fixture/primary","notification_timeout_seconds":1}"#,
    )
    .unwrap();
    std::fs::write(
        config_path(home, "models.yml"),
        response_models(server, Some(protocol)),
    )
    .unwrap();
    let fake = project.join(".plan-fake-bin");
    if fake.is_dir() {
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_pp"));
        command.cwd(project);
        command.env("HOME", home);
        let mut paths = vec![fake];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command.env("PATH", std::env::join_paths(paths).unwrap());
        Pty::spawn(command)
    } else {
        Pty::start_with_args(project, home, &[])
    }
}

fn plan_call(protocol: &str, id: &str, name: &str, arguments: Value) -> Value {
    if protocol == "completion" {
        call(id, name, arguments)
    } else {
        response_call(id, name, arguments)
    }
}

fn plan_calls(request: Request, protocol: &str, calls: Value) {
    if protocol == "completion" {
        calls_reply(request, calls);
    } else {
        response_reply(request, calls);
    }
}

fn plan_final(request: Request, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}

fn plan_wait_mode(pty: &mut Pty, mode: &str) {
    pty.wait_for("authoritative workflow mode", |screen| {
        screen
            .contents()
            .lines()
            .last()
            .is_some_and(|line| {
                ["off", "low", "med", "hig", "xhi", "max"].iter().any(|effort| {
                    line.contains(&format!("{effort} · \u{f0132} {mode} · "))
                })
            })
    });
}

fn plan_wait_execute(pty: &mut Pty) {
    pty.wait_for("执行模式隐藏模式标签", |screen| {
        screen.contents().lines().last().is_some_and(|line| {
            line.trim_start().starts_with("\u{f1d8} · ")
                && !line.contains('\u{f0132}')
                && !line.contains(" · EXEC · ")
                && !line.contains(" · E · ")
        })
    });
}

fn plan_enter(pty: &mut Pty, server: &Server) {
    pty.ready();
    plan_wait_execute(pty);
    pty.input("/plan\r");
    plan_wait_mode(pty, "Plan");
    no_more(server);
}

#[test]
fn plan_tui_prompt_enters_before_request_and_reuses_plan_mode() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut settings = super::mcp::settings("");
        settings["notification_timeout_seconds"] = json!(1);
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            settings.to_string(),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("/plan Research the login flow\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        plan_wait_mode(&mut pty, "Plan");
        cancel_mcp_assert_pid_stopped(&project.path().join("mcp-log.pid"));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["Research the login flow"]
        );
        let tools = body["tools"].as_array().unwrap();
        assert!(!tools.is_empty());
        for tool in tools {
            let name = if protocol == "completion" {
                &tool["function"]["name"]
            } else {
                &tool["name"]
            };
            assert!(
                matches!(
                    name.as_str(),
                    Some("read" | "grep" | "glob" | "ask" | "plan_write" | "plan_submit")
                ),
                "{body}"
            );
        }
        // 即使模型返回隐藏的写入调用，也必须已经启用 Plan 执行门禁。
        plan_calls(
            request,
            protocol,
            json!([plan_call(
                protocol,
                "deny-write",
                "write",
                json!({"path":"forbidden.txt","content":"wrong"})
            )]),
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_tool_outputs(&body, protocol).last().unwrap().1,
            json!({"ok":false,"error":"Tool is not available in Plan mode."})
        );
        assert!(!project.path().join("forbidden.txt").exists());
        plan_final(request, protocol, "Research complete");
        pty.wait_for("首个规划请求完成", |screen| {
            screen.contents().contains("Research complete")
        });
        pty.ready();
        pty.input("/plan Continue the research\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["Research the login flow", "Continue the research"]
        );
        plan_final(request, protocol, "Follow-up complete");
        pty.wait_for("追加规划请求完成", |screen| {
            screen.contents().contains("Follow-up complete")
        });
        pty.ready();
        no_more(&fixture);
        pty.input("\x03");
        pty.finish();
    }
}

fn plan_wait_review(pty: &mut Pty, revision: u64) {
    pty.wait_for("saved plan review panel", |screen| {
        let text = cancel_normalize(screen);
        text.contains(&format!("Plan·r{revision}")) && text.contains("Execute(keepcontext)")
    });
}

fn plan_path(project: &Path) -> std::path::PathBuf {
    let files: Vec<_> = std::fs::read_dir(project.join(".pp/plans"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    assert_eq!(files.len(), 1, "expected one saved plan: {files:?}");
    files.into_iter().next().unwrap()
}

fn plan_drive_ready(pty: &mut Pty, server: &Server, protocol: &str, task: &str, content: &str) {
    pty.input(&format!("{task}\r"));
    let (request, body) = pty.request_at(server, api_path(protocol));
    let names: Vec<_> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| {
            tool[if protocol == "completion" {
                "function"
            } else {
                "unused"
            }]["name"]
                .as_str()
                .or_else(|| tool["name"].as_str())
                .unwrap()
        })
        .collect();
    assert!(
        names.iter().all(|name| matches!(
            *name,
            "read"
                | "ask"
                | "grep"
                | "glob"
                | "plan_write"
                | "plan_submit"
        )),
        "{body}"
    );
    plan_calls(
        request,
        protocol,
        json!([
            plan_call(
                protocol,
                "plan-write",
                "plan_write",
                json!({"title":"Create canary","content":content,"expected_revision":0})
            ),
            plan_call(
                protocol,
                "plan-submit",
                "plan_submit",
                json!({"expected_revision":1})
            ),
            plan_call(
                protocol,
                "sealed-shell",
                "bash",
                json!({"command":"touch forbidden-after-submit"})
            ),
            plan_call(
                protocol,
                "sealed-write",
                "plan_write",
                json!({"title":"Wrong","content":"- [ ] wrong","expected_revision":1})
            )
        ]),
    );
    plan_wait_review(pty, 1);
    no_more(server);
}

fn plan_choose(pty: &mut Pty, action: usize) {
    pty.input("\t");
    match action {
        0 => pty.input("\x1b[A"),
        1 => {}
        2 => pty.input("\x1b[B"),
        3 => pty.input("\x1b[B\x1b[B"),
        _ => panic!("invalid Plan action"),
    }
    pty.input("\r");
}

#[test]
fn plan_tui_approval_uses_reviewed_bytes_and_tracks_only_verified_progress() {
    for protocol in ["completion", "response"] {
        for fresh in [false, true] {
            let fixture = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
            plan_enter(&mut pty, &fixture);
            pty.input("Plan the canary ORIGIN_ONLY_MARKER\r");
            let (request, _) = pty.request_at(&fixture, api_path(protocol));
            plan_final(
                request,
                protocol,
                "EXPLORATION_ONLY_MARKER: verified current code",
            );
            pty.ready();
            plan_drive_ready(
                &mut pty,
                &fixture,
                protocol,
                "Finalize the plan",
                PLAN_TUI_BODY,
            );
            let origin_id = saved_session_id(home.path());
            let path = plan_path(project.path());
            let approved = PLAN_TUI_BODY.replace("approved bytes", "reviewed bytes");
            std::fs::write(&path, &approved).unwrap();
            // The original review token cannot approve external bytes.
            plan_choose(&mut pty, usize::from(!fresh));
            plan_wait_review(&mut pty, 2);
            no_more(&fixture);
            assert!(!project.path().join("canary.txt").exists());
            plan_choose(&mut pty, usize::from(!fresh));
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            let encoded = body.to_string();
            assert!(encoded.contains("reviewed bytes"), "{body}");
            assert!(encoded.contains("[Approved Markdown snapshot]"), "{body}");
            assert_eq!(encoded.contains("ORIGIN_ONLY_MARKER"), !fresh, "{body}");
            assert_eq!(
                encoded.contains("EXPLORATION_ONLY_MARKER"),
                !fresh,
                "{body}"
            );
            assert_eq!(
                cancel_user_messages(&body, protocol).len(),
                if fresh { 1 } else { 3 }
            );
            let tools = body["tools"].as_array().unwrap();
            assert!(
                tools
                    .iter()
                    .any(|tool| tool["function"]["name"] == "write" || tool["name"] == "write")
            );
            assert!(
                !tools
                    .iter()
                    .any(|tool| tool["function"]["name"] == "plan_search"
                        || tool["name"] == "plan_search")
            );
            let ids = resume_boundary_session_ids(home.path());
            assert_eq!(ids.len(), if fresh { 2 } else { 1 });
            let execution_id = if fresh {
                ids.iter().find(|id| **id != origin_id).unwrap().clone()
            } else {
                origin_id.clone()
            };
            if fresh {
                assert!(cancel_tool_outputs(&body, protocol).is_empty());
                let source = plan_journal(home.path(), &origin_id);
                let workflow = source
                    .iter()
                    .rev()
                    .find(|record| record["customType"] == "pp.workflow")
                    .unwrap();
                assert_eq!(workflow["data"]["workflow"]["mode"], "plan");
                assert_eq!(workflow["data"]["workflow"]["plan"]["status"], "executed");
            } else {
                let outputs = cancel_tool_outputs(&body, protocol);
                assert_eq!(outputs.len(), 4);
                assert_eq!(
                    outputs[2].1["error"],
                    "Not executed: plan submitted for review."
                );
                assert_eq!(
                    outputs[3].1["error"],
                    "Not executed: plan submitted for review."
                );
            }
            plan_calls(
                request,
                protocol,
                json!([
                    plan_call(
                        protocol,
                        "execute-write",
                        "write",
                        json!({"path":"canary.txt","content":"reviewed bytes"})
                    ),
                    plan_call(
                        protocol,
                        "execute-read",
                        "read",
                        json!({"path":"canary.txt"})
                    )
                ]),
            );
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            assert_eq!(
                std::fs::read(project.path().join("canary.txt")).unwrap(),
                b"reviewed bytes"
            );
            assert_eq!(
                cancel_tool_outputs(&body, protocol).last().unwrap().1["content"],
                "reviewed bytes"
            );
            let relative = path.strip_prefix(project.path()).unwrap().to_str().unwrap();
            plan_calls(
                request,
                protocol,
                json!([plan_call(
                    protocol,
                    "execute-progress",
                    "edit",
                    json!({"path":relative,"edits":[{"oldText":"- [ ] Create canary.txt","newText":"- [x] Create canary.txt"}]})
                )]),
            );
            let (request, _) = pty.request_at(&fixture, api_path(protocol));
            // The provider is still holding an execution request while local cached view opens.
            pty.input("/plan view\r");
            pty.wait_for("cached verified execution progress", |screen| {
                let text = screen.contents();
                text.contains("1/2") && text.contains('☑') && text.contains('☐')
            });
            plan_wait_execute(&mut pty);
            no_more(&fixture);
            pty.input("\x1b");
            plan_calls(
                request,
                protocol,
                json!([plan_call(
                    protocol,
                    "execute-failure",
                    "bash",
                    json!({"command":"exit 7"})
                )]),
            );
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            assert_eq!(
                cancel_tool_outputs(&body, protocol).last().unwrap().1["exit_code"],
                7
            );
            pty.input("\x1b");
            cancel_wait_cancelled_ready(&mut pty);
            drop(request);
            let latest = std::fs::read_to_string(&path).unwrap();
            assert!(latest.contains("- [x] Create canary.txt"));
            assert!(latest.contains("- [ ] Verify the failure boundary"));
            let journal = plan_journal(home.path(), &execution_id);
            let approval = journal
                .iter()
                .filter(|record| record["type"] == "message" && record["message"]["role"] == "user")
                .find(|record| {
                    record["message"]["content"]
                        .as_str()
                        .is_some_and(|text| text.contains("[Approved Markdown snapshot]"))
                })
                .unwrap();
            assert!(
                approval["message"]["content"]
                    .as_str()
                    .unwrap()
                    .contains("- [ ] Create canary.txt")
            );
            assert!(
                !approval["message"]["content"]
                    .as_str()
                    .unwrap()
                    .contains("- [x] Create canary.txt")
            );
            pty.input("\x03");
            pty.finish();
            let mut resumed =
                Pty::start_with_args(project.path(), home.path(), &["-r", &execution_id]);
            resumed.ready();
            plan_wait_execute(&mut resumed);
            resumed.input("/plan view\r");
            resumed.wait_for("restored partial execution progress", |screen| {
                screen.contents().contains("1/2")
            });
            no_more(&fixture);
            resumed.input("\x03");
            resumed.finish();
        }
    }
}

fn plan_journal(home: &Path, id: &str) -> Vec<Value> {
    std::fs::read_to_string(home.join(".pp/agent/sessions").join(format!("{id}.jsonl")))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn plan_tui_rejects_real_side_effects_but_reads_searches_and_asks() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("canary.txt"), "original bytes").unwrap();
        std::fs::create_dir(project.path().join("src")).unwrap();
        std::fs::write(
            project.path().join("src/agent.rs"),
            "first\nneedle here\nlast\n",
        )
        .unwrap();
        let fake = project.path().join(".plan-fake-bin");
        std::fs::create_dir(&fake).unwrap();
        for name in ["rg", "fd"] {
            let path = fake.join(name);
            std::fs::write(&path, "#!/bin/sh\ntouch forbidden-native-search\nexit 1\n").unwrap();
            std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
        }
        std::fs::write(
            project.path().join(".ignore"),
            "# custom project ignore rules\nignored/\n-exec\n",
        )
        .unwrap();
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        plan_enter(&mut pty, &fixture);
        pty.input("Research only, never overwrite the canary\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let malicious = [
            (
                "deny-write",
                "write",
                json!({"path":"canary.txt","content":"wrong"}),
            ),
            (
                "deny-edit",
                "edit",
                json!({"path":"canary.txt","edits":[{"oldText":"original bytes","newText":"wrong"}]}),
            ),
            (
                "deny-bash",
                "bash",
                json!({"command":"touch forbidden-process"}),
            ),
            (
                "deny-background",
                "bash",
                json!({"command":"touch forbidden-process","mode":"background"}),
            ),
            (
                "deny-task",
                "bash_task",
                json!({"task_id":"unknown","action":"status"}),
            ),
            (
                "deny-grep",
                "plan_search",
                json!({"query":"needle","path":"src/agent.rs"}),
            ),
            (
                "deny-find",
                "find",
                json!({"args":[".","-exec","touch","forbidden-process",";"]}),
            ),
            (
                "deny-mcp",
                "mcp__unseen__write",
                json!({"path":"canary.txt","content":"wrong"}),
            ),
            ("research-read", "read", json!({"path":"src/agent.rs"})),
            (
                "research-search",
                "grep",
                json!({"pattern":"needle","path":"src"}),
            ),
            (
                "research-find",
                "glob",
                json!({"path":"src/agent.rs"}),
            ),
        ];
        plan_calls(
            request,
            protocol,
            Value::Array(
                malicious
                    .into_iter()
                    .map(|(id, name, args)| plan_call(protocol, id, name, args))
                    .collect(),
            ),
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs.len(), 11);
        for (_, result) in &outputs[..8] {
            assert_eq!(
                *result,
                json!({"ok":false,"error":"Tool is not available in Plan mode."})
            );
        }
        assert_eq!(outputs[8].1["content"], "first\nneedle here\nlast\n");
        assert!(outputs[9].1["content"].as_str().unwrap().contains("*2|needle here"));
        assert!(outputs[10].1["content"].as_str().unwrap().contains("agent.rs"));
        assert_eq!(
            std::fs::read(project.path().join("canary.txt")).unwrap(),
            b"original bytes"
        );
        assert!(!project.path().join("forbidden-process").exists());
        assert!(!project.path().join("forbidden-native-search").exists());
        assert!(!project.path().join(".pp-cache").exists());
        plan_calls(
            request,
            protocol,
            json!([plan_call(
                protocol,
                "research-ask",
                "ask",
                ask_single_arguments()
            )]),
        );
        ask_wait_panel(&mut pty, "Which database should the project use?");
        pty.input("\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_tool_outputs(&body, protocol).last().unwrap().1["ok"],
            true
        );
        plan_final(request, protocol, "Research done, no saved plan yet");
        pty.ready();
        plan_wait_mode(&mut pty, "Plan");
        pty.input("/quit\r");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn plan_tui_model_compaction_new_and_tree_never_restore_execute_authority() {
    for protocol in ["completion", "response"] {
        let other = if protocol == "completion" {
            "response"
        } else {
            "completion"
        };
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let base = format!("http://{}/v1", fixture.server_addr());
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            r#"{"default_model":"first/primary","notification_timeout_seconds":1}"#,
        )
        .unwrap();
        std::fs::write(config_path(home.path(),"models.yml"),json!({"providers":{
            "first": {"protocol":protocol,"api_key":"fixture-key","base_url":base,"models":{"primary": {"model":"first-model"}}},
            "second": {"protocol":other,"api_key":"fixture-key","base_url":base,"models":{"primary": {"model":"second-model"}}}}}).to_string()).unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("Execute history before entering Plan\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        plan_final(request, protocol, "Old Execute history");
        pty.ready();
        plan_enter(&mut pty, &fixture);
        plan_drive_ready(
            &mut pty,
            &fixture,
            protocol,
            "Plan before rewinding history",
            PLAN_TUI_BODY,
        );
        let path = plan_path(project.path());
        pty.input("\x1b");
        pty.wait_for("closing review for tree", |screen| {
            !screen.contents().contains("Execute (keep context)")
        });
        tree_tui_open(&mut pty, "plan_submit");
        tree_tui_rewind(
            &mut pty,
            &[TREE_TUI_HOME],
            "Execute history before entering Plan",
        );
        plan_wait_mode(&mut pty, "Plan");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), PLAN_TUI_BODY);
        pty.input("\x01\x0b/new\r");
        pty.wait_for("new session welcome after reset", |screen| {
            screen.contents().contains("▄▄▄██▀▀██") && input_is_ready(screen)
        });
        plan_wait_mode(&mut pty, "Plan");
        pty.input("/plan view\r");
        pty.text("No saved plan.");
        no_more(&fixture);
        pty.input("/model second/primary\r");
        pty.ready_model("second/primary");
        plan_wait_mode(&mut pty, "Plan");
        pty.input("Research after model change\r");
        let (request, body) = pty.request_at(&fixture, api_path(other));
        assert!(body.to_string().contains("PLAN MODE"));
        plan_calls(
            request,
            other,
            json!([plan_call(
                other,
                "switched-denied",
                "write",
                json!({"path":"unauthorized","content":"wrong"})
            )]),
        );
        let (request, body) = pty.request_at(&fixture, api_path(other));
        assert_eq!(
            cancel_tool_outputs(&body, other).last().unwrap().1["error"],
            "Tool is not available in Plan mode."
        );
        plan_final(request, other, &"old static evidence ".repeat(500));
        pty.ready();
        pty.input("Second research exchange\r");
        let (request, _) = pty.request_at(&fixture, api_path(other));
        plan_final(request, other, "Recent verified static facts");
        pty.ready();
        pty.input("/compact\r");
        let (request, body) = pty.request_at(&fixture, api_path(other));
        assert!(
            body["tools"].is_null() || body["tools"].as_array().is_some_and(Vec::is_empty),
            "{body}"
        );
        plan_final(
            request,
            other,
            "Static research summary; implementation remains unapproved.",
        );
        pty.wait_for("manual Plan compaction finished", |screen| {
            screen.contents().contains("Context compacted:")
        });
        pty.ready();
        plan_wait_mode(&mut pty, "Plan");
        pty.input("Research after compaction\r");
        let (request, body) = pty.request_at(&fixture, api_path(other));
        assert!(body.to_string().contains("PLAN MODE"));
        plan_calls(
            request,
            other,
            json!([plan_call(
                other,
                "compact-denied",
                "bash",
                json!({"command":"touch unauthorized"})
            )]),
        );
        let (request, body) = pty.request_at(&fixture, api_path(other));
        assert_eq!(
            cancel_tool_outputs(&body, other).last().unwrap().1["error"],
            "Tool is not available in Plan mode."
        );
        plan_final(request, other, "Still Plan after every lifecycle change");
        pty.ready();
        assert!(!project.path().join("unauthorized").exists());
        assert_eq!(std::fs::read_to_string(path).unwrap(), PLAN_TUI_BODY);
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn plan_tui_mcp_is_stopped_in_plan_and_cleanup_tracks_execute_reconnection() {
    for protocol in ["completion", "response"] {
        for off in [false, true] {
            let fixture = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let mut settings = super::mcp::settings("");
            settings["notification_timeout_seconds"] = json!(1);
            std::fs::write(
                config_path(home.path(), "settings.yml"),
                settings.to_string(),
            )
            .unwrap();
            std::fs::write(
                config_path(home.path(), "models.yml"),
                response_models(&fixture, Some(protocol)),
            )
            .unwrap();
            let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
            pty.ready();
            plan_enter(&mut pty, &fixture);
            cancel_mcp_assert_pid_stopped(&project.path().join("mcp-log.pid"));
            plan_drive_ready(
                &mut pty,
                &fixture,
                protocol,
                "Plan before MCP reconnection",
                PLAN_TUI_BODY,
            );
            assert_eq!(
                cancel_mcp_log(project.path())
                    .iter()
                    .filter(|entry| entry["method"] == "initialize")
                    .count(),
                1
            );
            let id = saved_session_id(home.path());
            pty.input("\x03");
            pty.finish();
            let mut pty = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
            pty.ready();
            plan_wait_mode(&mut pty, "Plan");
            no_more(&fixture);
            pty.input("/plan\r");
            plan_wait_review(&mut pty, 1);
            no_more(&fixture);
            assert_eq!(
                cancel_mcp_log(project.path())
                    .iter()
                    .filter(|entry| entry["method"] == "initialize")
                    .count(),
                1
            );
            if off {
                pty.input("\x1b");
                pty.wait_for("closing review before off", |screen| {
                    !screen.contents().contains("Execute (keep context)")
                });
                pty.input("/plan off\r");
                pty.wait_for("local leave confirmation", |screen| {
                    screen
                        .contents()
                        .contains("Leave Plan mode without executing this plan?")
                });
                pty.input("\r");
                plan_wait_mode(&mut pty, "Plan");
                no_more(&fixture);
                pty.input("/plan off\r");
                pty.wait_for("local leave confirmation again", |screen| {
                    screen
                        .contents()
                        .contains("Leave Plan mode without executing this plan?")
                });
                pty.input("\x1b[B\r");
                plan_wait_execute(&mut pty);
                no_more(&fixture);
                pty.input("Explicit normal execution after leaving Plan\r");
            } else {
                plan_choose(&mut pty, 1);
            }
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            assert!(
                body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["function"]["name"] == "mcp__local__echo"
                        || tool["name"] == "mcp__local__echo"),
                "{body}"
            );
            assert_eq!(
                cancel_mcp_log(project.path())
                    .iter()
                    .filter(|entry| entry["method"] == "initialize")
                    .count(),
                2
            );
            plan_calls(
                request,
                protocol,
                json!([
                    plan_call(
                        protocol,
                        "execute-mcp",
                        "mcp__local__echo",
                        json!({"text":"approved tool call"})
                    ),
                    plan_call(
                        protocol,
                        "mcp-canary",
                        "write",
                        json!({"path":"canary.txt","content":"MCP handoff executed"})
                    )
                ]),
            );
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            let outputs = cancel_tool_outputs(&body, protocol);
            let mcp = outputs.iter().find(|(id, _)| id == "execute-mcp").unwrap();
            assert_eq!(mcp.1["ok"], true);
            assert_eq!(mcp.1["result"]["content"][0]["text"], "中文结果");
            assert_eq!(
                std::fs::read(project.path().join("canary.txt")).unwrap(),
                b"MCP handoff executed"
            );
            pty.input("\x03");
            pty.finish();
            drop(request);
            cancel_mcp_assert_pid_stopped(&project.path().join("mcp-log.pid"));
        }
    }
}

#[test]
fn plan_tui_external_file_errors_never_restore_or_execute_snapshot_bytes() {
    for protocol in ["completion", "response"] {
        for fault in [
            "missing",
            "symlink",
            "directory",
            "invalid_utf8",
            "oversized",
        ] {
            let fixture = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
            plan_enter(&mut pty, &fixture);
            plan_drive_ready(
                &mut pty,
                &fixture,
                protocol,
                "Plan before external file error",
                PLAN_TUI_BODY,
            );
            let path = plan_path(project.path());
            match fault {
                "missing" => std::fs::remove_file(&path).unwrap(),
                "symlink" => {
                    std::fs::write(project.path().join("outside-plan"), PLAN_TUI_BODY).unwrap();
                    std::fs::remove_file(&path).unwrap();
                    std::os::unix::fs::symlink(project.path().join("outside-plan"), &path).unwrap();
                }
                "directory" => {
                    std::fs::remove_file(&path).unwrap();
                    std::fs::create_dir(&path).unwrap();
                }
                "invalid_utf8" => std::fs::write(&path, b"\xff\xfe").unwrap(),
                "oversized" => std::fs::write(&path, vec![b'x'; 262_145]).unwrap(),
                _ => unreachable!(),
            }
            plan_choose(&mut pty, 1);
            pty.wait_for("file validation failure keeps planning", |screen| {
                let text = cancel_normalize(screen);
                text.contains("Plan")
                    && (text.contains("UTF-8")
                        || text.contains("regularfile")
                        || text.contains("Nosuchfile")
                        || text.contains("toolarge")
                        || text.contains("exceeds"))
            });
            no_more(&fixture);
            assert!(!project.path().join("canary.txt").exists());
            match fault {
                "missing" => assert!(!path.exists()),
                "symlink" => assert!(
                    std::fs::symlink_metadata(&path)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                ),
                "directory" => assert!(path.is_dir()),
                "invalid_utf8" => assert_eq!(std::fs::read(&path).unwrap(), b"\xff\xfe"),
                "oversized" => assert_eq!(std::fs::metadata(&path).unwrap().len(), 262_145),
                _ => unreachable!(),
            }
            pty.input("\x03");
            pty.finish();
        }
    }
}

#[test]
fn plan_tui_pending_input_never_crosses_review_into_execute() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        plan_enter(&mut pty, &fixture);
        pty.input("Plan with pending input\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        pty.input("/plan off\r");
        pty.wait_for("busy mode switch refused", |screen| {
            cancel_normalize(screen).contains("CancelthecurrenttaskbeforechangingPlanmode.")
        });
        pty.input("\x01\x0bsteering requirement\r");
        runtime_input_counts(&mut pty, 1, 0, false);
        plan_calls(
            request,
            protocol,
            json!([
                plan_call(
                    protocol,
                    "pending-write",
                    "plan_write",
                    json!({"title":"Pending","content":PLAN_TUI_BODY,"expected_revision":0})
                ),
                plan_call(
                    protocol,
                    "pending-submit",
                    "plan_submit",
                    json!({"expected_revision":1})
                )
            ]),
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_tool_outputs(&body, protocol).last().unwrap().1["error"],
            "Apply pending input before submitting the plan."
        );
        assert_eq!(
            cancel_user_messages(&body, protocol).last().unwrap(),
            "steering requirement"
        );
        pty.input("followup research\x1b\r");
        runtime_input_counts(&mut pty, 0, 1, false);
        plan_calls(
            request,
            protocol,
            json!([plan_call(
                protocol,
                "pending-submit-again",
                "plan_submit",
                json!({"expected_revision":1})
            )]),
        );
        plan_wait_review(&mut pty, 1);
        no_more(&fixture);
        plan_choose(&mut pty, 1);
        no_more(&fixture);
        assert!(!project.path().join("canary.txt").exists());
        pty.input("\x1b");
        pty.wait_for("Plan panel closed before queue resume", |screen| {
            !screen.contents().contains("Execute (keep context)")
                && cancel_cursor_on_input_row(screen)
        });
        pty.function_key('r');
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol).last().unwrap(),
            "followup research"
        );
        plan_calls(
            request,
            protocol,
            json!([plan_call(
                protocol,
                "followup-denied",
                "write",
                json!({"path":"canary.txt","content":"unauthorized"})
            )]),
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_tool_outputs(&body, protocol).last().unwrap().1["error"],
            "Tool is not available in Plan mode."
        );
        plan_final(request, protocol, "Followup researched without execution");
        pty.ready();
        plan_wait_mode(&mut pty, "Plan");
        assert!(!project.path().join("canary.txt").exists());
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn plan_tui_discussion_preserves_main_draft_and_does_not_force_a_rewrite() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        plan_enter(&mut pty, &fixture);
        pty.input("Plan canary with discussion\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        pty.input("preserved main draft");
        pty.draft("preserved main draft");
        plan_calls(
            request,
            protocol,
            json!([
                plan_call(
                    protocol,
                    "discuss-write",
                    "plan_write",
                    json!({"title":"Canary","content":PLAN_TUI_BODY,"expected_revision":0})
                ),
                plan_call(
                    protocol,
                    "discuss-submit",
                    "plan_submit",
                    json!({"expected_revision":1})
                )
            ]),
        );
        plan_wait_review(&mut pty, 1);
        plan_choose(&mut pty, 2);
        pty.input("discard this question\x1b");
        plan_wait_review(&mut pty, 1);
        no_more(&fixture);
        pty.input("\r");
        pty.input("Why is file creation before failure verification?\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(
            cancel_user_messages(&body, protocol)
                .last()
                .unwrap()
                .contains("Why is file creation before failure verification?")
        );
        assert!(body.to_string().contains("Plan canary with discussion"));
        plan_final(
            request,
            protocol,
            "The first step establishes the file; the second checks failures. No rewrite needed.",
        );
        pty.ready();
        pty.draft("preserved main draft");
        plan_wait_mode(&mut pty, "Plan");
        assert_eq!(
            std::fs::read_to_string(plan_path(project.path())).unwrap(),
            PLAN_TUI_BODY
        );
        let id = saved_session_id(home.path());
        let journal = plan_journal(home.path(), &id);
        let workflow = journal
            .iter()
            .rev()
            .find(|record| record["customType"] == "pp.workflow")
            .unwrap();
        assert_eq!(workflow["data"]["workflow"]["plan"]["status"], "draft");
        assert_eq!(workflow["data"]["workflow"]["plan"]["revision"], 1);
        assert!(!project.path().join("canary.txt").exists());
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn plan_tui_submission_requires_deliberate_action_and_survives_resize_and_resume() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        plan_enter(&mut pty, &fixture);
        plan_drive_ready(
            &mut pty,
            &fixture,
            protocol,
            "Plan the canary",
            PLAN_TUI_BODY,
        );
        assert_eq!(
            std::fs::read_to_string(plan_path(project.path())).unwrap(),
            PLAN_TUI_BODY
        );
        assert!(!project.path().join("forbidden-after-submit").exists());
        assert!(!project.path().join("canary.txt").exists());
        let text = pty.parser.screen().contents();
        let fresh = text.find("Execute (plan only)").unwrap();
        let keep = text.find("Execute (keep context)").unwrap();
        let discuss = text.find("Continue discussing Plan").unwrap();
        let review = text.find("Review Plan").unwrap();
        assert!(fresh < keep && keep < discuss && discuss < review, "{text}");
        assert!(text.contains('☐'), "{text}");
        pty.input("\r");
        no_more(&fixture);
        pty.resize(4, 20);
        pty.wait_for("narrow mode indicator", |screen| {
            screen.contents().contains("Plan")
                || screen
                    .contents()
                    .lines()
                    .last()
                    .is_some_and(|line| line.contains('P'))
        });
        pty.resize(30, 100);
        plan_wait_review(&mut pty, 1);
        pty.input("\x1b");
        plan_wait_mode(&mut pty, "Plan");
        pty.input("开始吧\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(
            cancel_user_messages(&body, protocol)
                .last()
                .unwrap()
                .contains("开始吧")
        );
        plan_final(request, protocol, "Still discussing, not executing");
        pty.ready();
        assert!(!project.path().join("canary.txt").exists());
        plan_wait_mode(&mut pty, "Plan");
        let id = saved_session_id(home.path());
        pty.input("\x03");
        pty.finish();
        for flag in ["-r", "-c"] {
            let mut resumed = Pty::start_with_args(project.path(), home.path(), &[flag, &id]);
            resumed.ready();
            plan_wait_mode(&mut resumed, "Plan");
            no_more(&fixture);
            resumed.input("/quit\r");
            resumed.finish();
        }
    }
}
