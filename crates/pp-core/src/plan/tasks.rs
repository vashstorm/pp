use std::ops::Range;

use anyhow::{Result, anyhow, ensure};

/// A single real Markdown task item: the first-paragraph body and check state after the checkbox
/// marker is removed, plus the owning phase and the checkbox marker position in the source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTask {
    pub text: String,
    pub completed: bool,
    /// Full display text of the owning H2 phase; tasks without an H2 (document start or after an H1
    /// reset) get `Tasks`. H3-H6 headings inherit the current H2.
    pub phase: String,
    /// Physical phase region index: every H1/H2 heading opens a new region and repeated headings are
    /// not merged; used only for grouping by physical region and never serialized as a task ID.
    pub phase_index: usize,
    /// Byte range of the checkbox in the source file; it starts before the checkbox `[` (and may
    /// include a little leading whitespace) and ends after `]`. Write-back must locate the unique
    /// check marker inside the range and must not assume the whole range is the marker itself.
    pub marker: Range<usize>,
}

/// Extract H3 task headings and legacy Markdown list tasks in body order, preserving the source
/// position of each check marker. Only headings and task items recognized by the Markdown parser are
/// inspected, so fenced examples and ordinary prose do not count. Loose lists take the item's first
/// paragraph, tight lists take everything from the marker to the item's first newline. The same pass
/// captures H2 phase headings (ATX and setext alike): an H1 resets the phase, H3-H6 inherit the
/// current H2 and tasks without an H2 fall into `Tasks`; every new phase region gets a new
/// `phase_index`, repeated headings are not merged and document tasks are never reordered.
pub fn plan_tasks(content: &str) -> Vec<PlanTask> {
    use pulldown_cmark::{Event, HeadingLevel, Tag, TagEnd};
    let parser =
        pulldown_cmark::Parser::new_ext(content, pulldown_cmark::Options::ENABLE_TASKLISTS)
            .into_offset_iter();
    let mut tasks = Vec::new();
    // (task, whether a paragraph has started)
    let mut open: Option<(PlanTask, bool)> = None;
    let mut phase = String::from("Tasks");
    let mut phase_index = 0usize;
    // Display text already accumulated inside the open heading, plus the checked marker of an H3
    // task.
    let mut heading: Option<String> = None;
    let mut heading_check = None;
    for (event, range) in parser {
        if let Some((task, in_paragraph)) = &mut open {
            match &event {
                Event::Text(text) | Event::Code(text) => {
                    task.text.push_str(text);
                    continue;
                }
                Event::SoftBreak | Event::HardBreak if *in_paragraph => {
                    task.text.push(' ');
                    continue;
                }
                Event::Start(Tag::Paragraph) if task.text.is_empty() => {
                    *in_paragraph = true;
                    continue;
                }
                Event::Start(
                    Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. },
                )
                | Event::End(
                    TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link,
                ) => continue,
                _ => {}
            }
            if let Some((mut task, _)) = open.take() {
                task.text = task.text.trim().to_owned();
                tasks.push(task);
            }
        }
        match event {
            Event::TaskListMarker(completed) => {
                open = Some((
                    PlanTask {
                        text: String::new(),
                        completed,
                        phase: phase.clone(),
                        phase_index,
                        marker: range,
                    },
                    false,
                ));
            }
            Event::Start(Tag::Heading { level, .. }) => {
                heading = Some(String::new());
                if level == HeadingLevel::H3 {
                    heading_check = heading_task_check(content, range);
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some(heading_text) = &mut heading {
                    let marker_bytes = heading_check
                        .as_ref()
                        .map_or(0, |(marker, _)| marker.end.saturating_sub(range.start));
                    if let Some(text) = text.get(marker_bytes..) {
                        heading_text.push_str(text);
                    }
                }
            }
            Event::End(TagEnd::Heading(level)) => {
                if let Some(heading_text) = heading.take() {
                    match level {
                        HeadingLevel::H1 => {
                            phase_index += 1;
                            phase = "Tasks".to_string();
                        }
                        HeadingLevel::H2 => {
                            phase_index += 1;
                            phase = heading_text.trim().to_owned();
                        }
                        HeadingLevel::H3 => {
                            if let Some((marker, completed)) = heading_check.take() {
                                tasks.push(PlanTask {
                                    text: heading_text.trim().to_owned(),
                                    completed,
                                    phase: phase.clone(),
                                    phase_index,
                                    marker,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if let Some((mut task, _)) = open {
        task.text = task.text.trim().to_owned();
        tasks.push(task);
    }
    tasks
}

fn heading_task_check(content: &str, range: Range<usize>) -> Option<(Range<usize>, bool)> {
    let heading = content[range.clone()].trim_start_matches([' ', '\t']);
    let body = heading.strip_prefix("###")?.strip_prefix([' ', '\t'])?;
    let body = body.trim_start_matches([' ', '\t']);
    let (check_len, completed) = checkbox_prefix(body)?;
    let title = body[check_len..].strip_prefix([' ', '\t'])?.trim_start();
    if title
        .trim_matches(|c: char| c == '#' || c.is_whitespace())
        .is_empty()
    {
        return None;
    }
    let start = range.end - body.len();
    Some((start..start + check_len, completed))
}

fn checkbox_prefix(text: &str) -> Option<(usize, bool)> {
    match text.as_bytes() {
        [b'[', b']', ..] => Some((2, false)),
        [b'[', b' ', b']', ..] => Some((3, false)),
        [b'[', b'x' | b'X', b']', ..] => Some((3, true)),
        _ => None,
    }
}

/// Write task check state back into the source body: only for tasks whose state changed, locate the
/// unique check marker inside its marker source range, verify the old state matches the parsed state
/// and then replace the marker. Returns `None` when nothing changed; otherwise every byte outside the
/// markers is preserved, including CRLF, indentation and inline Markdown.
pub fn patch_task_checks(
    content: &str,
    tasks: &[PlanTask],
    checked: &[bool],
) -> Result<Option<String>> {
    ensure!(
        tasks.len() == checked.len(),
        "Checked vector length {} does not match {} parsed tasks.",
        checked.len(),
        tasks.len()
    );
    let mut edits = Vec::new();
    for (index, (task, &checked)) in tasks.iter().zip(checked).enumerate() {
        if task.completed == checked {
            continue;
        }
        let marker = content.get(task.marker.clone()).ok_or_else(|| {
            anyhow!(
                "Task {index} marker range {:?} is out of bounds.",
                task.marker
            )
        })?;
        let mut found = marker.match_indices('[').filter_map(|(offset, _)| {
            let (len, completed) = checkbox_prefix(&marker[offset..])?;
            Some((offset, len, completed))
        });
        let (offset, len, was_checked) = found.next().ok_or_else(|| {
            anyhow!(
                "Task {index} marker range {:?} contains no checkbox.",
                task.marker
            )
        })?;
        ensure!(
            found.next().is_none(),
            "Task {index} marker range {:?} contains multiple checkboxes.",
            task.marker
        );
        ensure!(
            was_checked == task.completed,
            "Task {index} marker is stale; re-parse the plan before patching."
        );
        let start = task.marker.start + offset;
        let replacement = if checked {
            "[x]"
        } else if content[..start]
            .trim_end_matches([' ', '\t'])
            .ends_with("###")
        {
            "[]"
        } else {
            "[ ]"
        };
        edits.push((start..start + len, replacement));
    }
    if edits.is_empty() {
        return Ok(None);
    }
    edits.sort_unstable_by_key(|(range, _)| range.start);
    let mut updated = String::with_capacity(content.len());
    let mut cursor = 0;
    for (range, replacement) in edits {
        ensure!(range.start >= cursor, "Task checkbox ranges overlap.");
        updated.push_str(&content[cursor..range.start]);
        updated.push_str(replacement);
        cursor = range.end;
    }
    updated.push_str(&content[cursor..]);
    Ok(Some(updated))
}

pub fn task_progress(content: &str) -> (usize, usize) {
    let tasks = plan_tasks(content);
    (
        tasks.iter().filter(|task| task.completed).count(),
        tasks.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_progress_ignores_code_examples_and_counts_real_tasks() {
        assert_eq!(
            task_progress(
                "```md\n- [ ] example\n```\n\nplain [x]\n\n- [x] verified\n  - [ ] pending\n"
            ),
            (1, 2)
        );
        assert_eq!(task_progress("```\n- [ ] not a task\n```"), (0, 0));
    }
}

#[cfg(test)]
mod task_tests {
    use super::*;

    #[test]
    fn plan_tasks_extract_phases_and_progress() {
        {
            let body = "# Plan title\n\nIntro with [ ] text.\n\n```md\n- [ ] fenced example\n### [] fenced heading task\n```\n\n## Phase one\n- [ ] Create the file; done when bytes match.\n  - Location: src/a.rs\n  - Acceptance: read-back equals input\n- [x] **Verify** `behavior` end to end.\n  - [ ] nested child action\n\n## Phase two\n- [ ] Loose item first paragraph.\n\n  Second paragraph is not the task text.\n";
            let tasks = plan_tasks(body);
            fn summary(items: &[PlanTask]) -> Vec<(&str, bool, &str)> {
                items
                    .iter()
                    .map(|task| (task.text.as_str(), task.completed, task.phase.as_str()))
                    .collect::<Vec<_>>()
            }
            assert_eq!(
                summary(&tasks),
                vec![
                    (
                        "Create the file; done when bytes match.",
                        false,
                        "Phase one"
                    ),
                    ("Verify behavior end to end.", true, "Phase one"),
                    ("nested child action", false, "Phase one"),
                    ("Loose item first paragraph.", false, "Phase two"),
                ]
            );
            // The marker points exactly at the checkbox in the source file.
            assert_eq!(&body[tasks[0].marker.clone()], "[ ]");
            assert_eq!(&body[tasks[1].marker.clone()], "[x]");
            assert_eq!(&body[tasks[2].marker.clone()], "[ ]");
            assert_eq!(&body[tasks[3].marker.clone()], "[ ]");
            assert_eq!(task_progress(body), (1, 4));
        }
        {
            let body = "- [ ] bootstrap task\n\n## Step\n- [ ] first\n\n## Step\n- [ ] second\n\n### Detail\n- [ ] inherits Step region\n\n# Reset\n- [ ] after reset\n\nSetext phase\n------------\n- [ ] setext task\n";
            let tasks = plan_tasks(body);
            fn summary(items: &[PlanTask]) -> Vec<(&str, bool, &str, usize)> {
                items
                    .iter()
                    .map(|task| {
                        (
                            task.text.as_str(),
                            task.completed,
                            task.phase.as_str(),
                            task.phase_index,
                        )
                    })
                    .collect::<Vec<_>>()
            }
            assert_eq!(
                summary(&tasks),
                vec![
                    ("bootstrap task", false, "Tasks", 0),
                    ("first", false, "Step", 1),
                    ("second", false, "Step", 2),
                    ("inherits Step region", false, "Step", 2),
                    ("after reset", false, "Tasks", 3),
                    ("setext task", false, "Setext phase", 4),
                ]
            );
            // Two regions of the repeated heading get different phase_index values and tasks keep
            // document order.
            assert_ne!(tasks[1].phase_index, tasks[2].phase_index);
            assert_eq!(task_progress(body), (0, 6));
        }
    }

    #[test]
    fn plan_tasks_patch_validation_progress_and_headings() {
        {
            let body = "# 计划\r\n\r\n## 写入\r\n- [ ] 写入 canary\t字节。\r\n  - Acceptance: 中文 ✓ 证据\r\n- [X] 已有勾选\r\n\r\n```md\r\n- [ ] fenced example\r\n```\r\n";
            let tasks = plan_tasks(body);
            assert_eq!(tasks.len(), 2);
            assert_eq!(
                tasks.iter().map(|task| task.completed).collect::<Vec<_>>(),
                vec![false, true]
            );
            let checked = vec![true, false];
            let patched = patch_task_checks(body, &tasks, &checked)
                .unwrap()
                .expect("checked states changed");
            // Byte-for-byte equality except for the state bytes of the two checkboxes.
            let diff: Vec<usize> = body
                .bytes()
                .zip(patched.bytes())
                .enumerate()
                .filter_map(|(index, (old, new))| (old != new).then_some(index))
                .collect();
            assert_eq!(
                diff,
                vec![tasks[0].marker.start + 1, tasks[1].marker.start + 1]
            );
            assert_eq!(patched.as_bytes()[diff[0]], b'x');
            assert_eq!(patched.as_bytes()[diff[1]], b' ');
            // Matches the literal replacen expectation (CRLF, tabs, CJK and other bytes included).
            let expected = body.replacen("[ ]", "[x]", 1).replacen("[X]", "[ ]", 1);
            assert_eq!(patched, expected);
            // An unchanged state does not copy the body.
            assert!(
                patch_task_checks(body, &tasks, &[false, true])
                    .unwrap()
                    .is_none()
            );
        }
        {
            let body = "- [ ] one\n- [ ] two\n";
            let tasks = plan_tasks(body);
            // Length mismatch.
            assert!(patch_task_checks(body, &tasks, &[true]).is_err());
            // Marker range out of bounds.
            let mut out_of_bounds = tasks.clone();
            out_of_bounds[0].marker = 100..110;
            assert!(patch_task_checks(body, &out_of_bounds, &[true, false]).is_err());
            // Two checkboxes inside the range: refuse to guess the position.
            let ambiguous = PlanTask {
                text: "one".into(),
                completed: false,
                phase: "Tasks".into(),
                phase_index: 0,
                marker: 0..9,
            };
            assert!(patch_task_checks("- [ ] [ ] one", &[ambiguous], &[true]).is_err());
            // No standard checkbox inside the range.
            let absent = PlanTask {
                text: "one".into(),
                completed: false,
                phase: "Tasks".into(),
                phase_index: 0,
                marker: 0..5,
            };
            assert!(patch_task_checks("hello world", &[absent], &[true]).is_err());
            // Old state disagrees with the parsed state (stale): the body still has [ ] while the
            // parsed state claims it is checked.
            let mut stale = plan_tasks("- [ ] pending\n");
            stale[0].completed = true;
            assert!(patch_task_checks("- [ ] pending\n", &stale, &[false]).is_err());
        }
        {
            let check = |body: &str| {
                let tasks = plan_tasks(body);
                let (completed, total) = task_progress(body);
                assert_eq!(total, tasks.len(), "total mismatch for {body:?}");
                assert_eq!(
                    completed,
                    tasks.iter().filter(|task| task.completed).count(),
                    "completed mismatch for {body:?}"
                );
            };
            check(
                "# Plan\n\nIntro [ ] not a task.\n\n```md\n- [ ] fenced\n```\n\n## Phase one\n- [ ] first item\n  - [ ] nested item\n- [X] completed item\n\n## Phase two\n- [ ] loose\n\n  second paragraph\n",
            );
            check(
                "# Plan\r\n\r\n## Write\r\n- [ ] write bytes\r\n  - Acceptance: read-back\r\n- [x] verify\r\n\r\n## Verify\r\n- [ ] read back\r\n",
            );
        }
        {
            let body = concat!(
                "## Parallel implementation\r\n",
                "### [] **Implement** `API` ###\r\n",
                "- [ ] legacy task\r\n",
                "### [X] b. Add tests\r\n",
                "### Plain heading\r\n",
                "### [] ###\r\n",
                "```markdown\r\n### [] fenced example\r\n```\r\n",
                "## Integration\r\n",
                "### [] Acceptance\r\n",
            );
            let tasks = plan_tasks(body);
            let summary = |tasks: &[PlanTask]| {
                tasks
                    .iter()
                    .map(|task| (task.text.clone(), task.phase_index))
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                summary(&tasks),
                vec![
                    ("Implement API".into(), 1),
                    ("legacy task".into(), 1),
                    ("b. Add tests".into(), 1),
                    ("Acceptance".into(), 2),
                ]
            );
            assert_eq!(task_progress(body), (1, 4));
            let patched = patch_task_checks(body, &tasks, &[true, true, false, true])
                .unwrap()
                .unwrap();
            assert_eq!(
                patched,
                body.replace("[] **Implement**", "[x] **Implement**")
                    .replace("- [ ] legacy task", "- [x] legacy task")
                    .replace("[X] b.", "[] b.")
                    .replace("[] Acceptance", "[x] Acceptance")
            );
            assert_eq!(summary(&plan_tasks(&patched)), summary(&tasks));
            assert_eq!(task_progress(&patched), (3, 4));
        }
    }
}
