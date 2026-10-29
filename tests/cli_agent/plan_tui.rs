// Real binary + loopback provider + PTY coverage. No production Agent mocks.

const PLAN_TUI_BODY: &str = "# Create canary\n\nCreate canary.txt with the exact bytes `approved bytes`.\n\n- [ ] Create canary.txt and read it back as approved bytes.\n- [ ] Verify the failure boundary without marking an unverified task complete.\n";

fn plan_start(server: &Server, home: &Path, project: &Path, protocol: &str) -> Pty {
    std::fs::write(
        config_path(home, "settings.yml"),
        r#"{"default_model":"fixture/primary","notification_timeout_seconds":1,"rate_limit_retry_budget_seconds":0}"#,
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
        screen.contents().lines().last().is_some_and(|line| {
            ["off", "low", "med", "hig", "xhi", "max"]
                .iter()
                .any(|effort| line.contains(&format!("{effort} · \u{f0132} {mode} · ")))
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




fn plan_wait_review(pty: &mut Pty, revision: u64) {
    pty.wait_for("saved plan review panel", |screen| {
        let text = cancel_normalize(screen);
        text.contains(&format!("Plan·r{revision}")) && text.contains("Execute(keepcontext)")
    });
}

fn plan_path(project: &Path) -> std::path::PathBuf {
    let files: Vec<_> = ["drafts", "plans", "done"]
        .into_iter()
        .flat_map(|location| {
            std::fs::read_dir(project.join(".pp").join(location))
                .into_iter()
                .flatten()
                .map(|entry| entry.unwrap().path())
        })
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name == "create-canary.md")
        })
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
            "read" | "ask" | "grep" | "glob" | "plan_write" | "plan_submit"
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
                json!({"title":"Create canary","file_name":"create-canary.md","content":content,"expected_revision":0})
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
                json!({"title":"Wrong","file_name":"create-canary.md","content":"- [ ] wrong","expected_revision":1})
            )
        ]),
    );
    plan_wait_review(pty, 1);
    no_more(server);
}

fn plan_choose(pty: &mut Pty, action: usize) {
    assert!(action < 6);
    pty.input(&format!("{}\r", action + 1));
}




fn plan_journal(home: &Path, id: &str) -> Vec<Value> {
    std::fs::read_to_string(home.join(".pp/agent/sessions").join(format!("{id}.jsonl")))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
// plan_*_cases 子模块由 cli_agent/tui.rs 注册；本文件只保留共享 helper。













































