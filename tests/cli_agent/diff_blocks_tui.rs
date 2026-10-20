// Diff 块级渲染 PTY 回归：两种 edit 模式／两协议的真实提交 diff（磁盘落盘 + 屏幕
// 红/绿与行号 + 统计）、write 新建/覆盖/同内容/空文件、CRLF→LF／末尾换行删除／BOM
// /非 UTF-8 的 unavailable 回退、无读权限文件的 write 不被快照失败阻断、失败 edit
// 不伪造 diff、超过 10 行的默认折叠与 Ctrl-O 展开/收起、退出 scrollback 完整展开
// 保留颜色，以及 /resume 与启动恢复重放相同 diff 且不重新执行旧工具、缺 FileChanged
// 的旧日志正常加载。全部通过真实 PTY、loopback HTTP 与磁盘文件观察，不用 sleep 猜
// 测阶段；辅助函数一律使用 `diff_blocks_` 前缀，测试函数使用 `transcript_blocks_`
// 前缀，不新增 use 声明以免与兄弟 include 文件冲突。

const DIFF_BLOCKS_ADDED: vt100::Color = vt100::Color::Rgb(137, 210, 129);
const DIFF_BLOCKS_REMOVED: vt100::Color = vt100::Color::Rgb(252, 58, 75);
const DIFF_BLOCKS_ACCENT: vt100::Color = vt100::Color::Rgb(254, 188, 56);

fn diff_blocks_settings(mode: &str) -> String {
    format!(
        r#"{{"default_model":"fixture/primary","edit":"{mode}","notification_timeout_seconds":1}}"#
    )
}

// 断言 needle 出现在屏幕上且其中每个单元格的前景色都等于 color。needle 仅使用
// ASCII，且要求匹配点之前的行前缀全是 ASCII，保证字节偏移等于终端列号。
fn diff_blocks_screen_colored(screen: &vt100::Screen, needle: &str, color: vt100::Color) -> bool {
    let (rows, columns) = screen.size();
    let width = needle.chars().count() as u16;
    for row in 0..rows {
        let line: String = (0..columns)
            .filter_map(|column| screen.cell(row, column))
            .map(|cell| cell.contents())
            .collect();
        for (start, _) in line.match_indices(needle) {
            if !line[..start].is_ascii() {
                continue;
            }
            let start = start as u16;
            if (start..start + width).all(|column| {
                screen
                    .cell(row, column)
                    .is_some_and(|cell| cell.fgcolor() == color)
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

// Ctrl-C 退出后解析真实终端 scrollback dump，返回纯文本与原始字节（用于颜色断言）。
fn diff_blocks_exit_scrollback(pty: &mut Pty, last_answer: &str) -> (String, Vec<u8>) {
    pty.input("\x03");
    pty.wait_for("leaving the alternate screen", |screen| {
        !screen.alternate_screen()
    });
    let marker = b"\x1b[?1049l";
    let start = pty
        .raw
        .windows(marker.len())
        .rposition(|bytes| bytes == marker)
        .unwrap();
    let deadline = Instant::now() + DEADLINE;
    loop {
        pty.drain();
        let tail = pty.raw[start + marker.len()..].to_vec();
        let mut parser = vt100::Parser::new(600, 100, 0);
        parser.process(&tail);
        let contents = parser.screen().contents();
        // 状态栏的 fixture-model 是 dump 结束的观察栅栏。
        if let (Some(answer), Some(footer)) =
            (contents.rfind(last_answer), contents.rfind("fixture-model"))
            && footer > answer
        {
            return (contents, tail);
        }
        assert!(
            Instant::now() < deadline,
            "incomplete exit scrollback\n{}",
            pty.diagnostics()
        );
        thread::sleep(Duration::from_millis(10));
    }
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
            config_path(home.path(), "settings.jsonc"),
            diff_blocks_settings("replace"),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.jsonc"),
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
            text.contains("edit · demo.txt")
                && text.contains("+1 -1")
                && text.contains("  1 - old")
                && text.contains("  1 + new")
                && text.contains("  2   keep")
                && diff_blocks_screen_colored(screen, "old", DIFF_BLOCKS_REMOVED)
                && diff_blocks_screen_colored(screen, "new", DIFF_BLOCKS_ADDED)
                && diff_blocks_screen_colored(screen, "edit · demo.txt", DIFF_BLOCKS_ACCENT)
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
                && text.contains("+1 -0")
                && text.contains("  1 + alpha")
                && diff_blocks_screen_colored(screen, "alpha", DIFF_BLOCKS_ADDED)
                && text.contains("write · empty.txt")
                && text.contains("Created empty file.")
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
            text.contains("No content changes.")
                && text.contains("+0 -0")
                && text.contains("  3 + beta")
                && diff_blocks_screen_colored(screen, "beta", DIFF_BLOCKS_ADDED)
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
            config_path(home.path(), "settings.jsonc"),
            diff_blocks_settings("hashline"),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.jsonc"),
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
            .split_once('|')
            .unwrap()
            .0
            .to_owned();
        assert!(anchor.starts_with("1:"), "{anchor}");
        diff_blocks_calls_reply(
            request,
            protocol,
            json!([diff_blocks_call(
                protocol,
                "e1",
                "edit",
                json!({"path":"demo.txt","edits":[
                    {"op":"replace","start":anchor,"end":anchor,"lines":["new"]}
                ]})
            )]),
        );
        pty.wait_for("committed hashline edit diff card", |screen| {
            let text = screen.contents();
            text.contains("edit · demo.txt")
                && text.contains("+1 -1")
                && text.contains("  1 - old")
                && text.contains("  1 + new")
                && text.contains("  2   keep")
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
// BOM 剥离产生真实 +1 -1、非 UTF-8 覆盖成功且显示 unavailable 回退。
#[test]
fn transcript_blocks_diff_endings_bom_and_unavailable() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(home.path(), "settings.jsonc"),
            diff_blocks_settings("replace"),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.jsonc"),
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
        pty.wait_for("line-ending diff hints", |screen| {
            let text = screen.contents();
            text.contains("write · eol.txt")
                && text.contains("  CRLF")
                && text.contains("  LF")
                && text.contains("write · nonl.txt")
                && text.contains("No newline at end of file")
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
        pty.wait_for("BOM strip and non-UTF-8 fallback cards", |screen| {
            let text = screen.contents();
            text.contains("write · bom.txt")
                && text.contains("+1 -1")
                && text.contains("  2   keep")
                && text
                    .lines()
                    .any(|line| line.contains("  1 -") && line.contains("bom"))
                && text
                    .lines()
                    .any(|line| line.contains("  1 +") && line.trim().ends_with("bom"))
                && text.contains("write · bin.dat")
                && text.contains("Previous content is not UTF-8; text diff unavailable.")
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
                config_path(home.path(), "settings.jsonc"),
                diff_blocks_settings(mode),
            )
            .unwrap();
            std::fs::write(
                config_path(home.path(), "models.jsonc"),
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
                    .split_once('|')
                    .unwrap()
                    .0
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
                        json!({"path":"demo.txt","edits":[
                            {"op":"replace","start":anchor,"end":anchor,"lines":["new"]}
                        ]})
                    )]),
                );
                "hashline mismatch at line"
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
                text.contains("edit · demo.txt")
                    && text.contains(error_needle)
                    && !text.contains("+1 -1")
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

// 超过 10 行的 diff 默认折叠，Ctrl-O 展开/收起，退出 scrollback 展开全部并保留颜色。
#[test]
fn transcript_blocks_diff_long_diff_collapses_expands_and_scrolls_back() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(home.path(), "settings.jsonc"),
            diff_blocks_settings("replace"),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.jsonc"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let old = (1..=12)
            .map(|number| number.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let new = (b'A'..=b'L')
            .map(|byte| char::from(byte).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(project.path().join("lines.txt"), format!("{old}\nkeep\n")).unwrap();
        let path = api_path(protocol);
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
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
            text.contains("edit · lines.txt")
                && text.contains("+12 -12")
                && text.contains("  1 - 1")
                && text.contains(" 10 - 10")
                && !text.contains(" 11 - 11")
                && !text.contains("  1 + A")
                && text.contains("… 15 more lines (Ctrl-O to expand)")
        });
        pty.input("\x0f");
        pty.wait_for("long diff expanded", |screen| {
            let text = screen.contents();
            text.contains("  1 + A")
                && text.contains(" 12 - 12")
                && text.contains(" 12 + L")
                && text.contains(" 13   keep")
                && !text.contains("(Ctrl-O to expand)")
        });
        pty.input("\x0f");
        pty.wait_for("long diff folded again", |screen| {
            let text = screen.contents();
            !text.contains("  1 + A")
                && text.contains("  1 - 1")
                && text.contains("… 15 more lines (Ctrl-O to expand)")
        });
        let (request, _) = pty.request_at(&server, path);
        diff_blocks_final(request, protocol, "diff-blocks long done");
        pty.text("diff-blocks long done");
        pty.ready();
        let (scrollback, raw) = diff_blocks_exit_scrollback(&mut pty, "diff-blocks long done");
        for expected in ["  1 - 1", "  1 + A", " 12 - 12", " 12 + L", " 13   keep"] {
            assert!(scrollback.contains(expected), "{expected}\n{scrollback}");
        }
        assert!(!scrollback.contains("(Ctrl-O to expand)"), "{scrollback}");
        let mut parser = vt100::Parser::new(600, 100, 0);
        parser.process(&raw);
        assert!(
            diff_blocks_screen_colored(parser.screen(), "12", DIFF_BLOCKS_REMOVED),
            "scrollback lost removed-line color\n{scrollback}"
        );
        assert!(
            diff_blocks_screen_colored(parser.screen(), "L", DIFF_BLOCKS_ADDED),
            "scrollback lost added-line color\n{scrollback}"
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
            config_path(home.path(), "settings.jsonc"),
            diff_blocks_settings("replace"),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.jsonc"),
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
                text.contains("edit · demo.txt")
                    && text.contains("  1 - old")
                    && text.contains("  1 + new")
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
                text.contains("edit · demo.txt")
                    && text.contains("  1 - old")
                    && text.contains("  1 + new")
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
                text.contains("edit · demo.txt")
                    && text.contains("edits_applied")
                    && !text.contains("+1 -1")
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
            config_path(home.path(), "settings.jsonc"),
            diff_blocks_settings("replace"),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.jsonc"),
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
            text.contains("write · locked.txt")
                && text.contains("Previous content could not be read; text diff unavailable.")
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
