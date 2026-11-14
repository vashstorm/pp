use std::ops::Range;

use anyhow::{Result, anyhow, ensure};

/// 一个真实 Markdown 任务项：去掉 checkbox 标记后的首段正文及勾选状态，
/// 以及所属阶段与源文件中的 checkbox 标记位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTask {
    pub text: String,
    pub completed: bool,
    /// 所属 H2 阶段的完整显示文本；未归属 H2（文档开头或 H1 重置后）为
    /// `Tasks`。H3-H6 标题继承当前 H2。
    pub phase: String,
    /// 物理阶段区域序号：每个 H1/H2 标题都开启新区域，重复标题不合并；
    /// 仅用于按物理区域分组，不序列化为任务 ID。
    pub phase_index: usize,
    /// 源文件中 checkbox 的字节区间；起点在 checkbox 的 `[` 之前
    /// （可能含少量前导空白），终点在 `]` 之后。回写时必须在区间内定位
    /// 唯一勾选标记，不能假定整个区间就是标记本身。
    pub marker: Range<usize>,
}

/// 按正文顺序提取 H3 任务标题及旧式 Markdown 列表任务，保留勾选标记的源位置。
/// 只检查 Markdown 解析器识别的标题和任务项，因此围栏示例及普通说明不计数。
/// 松散列表取项内首个段落，紧凑列表取 marker 后到该项首个换行为止。
/// 同一次遍历捕获 H2 阶段标题（ATX 与 setext 均可）：H1 重置阶段，
/// H3-H6 继承当前 H2，未归属 H2 的任务归入 `Tasks`；每个新的阶段区域有新的
/// `phase_index`，重复标题不合并、不重排文档任务。
pub fn plan_tasks(content: &str) -> Vec<PlanTask> {
    use pulldown_cmark::{Event, HeadingLevel, Tag, TagEnd};
    let parser =
        pulldown_cmark::Parser::new_ext(content, pulldown_cmark::Options::ENABLE_TASKLISTS)
            .into_offset_iter();
    let mut tasks = Vec::new();
    // (任务, 是否已进入段落)
    let mut open: Option<(PlanTask, bool)> = None;
    let mut phase = String::from("Tasks");
    let mut phase_index = 0usize;
    // 打开的标题内已聚合的显示文本，以及 H3 任务的勾选标记。
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

/// 将任务勾选状态回写到源正文：仅对状态变化的任务，在其 marker 源区间内
/// 定位唯一勾选标记，验证旧状态与解析状态一致后替换标记。全部无变化返回
/// `None`；有变化时保留 CRLF、缩进、内联 Markdown 及标记以外的所有字节。
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
    fn plan_tasks_extract_marker_text_in_order_and_ignore_prose_and_fences() {
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
        // marker 恰好指向源文件中的 checkbox 本体。
        assert_eq!(&body[tasks[0].marker.clone()], "[ ]");
        assert_eq!(&body[tasks[1].marker.clone()], "[x]");
        assert_eq!(&body[tasks[2].marker.clone()], "[ ]");
        assert_eq!(&body[tasks[3].marker.clone()], "[ ]");
        assert_eq!(task_progress(body), (1, 4));
    }

    #[test]
    fn plan_todo_tasks_capture_phase_regions_headings_and_duplicates() {
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
        // 重复标题的两个区域拥有不同的 phase_index，任务保持文档顺序。
        assert_ne!(tasks[1].phase_index, tasks[2].phase_index);
        assert_eq!(task_progress(body), (0, 6));
    }

    #[test]
    fn plan_todo_patch_updates_state_bytes_and_preserves_everything_else() {
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
        // 除两个 checkbox 的中间状态字节外逐字节相等。
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
        // 与逐字 replacen 的期望一致（含 CRLF、制表符、中文与其他字节）。
        let expected = body.replacen("[ ]", "[x]", 1).replacen("[X]", "[ ]", 1);
        assert_eq!(patched, expected);
        // 状态无变化时不复制正文。
        assert!(
            patch_task_checks(body, &tasks, &[false, true])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn plan_todo_patch_rejects_mismatch_out_of_bounds_ambiguous_and_stale() {
        let body = "- [ ] one\n- [ ] two\n";
        let tasks = plan_tasks(body);
        // 长度不一致。
        assert!(patch_task_checks(body, &tasks, &[true]).is_err());
        // marker 区间越界。
        let mut out_of_bounds = tasks.clone();
        out_of_bounds[0].marker = 100..110;
        assert!(patch_task_checks(body, &out_of_bounds, &[true, false]).is_err());
        // 区间内出现两个 checkbox：拒绝猜测位置。
        let ambiguous = PlanTask {
            text: "one".into(),
            completed: false,
            phase: "Tasks".into(),
            phase_index: 0,
            marker: 0..9,
        };
        assert!(patch_task_checks("- [ ] [ ] one", &[ambiguous], &[true]).is_err());
        // 区间内没有标准 checkbox。
        let absent = PlanTask {
            text: "one".into(),
            completed: false,
            phase: "Tasks".into(),
            phase_index: 0,
            marker: 0..5,
        };
        assert!(patch_task_checks("hello world", &[absent], &[true]).is_err());
        // 旧状态与解析状态不符（stale）：正文仍是 [ ]，解析状态却被标为已勾选。
        let mut stale = plan_tasks("- [ ] pending\n");
        stale[0].completed = true;
        assert!(patch_task_checks("- [ ] pending\n", &stale, &[false]).is_err());
    }

    #[test]
    fn plan_todo_task_progress_matches_parsed_tasks_on_real_documents() {
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
            "# 计划\n\nIntro [ ] not a task.\n\n```md\n- [ ] fenced\n```\n\n## 阶段一\n- [ ] 第一项\n  - [ ] 嵌套项\n- [X] 已完成项\n\n## 阶段二\n- [ ] loose\n\n  second paragraph\n",
        );
        check(
            "# Plan\r\n\r\n## Write\r\n- [ ] write bytes\r\n  - Acceptance: read-back\r\n- [x] verify\r\n\r\n## Verify\r\n- [ ] read back\r\n",
        );
    }

    #[test]
    fn plan_heading_tasks_round_trip_with_legacy_tasks() {
        let body = concat!(
            "## 并行实现\r\n",
            "### [] **实现** `API` ###\r\n",
            "- [ ] 旧格式任务\r\n",
            "### [X] b. 补充测试\r\n",
            "### 普通标题\r\n",
            "### [] ###\r\n",
            "```markdown\r\n### [] 围栏内示例\r\n```\r\n",
            "## 集成\r\n",
            "### [] 验收\r\n",
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
                ("实现 API".into(), 1),
                ("旧格式任务".into(), 1),
                ("b. 补充测试".into(), 1),
                ("验收".into(), 2),
            ]
        );
        assert_eq!(task_progress(body), (1, 4));
        let patched = patch_task_checks(body, &tasks, &[true, true, false, true])
            .unwrap()
            .unwrap();
        assert_eq!(
            patched,
            body.replace("[] **实现**", "[x] **实现**")
                .replace("- [ ] 旧格式任务", "- [x] 旧格式任务")
                .replace("[X] b.", "[] b.")
                .replace("[] 验收", "[x] 验收")
        );
        assert_eq!(summary(&plan_tasks(&patched)), summary(&tasks));
        assert_eq!(task_progress(&patched), (3, 4));
    }
}
