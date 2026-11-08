// Diff 块级渲染 PTY 回归：两种 edit 模式／两协议的真实提交 diff（磁盘落盘 + 屏幕
// 红/绿与行号 + 统计）、write 新建/覆盖/同内容/空文件、CRLF→LF／末尾换行删除／BOM
// /非 UTF-8 的 unavailable 回退、无读权限文件的 write 不被快照失败阻断、失败 edit
// 不伪造 diff、超过 40 行的默认折叠与Ctrl+O 展开/收起、退出原生历史保持当时的折叠状态
// 保留颜色，以及 /resume 与启动恢复重放相同 diff 且不重新执行旧工具、缺 FileChanged
// 的旧日志正常加载。生成期间的临时参数预览卡片另有两协议 × edit/write 的流式用例：
// 预览先于提交出现、磁盘未变、取消后消失、提交后替换为唯一真实卡片且不进入 journal。
// 全部通过真实 PTY、loopback HTTP 与磁盘文件观察，不用 sleep 猜测阶段；辅助函数一律
// 使用 `diff_blocks_` 前缀，测试函数使用 `transcript_blocks_` / `*_preview_tui_` 前缀，
// 不新增 use 声明以免与兄弟 include 文件冲突。

const DIFF_BLOCKS_ADDED: vt100::Color = vt100::Color::Rgb(137, 210, 129);
const DIFF_BLOCKS_REMOVED: vt100::Color = vt100::Color::Rgb(252, 58, 75);

fn diff_blocks_stream_delta(
    stream: &mut SseReply,
    protocol: &str,
    name: &str,
    call_id: &str,
    item_id: &str,
    text: &str,
    first: bool,
) {
    if protocol == "completion" {
        let function = if first { json!({"name":name,"arguments":text}) } else { json!({"arguments":text}) };
        let mut call = json!({"index":0,"function":function});
        if first {
            call["id"] = json!(call_id);
            call["type"] = json!("function");
        }
        stream.event(chat_chunk(json!({"tool_calls":[call]}), None));
    } else {
        if first {
            stream.event(json!({"type":"response.output_item.added","sequence_number":0,"output_index":0,
                "item":{"type":"function_call","id":item_id,"call_id":call_id,"name":name,"arguments":""}}));
        }
        stream.event(json!({"type":"response.function_call_arguments.delta","sequence_number":1,
            "output_index":0,"item_id":item_id,"delta":text}));
    }
}

#[test]
fn edit_preview_tui_streams_before_commit_both_protocols_and_edit_modes() {
    for protocol in ["completion", "response"] {
        for mode in ["replace", "hashline"] {
            let provider = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            std::fs::write(config_path(home.path(), "settings.yml"), diff_blocks_settings(mode)).unwrap();
            std::fs::write(config_path(home.path(), "models.yml"), response_models(&provider, Some(protocol))).unwrap();
            let target = project.path().join("demo.txt");
            std::fs::write(&target, "old\nkeep\n").unwrap();
            let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
            pty.ready();
            pty.input("stream-edit-task\r");
            let path = api_path(protocol);
            let (mut request, _) = pty.request_at(&provider, path);
            let args = if mode == "replace" {
                json!({"path":"demo.txt","edits":[{"oldText":"old","newText":"typing-中-READY"}]})
            } else {
                diff_blocks_calls_reply(request, protocol, json!([diff_blocks_call(protocol, "read-anchor", "read", json!({"path":"demo.txt"}))]));
                let body;
                (request, body) = pty.request_at(&provider, path);
                let result = diff_blocks_tool_result(&body, protocol, "read-anchor");
                let header = result["content"].as_str().unwrap().lines().next().unwrap();
                json!({"input":format!("{header}\nPUT 1.=1:\n+typing-中-READY")})
            };
            let arguments = args.to_string();
            let split = arguments.find("-READY").unwrap();
            let end = split + "-READY".len();
            let mut stream = SseReply::new(request);
            diff_blocks_stream_delta(
                &mut stream,
                protocol,
                "edit",
                "stream-edit",
                "fc-edit",
                &arguments[..split],
                true,
            );
            pty.wait_for("完整参数到达前显示编辑预览", |screen| {
                let text = screen.contents();
                text.contains("preview · not saved") && text.contains("typing-中") && !text.contains("READY")
            });
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\nkeep\n");
            pty.input("draft-preserved");
            pty.draft("draft-preserved");
            diff_blocks_stream_delta(
                &mut stream,
                protocol,
                "edit",
                "stream-edit",
                "fc-edit",
                &arguments[split..end],
                false,
            );
            pty.text("typing-中-READY");
            pty.draft("draft-preserved");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\nkeep\n");
            diff_blocks_stream_delta(
                &mut stream,
                protocol,
                "edit",
                "stream-edit",
                "fc-edit",
                &arguments[end..],
                false,
            );
            if protocol == "completion" {
                stream.event(chat_chunk(json!({}), Some("tool_calls")));
                stream.frame("[DONE]");
            } else {
                stream.event(json!({"type":"response.completed","sequence_number":2,
                    "response":response_body(json!([response_call("stream-edit", "edit", args)]))}));
            }
            stream.finish();
            let (request, body) = pty.request_at(&provider, path);
            assert_eq!(diff_blocks_tool_result(&body, protocol, "stream-edit")["ok"], true);
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "typing-中-READY\nkeep\n");
            pty.wait_for("临时预览替换为唯一真实提交卡片", |screen| {
                let text = screen.contents();
                text.contains("+1/-1") && !text.contains("not saved") && text.matches("Edit:").count() == 1
            });
            diff_blocks_final(request, protocol, "stream-edit-done");
            pty.text("stream-edit-done");
            pty.ready();
            pty.draft("draft-preserved");
            let (history, _) = diff_blocks_exit_scrollback(&mut pty, "stream-edit-done");
            assert!(!history.contains("not saved"), "{history}");
            pty.finish();
            for entry in std::fs::read_dir(resume_history_journal_dir(home.path())).unwrap() {
                let entry = entry.unwrap();
                if entry.path().extension().is_some_and(|extension| extension == "jsonl") {
                    let journal = std::fs::read_to_string(entry.path()).unwrap();
                    assert!(!journal.contains("tool_arguments_delta"));
                }
            }
            no_more(&provider);
        }
    }
}

#[test]
fn edit_preview_tui_cancel_discards_partial_arguments_without_saving() {
    for protocol in ["completion", "response"] {
        let provider = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), diff_blocks_settings("replace")).unwrap();
        std::fs::write(config_path(home.path(), "models.yml"), response_models(&provider, Some(protocol))).unwrap();
        let target = project.path().join("demo.txt");
        std::fs::write(&target, "old").unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("cancel-edit-preview\r");
        let (request, _) = pty.request_at(&provider, api_path(protocol));
        let mut stream = SseReply::new(request);
        diff_blocks_stream_delta(
            &mut stream,
            protocol,
            "edit",
            "stream-edit",
            "fc-edit",
            r#"{"path":"demo.txt","edits":[{"oldText":"old","newText":"cancel-marker"#,
            true,
        );
        pty.text("cancel-marker");
        pty.input("\x1b");
        pty.wait_for("取消后移除编辑预览并恢复输入", |screen| {
            input_is_ready(screen) && !screen.contents().contains("not saved")
        });
        assert_eq!(std::fs::read_to_string(target).unwrap(), "old");
        drop(stream);
        pty.input("\x03");
        pty.finish();
        no_more(&provider);
    }
}

// 生成 write 参数期间显示临时预览卡片（Write: 标题含路径 + 新增行正文），
// 超限后滚动到最新行，真实保存时将同一段尾部正文保留在提交卡片及原生历史中。
#[test]
fn write_preview_tui_streams_before_commit_both_protocols() {
    for protocol in ["completion", "response"] {
        let provider = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            diff_blocks_settings("replace"),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&provider, Some(protocol)),
        )
        .unwrap();
        let target = project.path().join("new.txt");
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("stream-write-task\r");
        let path = api_path(protocol);
        let (request, _) = pty.request_at(&provider, path);
        // 键序固定为 path 在前，预览首片就要同时呈现路径行与已生成正文；
        // `serde_json::Value` 会按字母序重排键，因此不能用 `json!` 构造流式分片。
        let content = (1..=20)
            .map(|i| format!("write-row-{i:02}\n"))
            .collect::<String>();
        let encoded = serde_json::to_string(&content).unwrap();
        let arguments = format!(r#"{{"path":"new.txt","content":{encoded}}}"#);
        let args: Value = serde_json::from_str(&arguments).unwrap();
        let split = arguments.find("write-row-17").unwrap();
        let end = arguments.len() - 2;
        let shows_tail = |text: &str| {
            text.contains("write-row-05")
                && text.contains("write-row-20")
                && !text.contains("write-row-04")
                && !text.contains("write-row-01")
        };
        let mut stream = SseReply::new(request);
        diff_blocks_stream_delta(
            &mut stream,
            protocol,
            "write",
            "stream-write",
            "fc-write",
            &arguments[..split],
            true,
        );
        pty.wait_for("完整参数到达前显示 write 预览卡片", |screen| {
            let text = screen.contents();
            text.contains("preview · not saved")
                && text.contains("Write:")
                && text.contains("new.txt")
                && text.contains("write-row-01")
                && text.contains("write-row-16")
                && !text.contains("more lines")
        });
        diff_blocks_stream_delta(
            &mut stream,
            protocol,
            "write",
            "stream-write",
            "fc-write",
            &arguments[split..end],
            false,
        );
        pty.wait_for("超限的 write 预览滚动到末尾十六行", |screen| {
            let text = screen.contents();
            text.contains("preview · not saved") && text.contains("more lines") && shows_tail(&text)
        });
        assert!(!target.exists());
        diff_blocks_stream_delta(
            &mut stream,
            protocol,
            "write",
            "stream-write",
            "fc-write",
            &arguments[end..],
            false,
        );
        if protocol == "completion" {
            stream.event(chat_chunk(json!({}), Some("tool_calls")));
            stream.frame("[DONE]");
        } else {
            stream.event(json!({"type":"response.completed","sequence_number":2,
                "response":response_body(json!([response_call("stream-write", "write", args)]))}));
        }
        stream.finish();
        let (request, body) = pty.request_at(&provider, path);
        assert_eq!(
            diff_blocks_tool_result(&body, protocol, "stream-write")["ok"],
            true
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), content);
        pty.wait_for("write 保存后保留末尾十六行", |screen| {
            let text = screen.contents();
            text.contains("Wrote ") && !text.contains("not saved") && shows_tail(&text)
        });
        diff_blocks_final(request, protocol, "stream-write-done");
        pty.text("stream-write-done");
        pty.ready();
        let (history, _) = diff_blocks_exit_scrollback(&mut pty, "stream-write-done");
        assert!(shows_tail(&history) && !history.contains("not saved"), "{history}");
        pty.finish();
        for entry in std::fs::read_dir(resume_history_journal_dir(home.path())).unwrap() {
            let entry = entry.unwrap();
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                let journal = std::fs::read_to_string(entry.path()).unwrap();
                assert!(!journal.contains("tool_arguments_delta"));
            }
        }
        no_more(&provider);
    }
}

fn diff_blocks_settings(mode: &str) -> String {
    format!(
        r#"{{"default_model":"fixture/primary","edit":"{mode}","notification_timeout_seconds":1,"read":{{"toolResultPreview":true}}}}"#
    )
}

// 直接按终端单元格匹配 ASCII 文本及颜色，避免标题图标影响列号计算。
fn diff_blocks_screen_colored(screen: &vt100::Screen, needle: &str, color: vt100::Color) -> bool {
    // edit 使用参考图色板，/changes 继续使用原有 diff 色板；局部变更以背景强调。
    let edit_color = match color {
        DIFF_BLOCKS_ADDED => vt100::Color::Rgb(153, 208, 137),
        DIFF_BLOCKS_REMOVED => vt100::Color::Rgb(232, 76, 82),
        other => other,
    };
    let (rows, columns) = screen.size();
    let width = needle.chars().count() as u16;
    for row in 0..rows {
        for start in 0..columns.saturating_sub(width).saturating_add(1) {
            if needle.chars().enumerate().all(|(offset, expected)| {
                screen
                    .cell(row, start + offset as u16)
                    .is_some_and(|cell| {
                        cell.contents() == expected.to_string()
                            && (cell.fgcolor() == color || cell.fgcolor() == edit_color || cell.bgcolor() == edit_color)
                    })
            }) {
                return true;
            }
        }
    }
    false
}

fn diff_blocks_call(protocol: &str, id: &str, name: &str, args: Value) -> Value {
    if protocol == "completion" {
        call(id, name, args)
    } else {
        response_call(id, name, args)
    }
}

fn diff_blocks_calls_reply(request: Request, protocol: &str, calls: Value) {
    if protocol == "completion" {
        calls_reply(request, calls);
    } else {
        response_reply(request, calls);
    }
}

fn diff_blocks_final(request: Request, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}

fn diff_blocks_tool_result(body: &Value, protocol: &str, id: &str) -> Value {
    cancel_tool_outputs(body, protocol)
        .into_iter()
        .find(|entry| entry.0 == id)
        .unwrap_or_else(|| panic!("missing tool result for {id}"))
        .1
}

// Ctrl-C 退出后读取原生滚动历史及主屏幕，保留颜色用于断言。
fn diff_blocks_exit_scrollback(pty: &mut Pty, last_answer: &str) -> (String, Vec<u8>) {
    pty.input("\x03");
    pty.wait_for("退出后恢复输入模式", |screen| !screen.bracketed_paste());
    let (contents, raw) = pty.native_history();
    assert!(contents.contains(last_answer), "{contents}");
    (contents, raw)
}

// 复制一份真实 journal 并删除全部 FileChanged 事件行，模拟缺少 diff 事件的旧日志。
fn diff_blocks_clone_journal_without_file_changed(home: &Path, source: &str, id: &str) {
    let journal = resume_history_journal_dir(home).join(format!("{source}.jsonl"));
    let text = std::fs::read_to_string(journal).unwrap();
    let mut lines = text.lines();
    let mut header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    header["id"] = json!(id);
    let mut cloned = format!("{header}\n");
    let mut parent = Value::Null;
    for line in lines {
        let mut entry: Value = serde_json::from_str(line).unwrap();
        if entry["data"]["event"].get("file_changed").is_some() { continue; }
        entry["parentId"] = parent;
        parent = entry["id"].clone();
        cloned.push_str(&entry.to_string());
        cloned.push('\n');
    }
    std::fs::write(
        resume_history_journal_dir(home).join(format!("{id}.jsonl")),
        cloned,
    )
    .unwrap();
}

// replace 模式的真实 edit old→new 与 write 新建/覆盖/同内容/空文件：屏幕红绿、
// 行号、+N -N 统计和磁盘结果一致。
#[test]
fn transcript_blocks_diff_replace_edit_and_write_cards() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            diff_blocks_settings("replace"),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        std::fs::write(project.path().join("demo.txt"), "old\nkeep\n").unwrap();
        let path = api_path(protocol);
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("diff-blocks edit demo\r");

        let (request, _) = pty.request_at(&server, path);
        diff_blocks_calls_reply(
            request,
            protocol,
            json!([diff_blocks_call(
                protocol,
                "e1",
                "edit",
                json!({"path":"demo.txt","edits":[{"oldText":"old","newText":"new"}]})
            )]),
        );
        pty.wait_for("committed replace edit diff card", |screen| {
            let text = screen.contents();
            text.contains("Edit:") && text.contains("demo.txt")
                && text.contains("+1/-1")
                && text.contains("-  1│old")
                && text.contains("+  1│new")
                && text.contains("   2│keep")
                && diff_blocks_screen_colored(screen, "old", DIFF_BLOCKS_REMOVED)
                && diff_blocks_screen_colored(screen, "new", DIFF_BLOCKS_ADDED)
                && diff_blocks_screen_colored(screen, "Edit:", vt100::Color::Rgb(244, 191, 84))
                && diff_blocks_screen_colored(screen, "demo.txt", vt100::Color::Rgb(244, 191, 84))
        });
        assert_eq!(
            std::fs::read_to_string(project.path().join("demo.txt")).unwrap(),
            "new\nkeep\n"
        );
        let (request, body) = pty.request_at(&server, path);
        let edited = diff_blocks_tool_result(&body, protocol, "e1");
        assert_eq!(edited["ok"], json!(true), "{edited}");
        assert_eq!(edited["edits_applied"], json!(1), "{edited}");

        diff_blocks_calls_reply(
            request,
            protocol,
            json!([
                diff_blocks_call(
                    protocol,
                    "w1",
                    "write",
                    json!({"path":"fresh.txt","content":"alpha\n"})
                ),
                diff_blocks_call(protocol, "w2", "write", json!({"path":"empty.txt","content":""})),
            ]),
        );
        pty.wait_for("created and empty write cards", |screen| {
            let text = screen.contents();
            text.contains("write · fresh.txt")
                && text.contains("alpha")
                && text.contains("Wrote 6 bytes")
                && text.contains("write · empty.txt")
                && text.contains("Wrote 0 bytes")
        });
        assert_eq!(
            std::fs::read_to_string(project.path().join("fresh.txt")).unwrap(),
            "alpha\n"
        );
        assert_eq!(std::fs::read(project.path().join("empty.txt")).unwrap().len(), 0);

        let (request, _) = pty.request_at(&server, path);
        diff_blocks_calls_reply(
            request,
            protocol,
            json!([
                diff_blocks_call(
                    protocol,
                    "w3",
                    "write",
                    json!({"path":"fresh.txt","content":"alpha\n"})
                ),
                diff_blocks_call(
                    protocol,
                    "w4",
                    "write",
                    json!({"path":"demo.txt","content":"new\nkeep\nbeta\n"})
                ),
            ]),
        );
        pty.wait_for("same-content and overwrite write cards", |screen| {
            let text = screen.contents();
            text.contains("Wrote 6 bytes")
                && text.contains("beta")
                && text.contains("Wrote 14 bytes")
        });
        let (request, _) = pty.request_at(&server, path);
        diff_blocks_final(request, protocol, "diff-blocks write round done");
        pty.text("diff-blocks write round done");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
        assert_eq!(
            std::fs::read_to_string(project.path().join("demo.txt")).unwrap(),
            "new\nkeep\nbeta\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.path().join("fresh.txt")).unwrap(),
            "alpha\n"
        );
    }
}

// hashline 模式：先 read 取得真实锚点，再用锚点 edit；diff 与磁盘一致。
#[test]
fn transcript_blocks_diff_hashline_edit_from_read_anchors() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            diff_blocks_settings("hashline"),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        std::fs::write(project.path().join("demo.txt"), "old\nkeep\n").unwrap();
        let path = api_path(protocol);
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("diff-blocks hashline edit\r");
        let (request, _) = pty.request_at(&server, path);
        diff_blocks_calls_reply(
            request,
            protocol,
            json!([diff_blocks_call(protocol, "r1", "read", json!({"path":"demo.txt"}))]),
        );
        let (request, body) = pty.request_at(&server, path);
        let read = diff_blocks_tool_result(&body, protocol, "r1");
        assert_eq!(read["ok"], json!(true), "{read}");
        let anchor = read["content"]
            .as_str()
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_owned();
        assert!(anchor.starts_with('['), "{anchor}");
        diff_blocks_calls_reply(
            request,
            protocol,
            json!([diff_blocks_call(
                protocol,
                "e1",
                "edit",
                json!({"input":format!("{anchor}\nPUT 1.=1:\n+new")})
            )]),
        );
        pty.wait_for("committed hashline edit diff card", |screen| {
            let text = screen.contents();
            text.contains("Edit:") && text.contains("demo.txt")
                && text.contains("+1/-1")
                && text.contains("-  1│old")
                && text.contains("+  1│new")
                && text.contains("   2│keep")
                && diff_blocks_screen_colored(screen, "old", DIFF_BLOCKS_REMOVED)
                && diff_blocks_screen_colored(screen, "new", DIFF_BLOCKS_ADDED)
        });
        assert_eq!(
            std::fs::read_to_string(project.path().join("demo.txt")).unwrap(),
            "new\nkeep\n"
        );
        let (request, _) = pty.request_at(&server, path);
        diff_blocks_final(request, protocol, "diff-blocks hashline done");
        pty.text("diff-blocks hashline done");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }
}

// 行结束符边界可见：CRLF→LF 附 LF/CRLF 提示、末尾换行删除附 No newline 行、
// BOM 剥离产生真实 +1/-1、非 UTF-8 覆盖成功且显示 unavailable 回退。
#[test]
fn transcript_blocks_diff_endings_bom_and_unavailable() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            diff_blocks_settings("replace"),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        std::fs::write(project.path().join("eol.txt"), "a\r\nb\r\n").unwrap();
        std::fs::write(project.path().join("nonl.txt"), "x\n").unwrap();
        std::fs::write(project.path().join("bom.txt"), "\u{feff}bom\nkeep\n").unwrap();
        std::fs::write(project.path().join("bin.dat"), b"\xff\xfe\x00abc").unwrap();
        let path = api_path(protocol);
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("diff-blocks endings\r");
        let (request, _) = pty.request_at(&server, path);
        diff_blocks_calls_reply(
            request,
            protocol,
            json!([
                diff_blocks_call(
                    protocol,
                    "w1",
                    "write",
                    json!({"path":"eol.txt","content":"a\nb\n"})
                ),
                diff_blocks_call(protocol, "w2", "write", json!({"path":"nonl.txt","content":"x"})),
            ]),
        );
        pty.wait_for("换行变更后的写入正文", |screen| {
            let text = screen.contents();
            text.contains("write · eol.txt") && text.contains("Wrote 4 bytes")
                && text.contains("write · nonl.txt") && text.contains("Wrote 1 bytes")
        });
        assert_eq!(
            std::fs::read(project.path().join("eol.txt")).unwrap(),
            b"a\nb\n"
        );
        assert_eq!(std::fs::read(project.path().join("nonl.txt")).unwrap(), b"x");

        let (request, _) = pty.request_at(&server, path);
        diff_blocks_calls_reply(
            request,
            protocol,
            json!([
                diff_blocks_call(
                    protocol,
                    "w3",
                    "write",
                    json!({"path":"bom.txt","content":"bom\nkeep\n"})
                ),
                diff_blocks_call(
                    protocol,
                    "w4",
                    "write",
                    json!({"path":"bin.dat","content":"binary text\n"})
                ),
            ]),
        );
        pty.wait_for("BOM 与二进制覆盖后的正文预览", |screen| {
            let text = screen.contents();
            text.contains("write · bom.txt") && text.contains("bom") && text.contains("keep")
                && text.contains("write · bin.dat") && text.contains("binary text")
        });
        let (request, body) = pty.request_at(&server, path);
        let written = diff_blocks_tool_result(&body, protocol, "w4");
        assert_eq!(written["ok"], json!(true), "{written}");
        diff_blocks_final(request, protocol, "diff-blocks endings done");
        pty.text("diff-blocks endings done");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
        assert_eq!(
            std::fs::read(project.path().join("bom.txt")).unwrap(),
            b"bom\nkeep\n"
        );
        assert_eq!(
            std::fs::read(project.path().join("bin.dat")).unwrap(),
            b"binary text\n"
        );
    }
}

// 提交前失败没有 FileChanged：replace 找不到 oldText 与过期 hashline 锚点都只显示
// 工具错误，不伪造红绿 diff，文件保持原样。
#[test]
fn transcript_blocks_diff_failed_edits_have_no_fabricated_diff() {
    for mode in ["replace", "hashline"] {
        for protocol in ["completion", "response"] {
            let server = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            std::fs::write(
                config_path(home.path(), "settings.yml"),
                diff_blocks_settings(mode),
            )
            .unwrap();
            std::fs::write(
                config_path(home.path(), "models.yml"),
                response_models(&server, Some(protocol)),
            )
            .unwrap();
            std::fs::write(project.path().join("demo.txt"), "old\nkeep\n").unwrap();
            let path = api_path(protocol);
            let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
            pty.ready();
            pty.input("diff-blocks failing edit\r");
            let (request, _) = pty.request_at(&server, path);
            let error_needle = if mode == "hashline" {
                diff_blocks_calls_reply(
                    request,
                    protocol,
                    json!([diff_blocks_call(protocol, "r1", "read", json!({"path":"demo.txt"}))]),
                );
                let (request, body) = pty.request_at(&server, path);
                let read = diff_blocks_tool_result(&body, protocol, "r1");
                let anchor = read["content"]
                    .as_str()
                    .unwrap()
                    .lines()
                    .next()
                    .unwrap()
                    .to_owned();
                // 外部修改使锚点在 edit 执行前过期。
                std::fs::write(project.path().join("demo.txt"), "external\nkeep\n").unwrap();
                diff_blocks_calls_reply(
                    request,
                    protocol,
                    json!([diff_blocks_call(
                        protocol,
                        "e1",
                        "edit",
                        json!({"input":format!("{anchor}\nPUT 1.=1:\n+new")})
                    )]),
                );
                "hashline snapshot changed"
            } else {
                diff_blocks_calls_reply(
                    request,
                    protocol,
                    json!([diff_blocks_call(
                        protocol,
                        "e1",
                        "edit",
                        json!({"path":"demo.txt","edits":[
                            {"oldText":"absent old text","newText":"new"}
                        ]})
                    )]),
                );
                "could not find edits[0].oldText"
            };
            pty.wait_for("failed edit error without diff body", |screen| {
                let text = screen.contents();
                text.contains("Edit:") && text.contains("demo.txt")
                    && text.contains(error_needle)
                    && !text.contains("+1/-1")
                    && !diff_blocks_screen_colored(screen, "old", DIFF_BLOCKS_REMOVED)
                    && !diff_blocks_screen_colored(screen, "new", DIFF_BLOCKS_ADDED)
            });
            let (request, body) = pty.request_at(&server, path);
            let failed = diff_blocks_tool_result(&body, protocol, "e1");
            assert_eq!(failed["ok"], json!(false), "{failed}");
            diff_blocks_final(request, protocol, "diff-blocks failure done");
            pty.text("diff-blocks failure done");
            pty.ready();
            pty.input("/quit\r");
            pty.finish();
            no_more(&server);
            let expected = if mode == "hashline" {
                "external\nkeep\n"
            } else {
                "old\nkeep\n"
            };
            assert_eq!(
                std::fs::read_to_string(project.path().join("demo.txt")).unwrap(),
                expected
            );
        }
    }
}

// 超过 16 行的 diff 默认折叠，Ctrl+O 展开/收起，退出 scrollback 展开全部并保留颜色。
#[test]
fn transcript_blocks_diff_long_diff_collapses_expands_and_scrolls_back() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            diff_blocks_settings("replace"),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let old = (1..=24)
            .map(|number| number.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let new = (b'A'..=b'X')
            .map(|byte| char::from(byte).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(project.path().join("lines.txt"), format!("{old}\nkeep\n")).unwrap();
        let path = api_path(protocol);
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.resize(80, 100);
        pty.input("diff-blocks long edit\r");
        let (request, _) = pty.request_at(&server, path);
        diff_blocks_calls_reply(
            request,
            protocol,
            json!([diff_blocks_call(
                protocol,
                "e1",
                "edit",
                json!({"path":"lines.txt","edits":[{"oldText":old,"newText":new}]})
            )]),
        );
        pty.wait_for("long diff collapsed preview", |screen| {
            let text = screen.contents();
            text.contains("Edit:") && text.contains("lines.txt")
                && text.contains("+24/-24")
                && text.contains("-  1│1")
                && text.contains("- 16│16")
                && !text.contains("- 17│17")
                && text.contains("… 33 more lines (Ctrl + O to expand)")
        });
        pty.input("\x0f");
        pty.wait_for("long diff expanded", |screen| {
            let text = screen.contents();
            text.contains("+  1│A")
                && text.contains("- 24│24")
                && text.contains("+ 24│X")
                && text.contains("  25│keep")
                && !text.contains("(Ctrl + O to expand)")
        });
        assert!(
            diff_blocks_screen_colored(pty.parser.screen(), "A", DIFF_BLOCKS_ADDED),
            "expanded diff lost added-line color"
        );
        pty.input("\x0f");
        pty.wait_for("long diff folded again", |screen| {
            let text = screen.contents();
            !text.contains("- 17│17")
                && text.contains("-  1│1")
                && text.contains("… 33 more lines (Ctrl + O to expand)")
        });
        let (request, _) = pty.request_at(&server, path);
        diff_blocks_final(request, protocol, "diff-blocks long done");
        pty.text("diff-blocks long done");
        pty.ready();
        let (scrollback, raw) = diff_blocks_exit_scrollback(&mut pty, "diff-blocks long done");
        for expected in ["-  1│1", "- 16│16"] {
            assert!(scrollback.contains(expected), "{expected}\n{scrollback}");
        }
        assert!(
            scrollback.contains("(Ctrl + O to expand)")
                && !scrollback.contains("- 17│17")
                && !scrollback.contains("+  1│A"),
            "{scrollback}"
        );
        let mut parser = vt100::Parser::new(600, 100, 0);
        parser.process(&raw);
        assert!(
            diff_blocks_screen_colored(parser.screen(), "1", DIFF_BLOCKS_REMOVED),
            "scrollback lost removed-line color\n{scrollback}"
        );
        pty.finish();
        no_more(&server);
        assert_eq!(
            std::fs::read_to_string(project.path().join("lines.txt")).unwrap(),
            format!("{new}\nkeep\n")
        );
    }
}

// /resume 与启动恢复重放相同 diff 且不重新执行旧工具；缺 FileChanged 的旧日志
// 只显示工具结果摘要，不伪造 diff。
#[test]
fn transcript_blocks_diff_resume_replays_committed_diffs_without_reexecution() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            diff_blocks_settings("replace"),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        std::fs::write(project.path().join("demo.txt"), "old\nkeep\n").unwrap();
        let path = api_path(protocol);

        // Phase 1: 真实提交 edit 并退出保存。
        {
            let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
            pty.ready();
            pty.input("diff-resume edit demo\r");
            let (request, _) = pty.request_at(&server, path);
            diff_blocks_calls_reply(
                request,
                protocol,
                json!([diff_blocks_call(
                    protocol,
                    "e1",
                    "edit",
                    json!({"path":"demo.txt","edits":[{"oldText":"old","newText":"new"}]})
                )]),
            );
            let (request, _) = pty.request_at(&server, path);
            diff_blocks_final(request, protocol, "diff-resume answer");
            pty.text("diff-resume answer");
            pty.ready();
            pty.input("/quit\r");
            pty.finish();
            no_more(&server);
        }
        assert_eq!(
            std::fs::read_to_string(project.path().join("demo.txt")).unwrap(),
            "new\nkeep\n"
        );
        let id = saved_session_id(home.path());
        let journal = resume_history_journal_dir(home.path()).join(format!("{id}.jsonl"));

        // Phase 2: /resume 重放同一份 diff，无模型请求、无日志追加。
        {
            let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
            pty.ready();
            pty.input("/resume\r");
            pty.wait_for("resume panel lists the diff session", |screen| {
                let text = screen.contents();
                text.contains("Resume session")
                    && text.contains("diff-resume edit demo")
                    && text.lines().any(|line| line.starts_with('>'))
            });
            pty.input("\r");
            resume_history_wait_resumed(&mut pty, &id, &["diff-resume answer"]);
            no_more(&server);
            let journal_before = std::fs::read(&journal).unwrap();
            pty.wait_for("replayed edit diff card", |screen| {
                let text = screen.contents();
                text.contains("Edit:")
                    && text.contains("-  1│old")
                    && text.contains("+  1│new")
                    && diff_blocks_screen_colored(screen, "old", DIFF_BLOCKS_REMOVED)
                    && diff_blocks_screen_colored(screen, "new", DIFF_BLOCKS_ADDED)
            });
            assert_eq!(std::fs::read(&journal).unwrap(), journal_before);
            pty.input("/quit\r");
            pty.finish();
        }
        no_more(&server);

        // Phase 3: 启动恢复 --resume 重放相同 diff，也不重新执行。
        {
            let mut pty = Pty::start_with_args(project.path(), home.path(), &["--resume", &id]);
            pty.ready();
            pty.wait_for("restored transcript shows the same diff", |screen| {
                let text = screen.contents();
                text.contains("Edit:")
                    && text.contains("-  1│old")
                    && text.contains("+  1│new")
                    && diff_blocks_screen_colored(screen, "old", DIFF_BLOCKS_REMOVED)
                    && diff_blocks_screen_colored(screen, "new", DIFF_BLOCKS_ADDED)
            });
            no_more(&server);
            pty.input("/quit\r");
            pty.finish();
        }
        no_more(&server);
        assert_eq!(
            std::fs::read_to_string(project.path().join("demo.txt")).unwrap(),
            "new\nkeep\n"
        );

        // Phase 4: 删除 FileChanged 事件的旧日志只显示结果摘要，不伪造 diff。
        let stripped_id = resume_boundary_crafted_id(9, 'a');
        diff_blocks_clone_journal_without_file_changed(home.path(), &id, &stripped_id);
        {
            let mut pty =
                Pty::start_with_args(project.path(), home.path(), &["--resume", &stripped_id]);
            pty.ready();
            pty.wait_for("old journal replays summary without diff body", |screen| {
                let text = screen.contents();
                text.contains("Edit:")
                    && text.contains("edits_applied")
                    && !text.contains("+1/-1")
                    && !diff_blocks_screen_colored(screen, "old", DIFF_BLOCKS_REMOVED)
                    && !diff_blocks_screen_colored(screen, "new", DIFF_BLOCKS_ADDED)
            });
            no_more(&server);
            pty.input("/quit\r");
            pty.finish();
        }
        no_more(&server);
        assert_eq!(
            std::fs::read_to_string(project.path().join("demo.txt")).unwrap(),
            "new\nkeep\n"
        );
    }
}

// 真实无读权限（0200）文件的 write：快照读取失败不阻断已允许的写入，diff 显示
// 固定 unavailable 原因。root 下权限检查被绕过，跳过。
#[test]
fn transcript_blocks_diff_write_without_read_permission_reports_unavailable() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            diff_blocks_settings("replace"),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let locked = project.path().join("locked.txt");
        std::fs::write(&locked, "secret\n").unwrap();
        std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o200))
            .unwrap();
        let path = api_path(protocol);
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("diff-blocks locked write\r");
        let (request, _) = pty.request_at(&server, path);
        diff_blocks_calls_reply(
            request,
            protocol,
            json!([diff_blocks_call(
                protocol,
                "w1",
                "write",
                json!({"path":"locked.txt","content":"replacement\n"})
            )]),
        );
        pty.wait_for("unavailable diff on unreadable file", |screen| {
            let text = screen.contents();
            text.contains("write · locked.txt") && text.contains("replacement") && text.contains("Wrote 12 bytes")
        });
        // 恢复权限供清理与断言。
        std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o644))
            .unwrap();
        assert_eq!(std::fs::read_to_string(&locked).unwrap(), "replacement\n");
        let (request, body) = pty.request_at(&server, path);
        let written = diff_blocks_tool_result(&body, protocol, "w1");
        assert_eq!(written["ok"], json!(true), "{written}");
        diff_blocks_final(request, protocol, "diff-blocks locked done");
        pty.text("diff-blocks locked done");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }
}
