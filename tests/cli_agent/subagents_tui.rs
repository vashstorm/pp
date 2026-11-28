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
    fixture: &TestServer,
    protocol: &str,
    tasks: Value,
    needles: &[&str],
) -> Vec<CapturedRequest> {
    let (request, _) = pty.request_at(fixture, api_path(protocol));
    ask_reply_calls(
        request,
        protocol,
        json!([hub_call(protocol, "task", "task", tasks)]),
    );
    let mut found: Vec<Option<CapturedRequest>> = needles.iter().map(|_| None).collect();
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

/// 结束子 Agent；只有整批完成后才消费父方继续请求。
fn hub_finish_child(pty: &mut Pty, fixture: &TestServer, protocol: &str, child: CapturedRequest, text: &str) {
    ask_final_reply(child, protocol, text);
    let (request, body) = pty.request_at(fixture, api_path(protocol));
    assert!(!hub_is_child(&body), "unexpected child request");
    ask_final_reply(request, protocol, "parent-ack");
    pty.text("parent-ack");
    pty.ready();
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

/// 按正文定位写出该文本的 Agent 观察日志，返回 agent id 与全部记录。
/// 新格式每行都是独立 JSON（不再共享值表），测试侧可直接用 serde_json 解析。
fn hub_events_with(home: &std::path::Path, needle: &str) -> (String, Vec<Value>) {
    for entry in std::fs::read_dir(hub_dir(home))
        .expect("subagent directory")
        .flatten()
    {
        let Some(agent) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".events.jsonl"))
            .map(str::to_owned)
        else {
            continue;
        };
        let bytes = std::fs::read(entry.path()).expect("event log");
        if !String::from_utf8_lossy(&bytes).contains(needle) {
            continue;
        }
        let records = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("observed event json"))
            .collect();
        return (agent, records);
    }
    panic!("no observation log contains {needle}");
}

/// 观察记录的种类：unit 变体是字符串，结构变体与 hub 记录是单键对象。
fn hub_record_kind(record: &Value) -> String {
    if let Some(hub) = record.get("hub") {
        return format!(
            "hub:{}:{}",
            hub["kind"].as_str().unwrap_or("?"),
            hub["status"].as_str().unwrap_or("")
        );
    }
    match &record["event"] {
        Value::String(kind) => kind.clone(),
        Value::Object(map) => map.keys().next().cloned().unwrap_or_default(),
        other => panic!("unexpected observed record: {other}"),
    }
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
fn agent_hub_streams_and_anchors_activity() {
    let protocol = "response";
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("probe.txt"), "hub-probe-body").unwrap();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            r#"{"default_model":"fixture/primary","notification_timeout_seconds":1,"bash_timeout_seconds":37,"read":{"toolResultPreview":true},"model_roles":{"fast":{"model":"fixture/primary","effort":"none"}}}"#,
        ).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &["--yolo"]);
        pty.ready();
        pty.input("delegate hub stream task\r");
        let mut children = hub_dispatch(
            &mut pty,
            &fixture,
            protocol,
            json!({"context":"hub context","tasks":[
                {"role":"fast","name":"LiveWriter","task":"stream the two hub chunks in order"},
                {"role":"fast","name":"ToolFailure","task":"read probe.txt then run the failing command"}
            ]}),
            &["the two hub chunks", "probe.txt"],
        );
        // LiveWriter 保持 SSE 打开；ToolFailure 走 失败工具 + 后续成功回复。
        let mut live = children.remove(0).start_sse().unwrap();
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
        cancel_assert_no_request(&fixture);
        // 先发第一段，再进入详情：无需额外按键即可看到。
        streaming_delta(&mut live, protocol, "hub-stream-first");
        pty.input("/agents\r");
        pty.wait_for("运行中的子 Agent 名称前显示动态转轮", |screen| {
            let text = screen.contents();
            "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
                .chars()
                .any(|frame| text.contains(&format!("{frame} LiveWriter")))
        });
        pty.text("ToolFailure");
        pty.input("\r");
        pty.text("hub-stream-first");
        pty.wait_for("子 Agent 在流式请求结束前显示正速率", context_tui_positive_speed);
        let (first_rate, _) = context_tui_speed(pty.parser.screen()).unwrap();
        pty.wait_for("子 Agent 详情显示实时时钟", |screen| {
            let text = screen.contents();
            WAIT_CLOCKS
                .into_iter()
                .any(|clock| text.contains(&format!("{clock} Responding")))
        });
        let first_clock = {
            let text = pty.contents();
            WAIT_CLOCKS
                .into_iter()
                .find(|clock| text.contains(&format!("{clock} Responding")))
                .unwrap()
        };
        pty.wait_for("子 Agent 详情时钟继续转动", |screen| {
            let text = screen.contents();
            WAIT_CLOCKS
                .into_iter()
                .any(|clock| clock != first_clock && text.contains(&format!("{clock} Responding")))
        });
        // 第二段只追加一次。
        // 分片拼接必须与最终文本一致，否则 Responses 协议会判定冲突。
        let second = format!(" hub-stream-second{}", " more".repeat(100));
        streaming_delta(&mut live, protocol, &second);
        pty.text("hub-stream-second");
        assert_eq!(hub_count(&pty.contents(), "hub-stream-second"), 1);
        pty.wait_for("子 Agent 在后续输出到达时提高速率", |screen| {
            context_tui_speed(screen).is_some_and(|(rate, _)| rate > first_rate)
        });
        streaming_complete(&mut live, protocol, &format!("hub-stream-first{second}"));
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        ask_final_reply(request, protocol, "hub-parent-stream-done");
        pty.text("✻ Completed");
        // Activity：失败只出现在 Errors。
        hub_escape(&mut pty, "Roster ·");
        pty.input("2");
        pty.text("All agents");
        pty.input("f");
        pty.text("Errors");
        let screen = pty.contents();
        assert_eq!(hub_count(&screen, "\u{f00d}"), 1, "{screen}");
        // Enter 定位到失败工具卡片。
        pty.input("\r");
        pty.text("Agent Hub");
        pty.text("print(\"hub-tool-fail\")");
        // 从磁盘历史恢复工具，使用共享配置，不把回放耗时当作 Wall。
        pty.text("Timeout: 37s");
        assert!(!pty.contents().contains("Wall:"));
        pty.input("\x1b[1;5H");
        pty.text("Read · probe.txt");
        assert!(!pty.contents().contains("hub-probe-body"));
        // Esc 回到原 scope/search/选中活动。
        hub_escape(&mut pty, "Errors");
        assert_eq!(hub_count(&pty.contents(), "\u{f00d}"), 1);
        hub_escape(&mut pty, "Roster ·");
        hub_close(&mut pty);
        pty.input("\x03");
        pty.finish();
}

