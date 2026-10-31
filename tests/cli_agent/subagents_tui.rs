// Agent Hub（Agents 总览 / Activity / 独立 child 详情）的真实 PTY 回归。
// 消息始终按原文发送；观察线程与 supervisor 的所有栅栏都用屏幕内容或 HTTP 请求，
// 不用固定 sleep 推测 UI 已经就绪。

/// 启动时可能同时存在 pending 目录与真实 session 目录，取已登记 Agent 最多的那份。
fn hub_dir(home: &std::path::Path) -> std::path::PathBuf {
    let mut best: Option<(usize, std::path::PathBuf)> = None;
    for directory in [".pp/agent/sessions", ".pp/run/sessions"] {
        let dir = home.join(directory);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("subagents") {
                continue;
            }
            let agents = std::fs::read(path.join("registry.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .and_then(|value| value["agents"].as_object().map(|agents| agents.len()))
                .unwrap_or(0);
            if best.as_ref().is_none_or(|(count, _)| agents >= *count) {
                best = Some((agents, path));
            }
        }
    }
    best.map(|(_, path)| path).expect("subagent root directory")
}

fn hub_registry(home: &std::path::Path) -> Value {
    let bytes = std::fs::read(hub_dir(home).join("registry.json")).expect("registry file");
    serde_json::from_slice(&bytes).expect("registry json")
}

fn hub_append(home: &std::path::Path, agent: &str, records: &[Value]) {
    use std::io::Write;
    let path = hub_dir(home).join(format!("{agent}.events.jsonl"));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("event log");
    for record in records {
        writeln!(file, "{record}").expect("append record");
    }
    file.flush().expect("flush record");
}

/// 两种协议的 tool call 形状不同，派发与工具回复都必须按协议构造。
fn hub_call(protocol: &str, id: &str, name: &str, args: Value) -> Value {
    if protocol == "completion" {
        call(id, name, args)
    } else {
        response_call(id, name, args)
    }
}

fn hub_body_text(body: &Value) -> String {
    body.to_string()
}

fn hub_is_child(body: &Value) -> bool {
    hub_body_text(body).contains("You are subagent")
}

/// 派发一批子 Agent，按 task 文本归位各自的第一个模型请求。
fn hub_dispatch(
    pty: &mut Pty,
    fixture: &Server,
    protocol: &str,
    tasks: Value,
    needles: &[&str],
) -> Vec<Request> {
    let (request, _) = pty.request_at(fixture, api_path(protocol));
    ask_reply_calls(
        request,
        protocol,
        json!([hub_call(protocol, "task", "task", tasks)]),
    );
    let mut found: Vec<Option<Request>> = needles.iter().map(|_| None).collect();
    // 父方在子 Agent pending 期间会自动继续，必须消费它紧随其后的请求。
    let mut parent_answered = false;
    while found.iter().any(Option::is_none) || !parent_answered {
        let (request, body) = pty.request_at(fixture, api_path(protocol));
        if !hub_is_child(&body) {
            assert!(!parent_answered, "unexpected second parent request");
            ask_final_reply(request, protocol, "Waiting for hub children.");
            parent_answered = true;
            continue;
        }
        let text = hub_body_text(&body);
        let index = needles
            .iter()
            .position(|needle| text.contains(needle))
            .expect("known child task");
        found[index] = Some(request);
    }
    found.into_iter().map(Option::unwrap).collect()
}

/// 结束一个子 Agent 的首轮输出，并消费父方 autowake 请求。
fn hub_finish_child(pty: &mut Pty, fixture: &Server, protocol: &str, child: Request, text: &str) {
    ask_final_reply(child, protocol, text);
    let (request, body) = pty.request_at(fixture, api_path(protocol));
    assert!(!hub_is_child(&body), "unexpected child request");
    ask_final_reply(request, protocol, "parent-ack");
}

fn hub_last_user(body: &Value) -> Option<String> {
    let items = body
        .get("messages")
        .or_else(|| body.get("input"))?
        .as_array()?;
    let last = items
        .iter()
        .rev()
        .find(|item| item["role"] == "user")?;
    match &last["content"] {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => parts
            .iter()
            .find_map(|part| part["text"].as_str())
            .map(str::to_owned),
        _ => None,
    }
}

fn hub_count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// 单发 Esc 并等到目标页面出现，避免它与下一按键合并成 Alt 序列。
fn hub_escape(pty: &mut Pty, expect: &str) {
    pty.input("\x1b");
    pty.text(expect);
}

/// 清掉 Agents 过滤：Esc 先清过滤，再关闭。
fn hub_clear_filter(pty: &mut Pty, expect: &str) {
    pty.input("\x1b");
    pty.text(expect);
}

/// 关闭 Hub：Esc 后主输入提示符必须恢复。
fn hub_close(pty: &mut Pty) {
    pty.input("\x1b");
    pty.ready();
}

#[test]
fn subagent_tui_inspects_cancels_revives_and_sends_literal_followups() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        // `/agents` 仍是角色定义只读页。
        pty.input("/agents\r");
        pty.text("scout");
        pty.text("bundled");
        pty.input("\x1b");
        pty.ready();
        pty.input("delegate terminal task\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let task = json!({"context":"terminal context","tasks":[{"task":"child terminal work"}]});
        let call_value = if protocol == "completion" {
            call("task", "task", task)
        } else {
            response_call("task", "task", task)
        };
        ask_reply_calls(request, protocol, json!([call_value]));
        let mut child = None;
        let mut parent = false;
        while child.is_none() || !parent {
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            if hub_is_child(&body) {
                child = Some(request);
            } else {
                ask_final_reply(request, protocol, "Waiting for subagent.");
                parent = true;
            }
        }
        pty.input("/subagents\r");
        pty.text("a1");
        pty.input("c");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(hub_body_text(&body).contains("cancelled"));
        drop(child.take());
        pty.input("\x1b");
        ask_final_reply(request, protocol, "child-cancelled");
        pty.text("child-cancelled");
        pty.input("/subagents\r");
        pty.text("a1");
        pty.input("r");
        pty.input("e/model hidden\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(hub_is_child(&body));
        assert!(hub_body_text(&body).contains("/model hidden"));
        assert_eq!(body["model"], "fixture-model");
        ask_final_reply(request, protocol, "child-followup-answer");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(hub_body_text(&body).contains("child-followup-answer"));
        ask_final_reply(request, protocol, "parent-followup-answer");
        pty.input("\x1b");
        pty.text("1 Agents");
        pty.input("\x1b");
        pty.ready();
        pty.text("parent-followup-answer");
        pty.input("/subagents\r");
        pty.text("a1");
        pty.input("x");
        pty.text("aborted");
        pty.input("\x1b");
        pty.ready();
        pty.input("\x03");
        pty.finish();
    }
}

#[test]
fn agent_hub_streams_and_anchors_activity() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("probe.txt"), "hub-probe-body").unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("delegate hub stream task\r");
        let mut children = hub_dispatch(
            &mut pty,
            &fixture,
            protocol,
            json!({"context":"hub context","tasks":[
                {"name":"LiveWriter","task":"stream the two hub chunks in order"},
                {"name":"ToolFailure","task":"read probe.txt then run the failing command"}
            ]}),
            &["the two hub chunks", "probe.txt"],
        );
        // LiveWriter 保持 SSE 打开；ToolFailure 走 失败工具 + 后续成功回复。
        let mut live = SseReply::new(children.remove(0));
        ask_reply_calls(
            children.remove(0),
            protocol,
            json!([hub_call(
                protocol,
                "read-probe",
                "read",
                json!({"path":"probe.txt"})
            )]),
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(hub_body_text(&body).contains("hub-probe-body"));
        ask_reply_calls(
            request,
            protocol,
            json!([hub_call(
                protocol,
                "fail-bash",
                "bash",
                json!({"command":"printf hub-tool-fail; exit 3"})
            )]),
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(hub_body_text(&body).contains("hub-tool-fail"));
        ask_final_reply(request, protocol, "hub-recovered");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        ask_final_reply(request, protocol, "hub-parent-ok");
        pty.text("hub-parent-ok");
        // 先发第一段，再进入详情：无需额外按键即可看到。
        streaming_delta(&mut live, protocol, "hub-stream-first");
        pty.input("/subagents\r");
        pty.text("LiveWriter");
        pty.text("ToolFailure");
        pty.input("\r");
        pty.text("hub-stream-first");
        // 第二段只追加一次。
        // 分片拼接必须与最终文本一致，否则 Responses 协议会判定冲突。
        streaming_delta(&mut live, protocol, " hub-stream-second");
        pty.text("hub-stream-second");
        assert_eq!(hub_count(&pty.contents(), "hub-stream-second"), 1);
        streaming_complete(&mut live, protocol, "hub-stream-first hub-stream-second");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        ask_final_reply(request, protocol, "hub-parent-stream-done");
        // Activity：失败只出现在 Errors。
        hub_escape(&mut pty, "j/k select");
        pty.input("2");
        pty.text("All agents");
        pty.input("f");
        pty.text("Errors");
        let screen = pty.contents();
        assert_eq!(hub_count(&screen, "✖"), 1, "{screen}");
        // Enter 定位到失败工具卡片。
        pty.input("\r");
        pty.text("Agent Hub");
        pty.text("printf hub-tool-fail");
        // Esc 回到原 scope/search/选中活动。
        hub_escape(&mut pty, "Errors");
        assert_eq!(hub_count(&pty.contents(), "✖"), 1);
        hub_escape(&mut pty, "j/k select");
        hub_close(&mut pty);
        pty.input("\x03");
        pty.finish();
    }
}

#[test]
fn agent_hub_literal_input_drafts_and_parent_isolation() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("delegate drafts task\r");
        let mut children = hub_dispatch(
            &mut pty,
            &fixture,
            protocol,
            json!({"context":"draft context","tasks":[
                {"name":"First","task":"child-one work"},
                {"name":"Second","task":"child-two work"}
            ]}),
            &["child-one", "child-two"],
        );
        hub_finish_child(&mut pty, &fixture, protocol, children.remove(0), "hub-first-output");
        hub_finish_child(&mut pty, &fixture, protocol, children.remove(0), "hub-second-output");
        // 主输入草稿必须贯穿整个 Hub 交互。
        pty.input("main draft text");
        pty.draft("main draft text");
        // 同批次排名按最近活动：以屏幕为准推断哪一路是首行。
        pty.input("\x1ba");
        pty.text("First");
        pty.text("Second");
        let screen = pty.contents();
        let (lead, lead_id, other, other_id) = if screen.find("First").unwrap_or(usize::MAX)
            < screen.find("Second").unwrap_or(usize::MAX)
        {
            ("First", "a1", "Second", "a2")
        } else {
            ("Second", "a2", "First", "a1")
        };
        let lead_output = if lead == "First" { "hub-first-output" } else { "hub-second-output" };
        pty.input("e");
        pty.text(&format!("{lead} / {lead_id}"));
        pty.input("jkgGcxr/model hidden");
        pty.input("\x0a");
        pty.input("第二行");
        pty.input("\x1b[200~ pasted-text\x1b[201~");
        pty.text("pasted-text");
        // Esc 保留草稿返回来源页，再 Esc 关闭 Hub。
        hub_escape(&mut pty, "j/k select");
        hub_close(&mut pty);
        pty.draft("main draft text");
        // Alt+A 重开后草稿仍在。
        pty.input("\x1ba");
        pty.text(lead);
        pty.input("e");
        pty.text(&format!("{lead} / {lead_id}"));
        pty.text("jkgGcxr/model hidden");
        pty.text("第二行");
        // 另一个 child 不共享草稿。
        hub_escape(&mut pty, "j/k select");
        pty.input("j");
        pty.input("e");
        pty.text(&format!("{other} / {other_id}"));
        assert!(
            !pty.contents().contains("jkgGcxr"),
            "child drafts must not leak between agents"
        );
        // 回到首个 child，发送原文字面量。
        hub_escape(&mut pty, "j/k select");
        pty.input("k");
        pty.input("e");
        pty.text(&format!("{lead} / {lead_id}"));
        pty.text("jkgGcxr/model hidden");
        pty.input("\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            hub_last_user(&body).as_deref(),
            Some("jkgGcxr/model hidden\n第二行 pasted-text")
        );
        assert_eq!(body["model"], "fixture-model");
        // 新 Run（sequence 重新计数）的后续输出与旧输出同时可见。
        ask_final_reply(request, protocol, "hub-followup-ok");
        pty.text("hub-followup-ok");
        pty.text(lead_output);
        assert_eq!(hub_count(&pty.contents(), "jkgGcxr/model hidden"), 1);
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(hub_body_text(&body).contains("hub-followup-ok"));
        ask_final_reply(request, protocol, "parent-followup-ok");
        hub_escape(&mut pty, "j/k select");
        hub_close(&mut pty);
        pty.draft("main draft text");
        pty.text("parent-followup-ok");
        pty.input("\x03");
        pty.finish();
    }
}

#[test]
fn agent_hub_controls_do_not_revive_on_inspect() {
    let protocol = "completion";
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        response_models(&fixture, Some(protocol)),
    )
    .unwrap();
    let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
    pty.ready();
    pty.input("delegate controls task\r");
    let children = hub_dispatch(
        &mut pty,
        &fixture,
        protocol,
        json!({"context":"controls context","tasks":[{"task":"child controls work"}]}),
        &["child controls work"],
    );
    let _running = children;
    let before = hub_registry(home.path());
    let generation = before["agents"]["a1"]["generation"].as_u64().unwrap();
    // 打开详情不 revive、不发模型请求。
    pty.input("/subagents\r");
    pty.text("a1");
    pty.input("\r");
    pty.text("Agent Hub");
    no_more(&fixture);
    assert_eq!(
        hub_registry(home.path())["agents"]["a1"]["generation"].as_u64(),
        Some(generation)
    );
    // Esc 只返回，不取消 root 或 child。
    pty.input("\x1b");
    pty.text("1 Agents");
    no_more(&fixture);
    // c 取消 Run：不置 aborted。
    pty.input("c");
    let (request, body) = pty.request_at(&fixture, api_path(protocol));
    assert!(hub_body_text(&body).contains("cancelled"));
    let cancelled = hub_registry(home.path());
    assert_ne!(cancelled["agents"]["a1"]["status"], "aborted");
    ask_final_reply(request, protocol, "cancel-ack");
    // Hub 打开时主对话不在屏幕上，取消后的表现以 registry 与运行状态为准。
    pty.text("Run cancelled");
    // r 对 idle 为空操作：不启动新 Run，也不发模型请求。
    let idle = hub_registry(home.path())["agents"]["a1"]["generation"]
        .as_u64()
        .unwrap();
    pty.input("r");
    no_more(&fixture);
    assert_eq!(
        hub_registry(home.path())["agents"]["a1"]["generation"].as_u64(),
        Some(idle)
    );
    // x 立即 aborted，generation 保护生效。
    pty.input("x");
    pty.wait_for("aborted status", |_| {
        hub_registry(home.path())["agents"]["a1"]["status"] == "aborted"
    });
    let aborted = hub_registry(home.path());
    assert_eq!(aborted["agents"]["a1"]["status"], "aborted");
    let killed = aborted["agents"]["a1"]["generation"].as_u64().unwrap();
    assert!(killed > generation);
    // r 才恢复已中止的 Agent。
    // kill 之后 worker 退出是异步的：在有限窗口内重发 r，直到恢复真实生效。
    let deadline = Instant::now() + DEADLINE;
    loop {
        pty.input("r");
        pty.text("Agent Hub");
        if hub_registry(home.path())["agents"]["a1"]["generation"]
            .as_u64()
            .is_some_and(|current| current > killed)
        {
            break;
        }
        assert!(Instant::now() < deadline, "revive never took effect");
        thread::sleep(Duration::from_millis(50));
    }
    pty.input("\x1b");
    pty.ready();
    // Esc 关闭 Hub 不取消 root：仍然可以正常结束。
    pty.input("\x03");
    pty.finish();
}

#[test]
fn agent_hub_layout_tree_scope_and_follow() {
    let protocol = "completion";
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        response_models(&fixture, Some(protocol)),
    )
    .unwrap();
    let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
    pty.ready();
    pty.input("delegate layout task\r");
    let mut children = hub_dispatch(
        &mut pty,
        &fixture,
        protocol,
        json!({"context":"layout context","tasks":[
            {"name":"Alpha","task":"alpha child work"},
            {"name":"Beta","task":"beta child work"}
        ]}),
        &["alpha child work", "beta child work"],
    );
    hub_finish_child(&mut pty, &fixture, protocol, children.remove(0), "alpha-output");
    hub_finish_child(&mut pty, &fixture, protocol, children.remove(0), "beta-output");
    // 140x40 分栏 + 80x24 单栏 + 极窄尺寸都不能越界。
    pty.resize(40, 140);
    pty.input("/subagents\r");
    pty.text("Alpha");
    let wide = pty.contents();
    assert!(wide.contains("Usage / Context") || wide.contains("Lineage"), "{wide}");
    pty.resize(24, 80);
    let narrow = pty.contents();
    assert!(narrow.contains("Alpha"));
    assert!(!narrow.contains("Lineage"), "narrow layout shows one pane only");
    pty.input("\t");
    pty.text("Lineage");
    pty.input("\t");
    pty.resize(12, 40);
    // 极窄尺寸只要求可用的标题与不越界，行内容可以裁剪。
    pty.text("1 Agents");
    pty.resize(4, 24);
    pty.resize(30, 100);
    // 树形过滤保留 lineage。
    pty.input("t");
    pty.input("/");
    for character in "Beta".chars() {
        pty.input(&character.to_string());
    }
    pty.input("\r");
    pty.text("Alpha");
    pty.text("Beta");
    // Activity scope：Selected/Subtree 不含 sibling。
    pty.input("2");
    pty.input("s");
    pty.text("Selected agent");
    pty.input("s");
    pty.text("Subtree");
    pty.input("1");
    // 测试暂停 follow：在 Activity 上先播报大量行。
    let records = (0..40)
        .map(|index| {
            json!({
                "agent_id": "a1",
                "run_id": "r1",
                "generation": 2,
                "timestamp_ms": 1_000 + index,
                "event": {"user_input_accepted": {"id": index, "text": format!("hub-activity-row-{index}")}}
            })
        })
        .collect::<Vec<_>>();
    hub_append(home.path(), "a1", &records);
    pty.input("\x1b[15~");
    pty.input("2");
    // Subtree 只含选中 Agent，回到 All 才能看到新注入的历史。
    pty.input("s");
    pty.text("All agents");
    pty.text("hub-activity-row-39");
    // 手动上移暂停跟随并固定选中 id。
    for _ in 0..5 {
        pty.input("k");
    }
    pty.text("paused");
    let paused = pty.contents();
    let pinned = (0..40)
        .rev()
        .map(|index| format!("hub-activity-row-{index}"))
        .find(|needle| paused.contains(needle))
        .expect("selected activity visible");
    pty.input("1");
    pty.input("2");
    let after = pty.contents();
    assert!(after.contains(&pinned), "paused viewport must not jump");
    // Space 恢复跟随并跳最新。
    pty.input(" ");
    pty.text("follow latest");
    // Ctrl+O 只影响 child：Main 工具卡片不变。
    pty.input("1");
    pty.input("\r");
    pty.text("Beta / a2");
    pty.input("\x0f");
    hub_escape(&mut pty, "j/k select");
    hub_clear_filter(&mut pty, "Alpha");
    hub_close(&mut pty);
    let main = pty.contents();
    assert!(main.contains("task"), "{main}");
    // 真实 SGR 鼠标点击 roster 的可见物理行：打开该 Agent 的详情。
    pty.input("\x1ba");
    pty.text("Agent Hub");
    pty.input("\x1b[<0;6;4M");
    pty.input("\x1b[<0;6;4m");
    pty.text(" / a");
    pty.resize(40, 100);
    pty.resize(40, 140);
    pty.input("\x1b");
    pty.text("j/k select");
    pty.input("\x1b");
    pty.ready();
    pty.input("\x03");
    pty.finish();
}

#[test]
fn agent_hub_reads_full_history_and_restores_legacy() {
    let protocol = "completion";
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        response_models(&fixture, Some(protocol)),
    )
    .unwrap();
    let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
    pty.ready();
    pty.input("delegate long history task\r");
    let mut children = hub_dispatch(
        &mut pty,
        &fixture,
        protocol,
        json!({"context":"legacy context","tasks":[{"task":"legacy child work"}]}),
        &["legacy child work"],
    );
    // 让 child 的完整历史超过 64 KiB，最早的 task 卡片只能靠完整读取看到。
    let mut stream = SseReply::new(children.remove(0));
    for index in 0..40 {
        streaming_delta(&mut stream, protocol, &format!("{}-{index}\n", "x".repeat(2_000)));
    }
    streaming_delta(&mut stream, protocol, "hub-tail-marker");
    streaming_complete(&mut stream, protocol, "");
    let (request, body) = pty.request_at(&fixture, api_path(protocol));
    assert!(!hub_is_child(&body));
    ask_final_reply(request, protocol, "long-parent-ok");
    pty.text("long-parent-ok");
    // 旧日志（无 timestamp / usage）追加到 a1：显示 — 而不是当前时间。
    hub_append(
        home.path(),
        "a1",
        &[json!({
            "agent_id": "a1",
            "run_id": "r1",
            "generation": 2,
            "event": {"user_input_accepted": {"id": 900, "text": "hub-legacy-row"}}
        })],
    );
    pty.input("/subagents\r");
    pty.input("\x1b[15~");
    pty.text("a1");
    // Activity 中旧记录没有时间戳，显示占位符。
    pty.input("2");
    pty.text("hub-legacy-row");
    let legacy = pty
        .contents()
        .lines()
        .find(|line| line.contains("hub-legacy-row"))
        .expect("legacy row visible")
        .to_owned();
    assert!(
        legacy.contains("— ✔"),
        "legacy row must not invent a timestamp: {legacy}"
    );
    // 详情只进入备用屏幕，关闭回 Main。
    pty.input("1");
    pty.input("\r");
    pty.text("Agent Hub");
    assert!(pty.parser.screen().alternate_screen());
    pty.text("hub-tail-marker");
    pty.input("\x1b[1;5H"); // Ctrl+Home 到顶部
    pty.text("legacy child work");
    // 未知用量显示 — 而不是零成本。
    hub_escape(&mut pty, "j/k select");
    let inspector = pty.contents();
    assert!(inspector.contains("cost —"), "{inspector}");
    hub_close(&mut pty);
    assert!(!pty.parser.screen().alternate_screen());
    no_more(&fixture);
    pty.input("\x03");
    pty.finish();
    // 恢复根后旧 child 仍可查看，且不会自动发送模型请求。
    let id = saved_session_id(home.path());
    let mut resumed = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
    resumed.ready();
    resumed.input("/subagents\r");
    resumed.text("a1");
    resumed.input("\r");
    resumed.text("legacy child work");
    no_more(&fixture);
    resumed.input("\x1b");
    resumed.ready();
    resumed.input("\x03");
    resumed.finish();
}
