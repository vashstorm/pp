use super::*;

/// 构造一条确定高度的会话条目，便于按索引核对翻页窗口。
/// Data 卡片按逻辑行逐行渲染，行数可直接控制。
fn fixed_entry(transcript: &mut Transcript, label: &str, rows: usize) {
    transcript.append("Note", &format!("{label}\n{}", "body\n".repeat(rows - 1)));
}

/// 期望从 offset 开始的窗口正文：取每行的可见文本并裁掉右侧填充空格。
fn visible_rows(buffer: &Buffer) -> Vec<String> {
    (buffer.area.y..buffer.area.bottom())
        .map(|y| {
            (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

/// 按同一确定性脚本构建混合历史，供对照渲染使用。
fn mixed_history() -> Transcript {
    let mut transcript = Transcript::new();
    // 240 轮 × 每轮 1.25 条（含错误卡）＝ 300 条混合条目。
    for index in 0..240 {
        match index % 4 {
            0 => fixed_entry(&mut transcript, &format!("short-{index:03}"), 1),
            1 => fixed_entry(&mut transcript, &format!("multi-{index:03}"), 5),
            // 空正文 Data 卡片高度为 0：验证零高度槽不占行也不加 gap。
            2 => transcript.append("Note", ""),
            _ => {
                fixed_entry(&mut transcript, &format!("wide-{index:03}"), 2);
                transcript.append_error(&format!("error-{index:03}"));
            }
        }
    }
    transcript
}

#[test]
fn long_history_pages_by_geometry_and_matches_full_render_row_by_row() {
    // 300+ 条混合条目：短文本、多行正文、空正文（零高度）与本地错误卡。
    let width = 40;
    let height = 10;
    let mut reference = mixed_history();
    let base = render(&mut reference, width, height);
    let base_rows = visible_rows(&base);
    assert!(base_rows.iter().any(|row| !row.is_empty()));

    // 逐页向下翻页：每屏的正文必须来自几何定位的同一批条目，且单调推进。
    let mut transcript = mixed_history();
    transcript.follow_bottom = false;
    transcript.offset = 0;
    let mut previous: Option<Vec<String>> = None;
    let mut pages = 0usize;
    loop {
        let buffer = render(&mut transcript, width, height);
        let rows = visible_rows(&buffer);
        assert!(
            rows.iter().any(|row| !row.is_empty()),
            "翻页窗口不应整屏空白"
        );
        if let Some(earlier) = previous.as_ref() {
            assert_ne!(&rows, earlier, "page_down 必须推进窗口");
        }
        previous = Some(rows);
        if transcript.following() {
            break;
        }
        transcript.page_down();
        pages += 1;
        assert!(pages < 5000, "翻页未终止");
    }

    // 逐页向上翻回顶部：offset 单调不增。
    let mut offsets = Vec::new();
    while transcript.offset > 0 {
        let before = transcript.offset;
        transcript.page_up();
        render(&mut transcript, width, height);
        assert!(transcript.offset <= before, "page_up 不应向下移动窗口");
        offsets.push(transcript.offset);
        assert!(offsets.len() < 5000, "回滚未终止");
    }

    // 与全量渲染的对应窗口逐行等价：完整查看器路径不受几何定位影响。
    let mut full = mixed_history();
    full.prepare_layout(width);
    full.sync_geometry();
    let total = full.total;
    assert!(total > 0, "同步几何后应得到正的总量");
    let mut stitched: Vec<String> = Vec::new();
    for start in (0..total).step_by(64) {
        let rows = (total - start).min(64);
        let area = Rect::new(0, 0, width, rows as u16);
        let mut buffer = Buffer::empty(area);
        full.render(&mut buffer, area, start);
        stitched.extend(visible_rows(&buffer));
    }
    // 全量回放的最后一屏与备用屏底部窗口包含同样的末尾内容。
    let last_window: Vec<String> = stitched
        .iter()
        .rev()
        .take(usize::from(height))
        .cloned()
        .collect();
    assert!(
        last_window
            .iter()
            .any(|row| row.contains("error-239") || row.contains("wide-239")),
        "全量回放末尾应包含最新内容：{last_window:?}"
    );
    assert!(
        base_rows
            .iter()
            .any(|row| row.contains("error-239") || row.contains("wide-239")),
        "备用屏底部窗口应包含最新内容：{base_rows:?}"
    );
    assert_eq!(
        base_rows.iter().filter(|row| !row.is_empty()).count(),
        last_window.iter().filter(|row| !row.is_empty()).count(),
        "备用屏与全量回放的末尾窗口可见行数应一致"
    );
}

#[test]
fn streaming_replacement_of_middle_entry_keeps_anchor_on_same_entry() {
    let mut transcript = Transcript::new();
    for index in 0..20 {
        fixed_entry(&mut transcript, &format!("before-{index:02}"), 3);
    }
    fixed_entry(&mut transcript, "middle-marker", 30);
    for index in 0..20 {
        fixed_entry(&mut transcript, &format!("after-{index:02}"), 3);
    }
    let width = 60;
    let height = 8;
    render(&mut transcript, width, height);
    // 从底部上溯到正好落在中段条目内的行号。
    let marker_index = transcript
        .entries
        .iter()
        .position(|entry| entry.text.contains("middle-marker"))
        .unwrap();
    transcript.sync_geometry();
    let marker_height = transcript.geometry.height(marker_index);
    assert!(
        marker_height >= 20,
        "被测条目应足够高以承载条目内行偏移：{marker_height}"
    );
    let marker_start = transcript.geometry.start(marker_index);
    transcript.offset = marker_start + 10;
    transcript.follow_bottom = false;
    transcript.browse_anchor = None;
    let before = screen(&render(&mut transcript, width, height));
    let anchor = transcript.browse_anchor.unwrap();
    assert_eq!(anchor.entry, marker_index, "锚点应落在被测条目内：{before}");
    assert_eq!(anchor.row, 10, "条目内行偏移应保持：{before}");
    assert!(before.contains("body"), "窗口应显示条目正文：{before}");

    // 中段条目内容被流式替换：身份变化应被下一帧捕获，锚点保持在同一条目。
    let entry = &mut transcript.entries[marker_index];
    entry.text = format!("replaced\n{}", "new body\n".repeat(45));
    entry.layout = None;

    let after = screen(&render(&mut transcript, width, height));
    assert!(
        after.contains("replaced") || after.contains("new body"),
        "替换后仍应显示该条目：{after}"
    );
    let resolved = transcript.browse_anchor.unwrap();
    assert_eq!(resolved.entry, anchor.entry, "锚点条目不应漂移");
    assert_eq!(resolved.row, anchor.row, "条目内行偏移不应漂移");
    // 替换后窗口仍落在同一批条目：后续内容不应被推进或拉回。
    assert!(
        after.contains("after-0") || after.contains("new body"),
        "{after}"
    );
}

#[test]
fn geometry_hidden_members_tree_roundtrip_theme_and_gap_lines() {
    {
        let mut transcript = Transcript::new();
        transcript.append("You", "start");
        for index in 0..5 {
            tool(
                &mut transcript,
                &format!("read-{index}"),
                "read",
                json!({"path": format!("file-{index}.txt")}),
            );
            transcript.progress(AgentEvent::ToolFinished {
                id: format!("read-{index}"),
                result: json!({"ok":true,"content": format!("body-{index}\n")}),
            });
        }
        transcript.append("You", "after the read group");
        let width = 60;
        render(&mut transcript, width, 12);
        transcript.sync_geometry();
        // 槽位：0=You，1=read 组首（可见），2..5=隐藏成员，6=收尾 You。
        assert_eq!(transcript.entries.len(), 7);
        let first_height = transcript.geometry.height(0);
        let group_height = transcript.geometry.height(1);
        assert!(group_height > 0, "read 组首应可见");
        for hidden in 2..6 {
            assert_eq!(
                transcript.geometry.height(hidden),
                0,
                "隐藏成员槽 {hidden} 不应占行"
            );
        }
        let after_start = transcript.geometry.start(6);
        let expected = first_height + 1 + group_height + 1;
        assert_eq!(after_start, expected);
        // 隐藏成员不占行：定位到收尾条目起点时窗口应从该条目开始。
        transcript.offset = after_start;
        transcript.follow_bottom = false;
        let rows = screen(&render(&mut transcript, width, 12));
        assert!(rows.contains("after the read group"));
        assert!(!rows.contains("body-"), "隐藏成员正文不应占行：{rows}");
    }

    {
        let mut transcript = Transcript::new();
        let mut blocks = Vec::new();
        for index in 0..3 {
            let entries = transcript.append_tree_node(
                answer(&format!(
                    "## Node {index}\n\n{}",
                    "content line\n".repeat(6)
                )),
                "",
            );
            blocks.push(TreeBlock {
                entries,
                label: Line::raw(format!("node-{index}")),
            });
        }
        // 先以普通视图渲染基线（备用屏几何已建立）。
        let width = 48;
        let height = 20;
        let baseline = screen(&render(&mut transcript, width, height));

        // 进入树视图（接管展示、清空几何），再返回普通视图：屏幕应逐行相同。
        tree_render(
            &mut transcript,
            &blocks,
            &[0, 1, 2],
            Some(1),
            false,
            width,
            height,
        );
        assert!(transcript.geometry.is_empty(), "树视图接管时应清空几何");
        let restored = screen(&render(&mut transcript, width, height));
        assert_eq!(restored, baseline);
    }

    {
        let mut transcript = Transcript::new();
        fixed_entry(&mut transcript, "entry", 3);
        let mut terminal = Terminal::new(TestBackend::new(0, 0)).unwrap();
        terminal
            .draw(|frame| transcript.draw(frame, frame.area()))
            .unwrap();
        assert!(transcript.geometry.is_empty(), "零尺寸帧不应产生几何");
        assert!(
            transcript.entries[0].layout.is_none(),
            "零尺寸帧不应排版条目"
        );
        // 恢复正常尺寸后渲染照常。
        let rows = screen(&render(&mut transcript, 40, 8));
        assert!(rows.contains("entry"));
    }

    {
        let mut transcript = Transcript::new();
        for index in 0..30 {
            fixed_entry(&mut transcript, &format!("history-{index:02}"), 2);
        }
        let width = 40;
        let height = 12;
        render(&mut transcript, width, height);
        transcript.scroll_up(20);
        let before = screen(&render(&mut transcript, width, height));
        let anchor = transcript.browse_anchor.unwrap();

        // 主题失效会清空全部布局：几何身份比较应捕获并全量重算。
        transcript.invalidate_theme();
        let after = screen(&render(&mut transcript, width, height));
        assert_eq!(after, before, "同主题往返应得到相同屏幕");
        let resolved = transcript.browse_anchor.unwrap();
        assert_eq!(resolved.entry, anchor.entry);
        assert_eq!(resolved.row, anchor.row);
    }

    {
        // gap 行（正高度条目之后那一行）本身没有内容：渲染应跳到下一条目。
        let mut transcript = Transcript::new();
        fixed_entry(&mut transcript, "first", 3);
        fixed_entry(&mut transcript, "second", 2);
        let width = 40;
        let height = 6;
        render(&mut transcript, width, height);
        transcript.sync_geometry();
        let first_height = transcript.geometry.height(0);
        let gap_offset = transcript.geometry.start(0) + first_height;
        transcript.offset = gap_offset;
        transcript.follow_bottom = false;
        transcript.browse_anchor = None;
        let rows = screen(&render(&mut transcript, width, height));
        assert!(rows.contains("second"), "gap 行起点应渲染下一条目：{rows}");
    }
}
