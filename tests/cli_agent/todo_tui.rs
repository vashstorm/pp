const TODO_TUI_BODY: &str = "## Write\n### [] Write canary bytes.\n- Acceptance: canary.txt contains smoke bytes.\n\n## Verify\n### [] Read back canary bytes.\n- Acceptance: read returns smoke bytes.\n";

fn plan_todo_result(body: &Value, protocol: &str, id: &str) -> Value {
    cancel_tool_outputs(body, protocol)
        .into_iter()
        .find(|(call_id, _)| call_id == id)
        .expect("native todo result")
        .1
}

/// 固定区内内容的物理行号；用相对行号断言位置，避免一行树形布局就重算全部坐标。
fn hud_row(screen: &vt100::Screen, needle: &str) -> Option<usize> {
    screen
        .rows(0, screen.size().1)
        .position(|row| row.contains(needle))
}

#[test]
fn tui_todo_new_and_mode_switches_keep_old_lists_cleared() {
    let protocol = "response";
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        for action in ["new", "plan"] {
            pty.input(&format!("create tasks before {action}\r"));
            let (request, _) = pty.request_at(&fixture, api_path(protocol));
            plan_calls(
                request,
                protocol,
                json!([plan_call(
                    protocol,
                    "init-before-switch",
                    "todo",
                    json!({"op":"init","items":["OLD_TODO_FIRST","OLD_TODO_SECOND"]})
                )]),
            );
            let (request, _) = pty.request_at(&fixture, api_path(protocol));
            pty.text("TODO · 0/2");
            plan_final(request, protocol, "Can you confirm?");
            pty.completed();
            assert!(!pty.parser.screen().contents().contains("Todo · init"));

            if action == "new" {
                pty.input("/new\r");
                pty.wait_for("新会话已重置", |screen| {
                    screen.contents().contains("▄▄▄██▀▀██") && input_is_ready(screen)
                });
            } else {
                for _ in 0..3 {
                    pty.input("/plan\r");
                    plan_wait_mode(&mut pty, "Plan");
                    assert!(!pty.parser.screen().contents().contains("TODO ·"));
                    pty.input("/plan\r");
                    plan_wait_execute(&mut pty);
                    assert!(!pty.parser.screen().contents().contains("TODO ·"));
                }
            }

            // 再次请求工具并消费最终 workflow，验证旧清单没有仅在界面暂时隐藏。
            pty.input("inspect tasks after switching\r");
            let (request, _) = pty.request_at(&fixture, api_path(protocol));
            plan_calls(
                request,
                protocol,
                json!([plan_call(protocol, "view-after-switch", "todo", json!({"op":"view"}))]),
            );
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            let result = plan_todo_result(&body, protocol, "view-after-switch");
            assert_eq!(result["ok"], true);
            assert!(result.get("phases").is_none());
            assert!(result["content"].as_str().unwrap().contains("empty"));
            plan_final(request, protocol, "No previous tasks remain.");
            pty.completed();
            assert!(!pty.parser.screen().contents().contains("TODO ·"));
            assert!(!pty.parser.screen().contents().contains("Todo · view"));
        }
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
}

// 三种执行起点（新会话 / 保留上下文 / 压缩后）各自独立，均验证 todo 快照、
// 完成提醒与归档字节一致。
fn plan_todo_approval_case(context: usize) {
    let protocol = "response";
        let fixture = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let mut pty = plan_start_with_role_models(&fixture, home.path(), project.path(), protocol);
            plan_enter(&mut pty, &fixture);
            if context == 2 {
                pty.input("Research previous context for compaction\r");
                let (request, planning) = pty.request_at(&fixture, api_path(protocol));
                assert_eq!(planning["model"], "planner-model");
                plan_final(request, protocol, "PRE_COMPACT_RESEARCH_CONTEXT");
                pty.ready();
            }
            plan_drive_ready(
                &mut pty,
                &fixture,
                protocol,
                "exercise todo plan",
                TODO_TUI_BODY,
            );
            let source_id = saved_session_id(home.path());
            let source = plan_journal(home.path(), &source_id);
            assert!(!source.iter().any(|record| {
                record["data"]["event"]["todo_snapshot"].is_object()
            }));
            let workflow = source.iter().rev()
                .find(|record| record["customType"] == "pp.workflow").unwrap();
            assert!(workflow["data"]["workflow"]["todos"].is_null());
            assert!(workflow["data"]["workflow"]["todo_plan_id"].is_null());
            assert_eq!(
                std::fs::read_to_string(project.path().join(".pp/drafts/create-canary.md"))
                    .unwrap(),
                TODO_TUI_BODY
            );
            plan_choose(&mut pty, context);
            if context == 2 {
                let (request, summary) = pty.request_at(&fixture, api_path(protocol));
                assert_eq!(summary["model"], "planner-model");
                assert!(
                    summary
                        .to_string()
                        .contains("Summarize the preceding conversation")
                );
                plan_final(request, protocol, "Compact approved planning context.");
            }
            let (request, execution) = pty.request_at(&fixture, api_path(protocol));
            assert_plan_execute_route(&execution, protocol);
            let execution_id = if context == 0 {
                resume_boundary_session_ids(home.path())
                    .into_iter()
                    .find(|id| id != &source_id)
                    .unwrap()
            } else {
                source_id.clone()
            };
            let executing = plan_journal(home.path(), &execution_id);
            let workflow = executing
                .iter()
                .rev()
                .find(|record| {
                    matches!(
                        record["customType"].as_str(),
                        Some("pp.workflow" | "pp.session_state")
                    )
                })
                .unwrap();
            assert_eq!(
                workflow["data"]["workflow"]["todos"][0]["tasks"][0]["status"],
                "in_progress"
            );
            plan_calls(
                request,
                protocol,
                json!([
                    plan_call(protocol, "initial-todo", "todo", json!({"op":"view"})),
                    plan_call(
                        protocol,
                        "canary-write",
                        "write",
                        json!({"path":"canary.txt","content":"smoke bytes"})
                    ),
                    plan_call(
                        protocol,
                        "canary-read",
                        "read",
                        json!({"path":"canary.txt"})
                    ),
                    plan_call(
                        protocol,
                        "first-done",
                        "todo",
                        json!({"op":"done","task":"Write canary bytes."})
                    ),
                    plan_call(protocol, "after-done", "todo", json!({"op":"view"}))
                ]),
            );
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            let initial = plan_todo_result(&body, protocol, "initial-todo");
            assert!(initial.get("phases").is_none());
            assert!(
                initial["content"]
                    .as_str()
                    .unwrap()
                    .contains("- [ ] Write canary bytes. (in progress)")
            );
            let after = plan_todo_result(&body, protocol, "after-done");
            assert!(after.get("phases").is_none());
            let after_content = after["content"].as_str().unwrap();
            assert!(after_content.contains("- [X] Write canary bytes."));
            assert!(after_content.contains("- [ ] Read back canary bytes. (in progress)"));
            pty.text("TODO · 1/2");
            assert!(!pty.parser.screen().contents().contains("Todo · done"));
            assert!(!pty.parser.screen().contents().contains("Todo · view"));
            assert_eq!(
                std::fs::read(project.path().join("canary.txt")).unwrap(),
                b"smoke bytes"
            );
            assert!(
                plan_todo_result(&body, protocol, "canary-read")["content"]
                    .as_str()
                    .unwrap()
                    .contains("smoke bytes")
            );
            assert_eq!(
                std::fs::read_to_string(project.path().join(".pp/plans/create-canary.md")).unwrap(),
                TODO_TUI_BODY.replacen("[]", "[x]", 1)
            );
            plan_final(request, protocol, "Work stopped early.");
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            let history = body[if protocol == "completion" {
                "messages"
            } else {
                "input"
            }]
            .as_array()
            .unwrap();
            let reminder = history
                .iter()
                .rev()
                .find(|item| item["role"] == "developer")
                .unwrap()["content"]
                .as_str()
                .unwrap();
            assert!(reminder.contains("Read back canary bytes."));
            assert!(!reminder.contains("Write canary bytes."));
            plan_calls(
                request,
                protocol,
                json!([plan_call(
                    protocol,
                    "second-done",
                    "todo",
                    json!({"op":"done","task":"Read back canary bytes."})
                )]),
            );
            let (request, _) = pty.request_at(&fixture, api_path(protocol));
            plan_final(request, protocol, "All tasks complete.");
            pty.completed();
            assert!(!project.path().join(".pp/plans/create-canary.md").exists());
            assert_eq!(
                std::fs::read_to_string(project.path().join(".pp/done/create-canary.md")).unwrap(),
                TODO_TUI_BODY.replace("[]", "[x]")
            );
        pty.input("/plan view\r");
        pty.wait_for("todo completed plan panel", |screen| {
            let text = screen.contents();
            text.contains("Completed")
                && text.contains("2/2")
                && text.contains(".pp/done/create-canary.md")
        });
        pty.input("\x1b");
        no_more(&fixture);
        pty.input("\x03");
        pty.finish();
}

#[test]
fn tui_todo_fixed_hud_streams_resizes_restores_and_clears_both_protocols() {
    let protocol = "response";
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        pty.input("fixed todo exercise\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let items: Vec<_> = (0..12).map(|i| format!("PINNED_TASK_{i:02}")).collect();
        plan_calls(
            request,
            protocol,
            json!([plan_call(
                protocol,
                "init-fixed",
                "todo",
                json!({"op":"init","items":items})
            )]),
        );
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        // 模型下一轮尚未结束时，已提交清单就应出现在固定位置。
        // 收起状态也保留阶段行：标题下面是阶段头，再往下才是内缩的任务行。
        pty.wait_for("任务区固定在输入框上方", |screen| {
            hud_row(screen, "TODO · 0/12").is_some_and(|title| {
                screen
                    .rows(0, 100)
                    .nth(title + 1)
                    .is_some_and(|row| row.contains("I. Tasks (0/12)"))
                    && screen
                        .rows(0, 100)
                        .nth(title + 2)
                        .is_some_and(|row| row.contains("PINNED_TASK_00"))
            })
        });
        let toggle_hint = if cfg!(target_os = "macos") { "⌥ + T" } else { "Ctrl + T" };
        let title = hud_row(pty.parser.screen(), "TODO · 0/12").expect("TODO 标题") as u16;
        assert!(
            pty.parser
                .screen()
                .rows(0, 100)
                .nth(usize::from(title))
                .unwrap()
                .contains(toggle_hint)
        );
        assert!(
            pty.parser
                .screen()
                .rows(0, 100)
                .nth(usize::from(title) - 1)
                .unwrap()
                .trim()
                .is_empty()
        );
        for (row, column, symbol) in [
            (title + 1, 0, "├"),
            (title + 1, 1, "─"),
            (title + 2, 0, "│"),
            (title + 2, 3, "├"),
            (title + 4, 3, "└"),
        ] {
            let cell = pty.parser.screen().cell(row, column).unwrap();
            assert_eq!(cell.contents(), symbol);
            assert_eq!(cell.fgcolor(), vt100::Color::Rgb(95, 102, 115));
        }
        let current = pty.parser.screen().cell(title + 2, 6).unwrap();
        assert_eq!(current.contents(), "[");
        assert_eq!(current.fgcolor(), vt100::Color::Rgb(255, 230, 109));
        assert!(
            pty.parser
                .screen()
                .rows(0, 100)
                .nth(usize::from(title) + 4)
                .unwrap()
                .contains("PINNED_TASK_02")
        );
        assert_eq!(
            pty.parser.screen().cell(title + 3, 10).unwrap().fgcolor(),
            vt100::Color::Rgb(119, 125, 136)
        );
        // 启动两个并行项；首项遇到依赖后，视口跟随最早的剩余进行项。
        plan_calls(request, protocol, json!([
            plan_call(protocol, "start-middle", "todo", json!({"op":"start","task":"PINNED_TASK_06"})),
            plan_call(protocol, "start-parallel", "todo", json!({"op":"start","task":"PINNED_TASK_07"})),
            plan_call(protocol, "block-first", "todo", json!({"op":"block","task":"PINNED_TASK_00","reason":"waiting on dependency"}))
        ]));
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        pty.wait_for("进行项在三项窗口中间", |screen| {
            hud_row(screen, "TODO · 0/12").is_some_and(|title| {
                ["PINNED_TASK_05", "PINNED_TASK_06", "PINNED_TASK_07"]
                    .iter()
                    .enumerate()
                    .all(|(offset, task)| {
                        screen
                            .rows(0, 100)
                            .nth(title + 2 + offset)
                            .is_some_and(|row| row.contains(task))
                    })
            })
        });
        let title = hud_row(pty.parser.screen(), "TODO · 0/12").expect("TODO 标题") as u16;
        for offset in [3, 4] {
            assert_eq!(
                pty.parser.screen().cell(title + offset, 10).unwrap().fgcolor(),
                vt100::Color::Rgb(255, 230, 109)
            );
        }
        let mut stream = request.start_sse().unwrap();
        let answer = (0..70)
            .map(|i| format!("    pinned-stream-{i:03}\n"))
            .collect::<String>()
            + "\nCan you confirm?";
        streaming_delta(&mut stream, protocol, &answer);
        pty.text("pinned-stream-069");
        pty.input("中文草稿");
        pty.draft("中文草稿");
        let title = hud_row(pty.parser.screen(), "TODO · 0/12").expect("TODO 标题");
        assert!(
            pty.parser
                .screen()
                .rows(0, 100)
                .nth(title - 1)
                .unwrap()
                .trim()
                .is_empty()
        );
        pty.function_key('t');
        // 展开后 HUD 向上生长，标题仍在主屏幕且当前进行项可见。
        pty.wait_for("TODO 原位展开", |screen| {
            hud_row(screen, "TODO · 0/12").is_some_and(|title| {
                screen
                    .rows(0, 100)
                    .nth(title + 1)
                    .is_some_and(|row| row.contains("│") && row.contains("├"))
            }) && screen.contents().contains("PINNED_TASK_06")
        });
        let screen = pty.parser.screen();
        let title = hud_row(screen, "TODO · 0/12").expect("TODO 标题") as u16;
        assert!(
            screen
                .rows(0, 100)
                .nth(usize::from(title))
                .unwrap()
                .contains(&format!("{toggle_hint} collapse"))
        );
        assert_eq!(screen.cell(title + 1, 0).unwrap().contents(), "│");
        assert_eq!(screen.cell(title + 1, 3).unwrap().contents(), "├");
        assert!(!pty.parser.screen().alternate_screen());
        pty.resize(24, 60);
        pty.wait_for("缩放后的 TODO 高度上限", |screen| {
            let Some(title) = hud_row(screen, "TODO · 0/12") else {
                return false;
            };
            let input = screen
                .rows(0, 60)
                .position(|row| row.contains('❯'))
                .unwrap_or(usize::MAX);
            let hud_rows = screen
                .rows(0, 60)
                .skip(title)
                .take(input.saturating_sub(title))
                .filter(|row| row.contains("TODO ·") || row.contains("PINNED_TASK_"))
                .count();
            // 高度上限为终端高度的三分之一（24 / 3 = 8）；标题与输入框之间
            // 还隔着活动区上方的一行空白分隔行、活动行和状态行各一行。
            title < input
                && hud_rows <= 8
                && input - title <= 11
                && screen
                    .rows(0, 60)
                    .nth(title)
                    .is_some_and(|row| row.contains("collapse"))
        });
        pty.draft("中文草稿");
        pty.function_key('t');
        pty.wait_for("TODO 收回五行树形摘要", |screen| {
            hud_row(screen, "TODO · 0/12").is_some_and(|title| {
                screen
                    .rows(0, 60)
                    .nth(title + 1)
                    .is_some_and(|row| row.contains("I. Tasks (0/12)"))
                    && screen
                        .rows(0, 60)
                        .nth(title + 4)
                        .is_some_and(|row| row.contains("PINNED_TASK_"))
                    && screen
                        .rows(0, 60)
                        .nth(title + 5)
                        .is_some_and(|row| !row.contains("PINNED_TASK_"))
            })
        });
        streaming_complete(&mut stream, protocol, &answer);
        stream.finish();
        pty.completed();
        let id = saved_session_id(home.path());
        let journal = plan_journal(home.path(), &id);
        assert!(
            !journal
                .iter()
                .any(|entry| entry["data"]["event"]["todo_state_updated"].is_object())
        );
        // native_history 包含当前屏幕；这里只检查真正进入滚动历史的行。
        let mut screen = pty.parser.screen().clone();
        screen.set_scrollback(usize::MAX);
        let history = (1..=screen.scrollback())
            .rev()
            .map(|offset| {
                screen.set_scrollback(offset);
                screen.rows(0, screen.size().1).next().unwrap()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !history.contains("PINNED_TASK_"),
            "固定区进入原生历史：{history}"
        );
        let history = pty.native_history().0;
        assert_eq!(history.matches("pinned-stream-000").count(), 1);
        assert_eq!(history.matches("PINNED_TASK_05").count(), 1);
        let before_exit = pty.raw.len();
        pty.input("\x03");
        pty.wait_for("退出恢复终端", |screen| !screen.bracketed_paste());
        assert!(!String::from_utf8_lossy(&pty.raw[before_exit..]).contains("pinned-stream-000"));
        pty.finish();
        no_more(&fixture);

        let mut pty = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
        pty.wait_for("恢复最终清单并默认收起", |screen| {
            hud_row(screen, "TODO · 0/12").is_some_and(|title| {
                screen
                    .rows(0, 100)
                    .nth(title + 1)
                    .is_some_and(|row| row.contains("I. Tasks (0/12)"))
            }) && screen.contents().contains("PINNED_TASK_06")
                && screen.contents().contains("PINNED_TASK_07")
                && !screen.contents().contains("PINNED_TASK_08")
        });
        pty.input("finish all tasks\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        plan_calls(
            request,
            protocol,
            json!([plan_call(
                protocol,
                "done-fixed",
                "todo",
                json!({"op":"done"})
            )]),
        );
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        pty.wait_for("完成后保留三项绿色任务", |screen| {
            hud_row(screen, "TODO · 12/12").is_some_and(|title| {
                screen
                    .rows(0, 100)
                    .nth(title)
                    .is_some_and(|row| row.contains("Complete"))
                    && (0..3).all(|offset| {
                        screen
                            .rows(0, 100)
                            .nth(title + 2 + offset)
                            .is_some_and(|row| {
                                row.contains(&format!("[\u{f00c}] PINNED_TASK_{:02}", 9 + offset))
                            })
                    })
            })
        });
        let title = hud_row(pty.parser.screen(), "TODO · 12/12").expect("TODO 标题") as u16;
        let screen = pty.parser.screen();
        assert_eq!(screen.cell(title, 0).unwrap().contents(), "\u{f00c}");
        assert_eq!(screen.cell(title, 0).unwrap().fgcolor(), vt100::Color::Rgb(137, 210, 129));
        // 阶段头与最后一个任务行都使用右结尾树枝。
        assert_eq!(screen.cell(title + 1, 0).unwrap().contents(), "├");
        assert_eq!(screen.cell(title + 4, 3).unwrap().contents(), "└");
        assert_eq!(screen.cell(title + 4, 10).unwrap().fgcolor(), vt100::Color::Rgb(137, 210, 129));
        plan_final(request, protocol, "Fixed tasks finished.");
        pty.completed();
        pty.input("clear the list\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        plan_calls(
            request,
            protocol,
            json!([plan_call(
                protocol,
                "clear-fixed",
                "todo",
                json!({"op":"init","list":[]})
            )]),
        );
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        pty.wait_for("空清单不占位置", |screen| {
            !screen.contents().contains("TODO ·")
        });
        plan_final(request, protocol, "Fixed list cleared.");
        pty.completed();
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
}

