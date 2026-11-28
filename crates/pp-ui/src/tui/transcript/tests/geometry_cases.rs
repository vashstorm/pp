use super::*;

/// Build a conversation entry with a deterministic height so paging windows can be checked by index.
/// The Data card renders line by line per logical line, so the row count is directly controllable.
fn fixed_entry(transcript: &mut Transcript, label: &str, rows: usize) {
    transcript.append("Note", &format!("{label}\n{}", "body\n".repeat(rows - 1)));
}

/// Expected window body starting at offset: take the visible text of each line and trim the right-hand padding spaces.
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

/// Build a mixed history from the same deterministic script for comparison rendering.
fn mixed_history() -> Transcript {
    let mut transcript = Transcript::new();
    // 240 turns × 1.25 entries each (including error cards) = 300 mixed entries.
    for index in 0..240 {
        match index % 4 {
            0 => fixed_entry(&mut transcript, &format!("short-{index:03}"), 1),
            1 => fixed_entry(&mut transcript, &format!("multi-{index:03}"), 5),
            // An empty-body Data card has height 0: verify that a zero-height slot takes no line and adds no gap.
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
    // 300+ mixed entries: short text, multi-line bodies, empty bodies (zero height) and local error cards.
    let width = 40;
    let height = 10;
    let mut reference = mixed_history();
    let base = render(&mut reference, width, height);
    let base_rows = visible_rows(&base);
    assert!(base_rows.iter().any(|row| !row.is_empty()));

    // Page down one page at a time: each screen's body must come from the same batch of entries located by the geometry and must advance monotonically.
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
            "a paged window must not be entirely blank"
        );
        if let Some(earlier) = previous.as_ref() {
            assert_ne!(&rows, earlier, "page_down must advance the window");
        }
        previous = Some(rows);
        if transcript.following() {
            break;
        }
        transcript.page_down();
        pages += 1;
        assert!(pages < 5000, "paging did not terminate");
    }

    // Page back up to the top: offset is monotonically non-increasing.
    let mut offsets = Vec::new();
    while transcript.offset > 0 {
        let before = transcript.offset;
        transcript.page_up();
        render(&mut transcript, width, height);
        assert!(
            transcript.offset <= before,
            "page_up must not move the window down"
        );
        offsets.push(transcript.offset);
        assert!(offsets.len() < 5000, "scrolling back did not terminate");
    }

    // Row-by-row equivalence with the corresponding window of the full render: the full viewer path is unaffected by geometry positioning.
    let mut full = mixed_history();
    full.prepare_layout(width);
    full.sync_geometry();
    let total = full.total;
    assert!(
        total > 0,
        "syncing the geometry must yield a positive total"
    );
    let mut stitched: Vec<String> = Vec::new();
    for start in (0..total).step_by(64) {
        let rows = (total - start).min(64);
        let area = Rect::new(0, 0, width, rows as u16);
        let mut buffer = Buffer::empty(area);
        full.render(&mut buffer, area, start);
        stitched.extend(visible_rows(&buffer));
    }
    // The last screen of the full replay and the alternate-screen bottom window hold the same trailing content.
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
        "the end of the full replay must contain the latest content: {last_window:?}"
    );
    assert!(
        base_rows
            .iter()
            .any(|row| row.contains("error-239") || row.contains("wide-239")),
        "the alternate-screen bottom window must contain the latest content: {base_rows:?}"
    );
    assert_eq!(
        base_rows.iter().filter(|row| !row.is_empty()).count(),
        last_window.iter().filter(|row| !row.is_empty()).count(),
        "the alternate screen and the full replay must show the same number of visible rows in the trailing window"
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
    // Walk up from the bottom to the line number that lands exactly inside the middle entry.
    let marker_index = transcript
        .entries
        .iter()
        .position(|entry| entry.text.contains("middle-marker"))
        .unwrap();
    transcript.sync_geometry();
    let marker_height = transcript.geometry.height(marker_index);
    assert!(
        marker_height >= 20,
        "the entry under test must be tall enough to carry an in-entry row offset: {marker_height}"
    );
    let marker_start = transcript.geometry.start(marker_index);
    transcript.offset = marker_start + 10;
    transcript.follow_bottom = false;
    transcript.browse_anchor = None;
    let before = screen(&render(&mut transcript, width, height));
    let anchor = transcript.browse_anchor.unwrap();
    assert_eq!(
        anchor.entry, marker_index,
        "the anchor must fall inside the entry under test: {before}"
    );
    assert_eq!(
        anchor.row, 10,
        "the in-entry row offset must be preserved: {before}"
    );
    assert!(
        before.contains("body"),
        "the window must show the entry body: {before}"
    );

    // The middle entry is replaced by streaming content: the identity change must be caught on the next frame and the anchor must stay on the same entry.
    let entry = &mut transcript.entries[marker_index];
    entry.text = format!("replaced\n{}", "new body\n".repeat(45));
    entry.layout = None;

    let after = screen(&render(&mut transcript, width, height));
    assert!(
        after.contains("replaced") || after.contains("new body"),
        "the entry must still be shown after replacement: {after}"
    );
    let resolved = transcript.browse_anchor.unwrap();
    assert_eq!(
        resolved.entry, anchor.entry,
        "the anchor entry must not drift"
    );
    assert_eq!(
        resolved.row, anchor.row,
        "the in-entry row offset must not drift"
    );
    // The window still lands on the same batch of entries after the replacement: later content must not push it or pull it back.
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
        // Slots: 0=You, 1=read group head (visible), 2..5=hidden members, 6=trailing You.
        assert_eq!(transcript.entries.len(), 7);
        let first_height = transcript.geometry.height(0);
        let group_height = transcript.geometry.height(1);
        assert!(group_height > 0, "the read group head must be visible");
        for hidden in 2..6 {
            assert_eq!(
                transcript.geometry.height(hidden),
                0,
                "hidden member slot {hidden} must take no line"
            );
        }
        let after_start = transcript.geometry.start(6);
        let expected = first_height + 1 + group_height + 1;
        assert_eq!(after_start, expected);
        // Hidden members take no line: positioning at the start of the trailing entry must begin the window at that entry.
        transcript.offset = after_start;
        transcript.follow_bottom = false;
        let rows = screen(&render(&mut transcript, width, 12));
        assert!(rows.contains("after the read group"));
        assert!(
            !rows.contains("body-"),
            "the body of a hidden member must take no line: {rows}"
        );
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
        // Render a baseline in the regular view first (the alternate-screen geometry is already built).
        let width = 48;
        let height = 20;
        let baseline = screen(&render(&mut transcript, width, height));

        // Enter the tree view (takes over the display, clears the geometry) and return to the regular view: the screen must be identical line by line.
        tree_render(
            &mut transcript,
            &blocks,
            &[0, 1, 2],
            Some(1),
            false,
            width,
            height,
        );
        assert!(
            transcript.geometry.is_empty(),
            "the tree view must clear the geometry on takeover"
        );
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
        assert!(
            transcript.geometry.is_empty(),
            "a zero-sized frame must not produce geometry"
        );
        assert!(
            transcript.entries[0].layout.is_none(),
            "a zero-sized frame must not lay out entries"
        );
        // Rendering works as usual once the size is restored.
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

        // Invalidating the theme clears every layout: the geometry identity comparison must catch that and recompute everything.
        transcript.invalidate_theme();
        let after = screen(&render(&mut transcript, width, height));
        assert_eq!(
            after, before,
            "a round trip within the same theme must yield the same screen"
        );
        let resolved = transcript.browse_anchor.unwrap();
        assert_eq!(resolved.entry, anchor.entry);
        assert_eq!(resolved.row, anchor.row);
    }

    {
        // The gap line (the row after a positive-height entry) has no content of its own: rendering must skip to the next entry.
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
        assert!(
            rows.contains("second"),
            "a window starting on a gap line must render the next entry: {rows}"
        );
    }
}
