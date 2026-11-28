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

include!("diff_blocks_tui/preview_cases.rs");

fn diff_blocks_settings(mode: &str) -> String {
    format!(
        r#"{{"default_model":"fixture/primary","edit":"{mode}","notification_timeout_seconds":1,"read":{{"toolResultPreview":true}}}}"#
    )
}

// 直接按终端单元格匹配 ASCII 文本及颜色，避免标题图标影响列号计算。
fn diff_blocks_screen_colored(screen: &vt100::Screen, needle: &str, color: vt100::Color) -> bool {
    // edit 使用参考图色板，局部变更以背景强调。
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

fn diff_blocks_calls_reply(request: CapturedRequest, protocol: &str, calls: Value) {
    if protocol == "completion" {
        calls_reply(request, calls);
    } else {
        response_reply(request, calls);
    }
}

fn diff_blocks_final(request: CapturedRequest, protocol: &str, text: &str) {
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
    let text = session_text(&journal);
    let mut lines = text.lines();
    let mut header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    header["id"] = json!(id);
    let mut cloned = format!("{header}\n");
    let mut parent = Value::Null;
    let mut event_map = Vec::new();
    let mut retained = 0;
    for line in lines {
        let mut entry: Value = serde_json::from_str(line).unwrap();
        if entry["data"]["event"] == "assistant_started" {
            event_map.clear(); retained = 0;
        } else if entry["customType"] == "pp.event" {
            event_map.push(Some(retained)); retained += 1;
        }
        if let Some(order) = entry["message"]["ppReplay"]["order"].as_array_mut() {
            order.retain_mut(|step| {
                if step["kind"] != "event" { return true; }
                let old = step["index"].as_u64().unwrap() as usize;
                match event_map[old] {
                    Some(index) => { step["index"] = json!(index); true }
                    None => false,
                }
            });
        }
        entry["parentId"] = parent;
        parent = entry["id"].clone();
        cloned.push_str(&entry.to_string());
        cloned.push('\n');
    }
    write_session_text(
        &resume_history_journal_dir(home).join(format!("{id}.jsonl")),
        &cloned,
    );
}

// replace 模式的真实 edit old→new 与 write 新建/覆盖/同内容/空文件：屏幕红绿、
// 行号、+N -N 统计和磁盘结果一致。
#[test]
fn transcript_blocks_diff_replace_edit_and_write_cards() {
    let protocol = "response";
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
            text.contains("Write · fresh.txt")
                && text.contains("alpha")
                && text.contains("Wrote 6 bytes")
                && text.contains("Write · empty.txt")
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

