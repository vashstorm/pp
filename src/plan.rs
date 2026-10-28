use std::ops::Range;

use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};

use crate::tools::todo::TodoPhase;

pub(crate) const PLAN_MAX_BYTES: usize = 262_144;

/// 计划文件的生命周期目录；新文档显式落在 Drafts。
/// serde 默认 Plans 仅供缺少 `location` 字段的旧记录反序列化。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanLocation {
    Drafts,
    #[default]
    Plans,
    Done,
}

impl PlanLocation {
    /// `.pp/` 下的目录名。
    pub(crate) fn dir_name(self) -> &'static str {
        match self {
            PlanLocation::Drafts => "drafts",
            PlanLocation::Plans => "plans",
            PlanLocation::Done => "done",
        }
    }
}

/// Plan 模式规则（含阶段式 Markdown 默认模板），见 `src/prompts/plan.md`。
pub(crate) const PLAN_INSTRUCTIONS: &str = include_str!("prompts/plan.md");

/// 执行期进度规则，由两种批准交接与 Execute 的 workflow 注入。
pub(crate) const PROGRESS_INSTRUCTIONS: &str = include_str!("prompts/plan-progress.md");

pub(crate) fn execution_prompt(plan: &PlanDocument, context: ExecutionContext) -> String {
    let mut prompt = String::from(
        "Implement the explicitly approved plan below. Normal system, current project and skill rules still apply. Inspect the relevant code's current state before changing it. Use the approved plan as the implementation basis; exploration history is evidence, not permission to revive abandoned alternatives. If the plan conflicts with an important user requirement in retained history, clarify the conflict before making changes.\n\n",
    );
    prompt.push_str(PROGRESS_INSTRUCTIONS);
    if context == ExecutionContext::Keep {
        prompt.push_str("\n\nOriginal user task (retained context):\n");
        prompt.push_str(&plan.origin_prompt);
    }
    use std::fmt::Write;
    // 审批流程保证只有已保存（revision>=1）的计划能进入执行交接。
    let path = plan
        .path()
        .expect("approved plan has a saved lifecycle path");
    write!(prompt, "\n\nApproved plan: {}\nPath: {}\nRevision: {}\n\n[Approved Markdown snapshot]\n{}\n[/Approved Markdown snapshot]", plan.id, path, plan.revision, plan.content).expect("writing to String");
    prompt
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkMode {
    #[default]
    Execute,
    Plan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanStatus {
    Draft,
    Ready,
    Executed,
    Completed,
    Abandoned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanDocument {
    pub id: String,
    pub revision: u64,
    pub title: String,
    pub content: String,
    pub status: PlanStatus,
    pub origin_prompt: String,
    /// AI 选定的稳定文件名（含 `.md`）；首存时绑定，旧记录缺失时保持 None。
    #[serde(default)]
    pub file_name: Option<String>,
    /// 生命周期位置；默认 Plans 仅供旧记录反序列化，新文档显式 Drafts。
    #[serde(default)]
    pub location: PlanLocation,
    /// 批准时由经 ReviewToken 验证的正文冻结的有序任务文本；执行进度导入
    /// 不得覆盖。旧记录与未批准草稿为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_tasks: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowState {
    pub mode: WorkMode,
    pub plan: Option<PlanDocument>,
    #[serde(default)]
    pub review: Option<PlanReviewReport>,
    /// 计划派生的阶段/任务投影缓存，同时是 active/blocked 运行态载体；
    /// Markdown 始终是任务与勾选的主记录，这里不是竞争的第二主记录。
    /// 空时省略，旧会话记录靠 serde default 兼容，checkpoint 格式不升版。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todos: Vec<TodoPhase>,
    /// None 表示普通清单；Some(id) 表示当前清单来自该计划的派生投影，
    /// 必须与当前绑定的 plan.id 一致。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_plan_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanReviewReport {
    pub plan_id: String,
    pub revision: u64,
    pub model_reference: String,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutionContext {
    Keep,
    Fresh,
    Compact,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewToken {
    pub session_id: String,
    pub plan_id: String,
    /// 审阅时安全读取到的实际短路径；重命名/移位/删除后旧 token 失效。
    pub path: String,
    pub revision: u64,
    pub content: String,
}

pub(crate) struct PlanReview {
    pub token: ReviewToken,
    pub document: PlanDocument,
    pub report: Option<PlanReviewReport>,
}

impl PlanDocument {
    pub(crate) fn new() -> Result<Self> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)
            .map_err(|_| anyhow::anyhow!("Could not allocate a plan ID."))?;
        let id = format!("{:032x}", u128::from_be_bytes(bytes));
        Ok(Self {
            id,
            revision: 0,
            title: String::new(),
            content: String::new(),
            status: PlanStatus::Draft,
            origin_prompt: String::new(),
            file_name: None,
            location: PlanLocation::Drafts,
            approved_tasks: None,
        })
    }

    /// 当前绑定的实际短路径。新 revision-0 未保存时返回 None；
    /// 已命名文档返回生命周期目录路径；旧 revision>=1 缺失文件名时
    /// 临时返回旧 `<id>.md` 路径，登记显式文件名后不再使用该兜底。
    pub(crate) fn path(&self) -> Option<String> {
        match (&self.file_name, self.revision) {
            (Some(name), _) => Some(path_for(name, self.location)),
            (None, 0) => None,
            (None, _) => Some(format!(".pp/plans/{}.md", self.id)),
        }
    }

    /// The next registered revision, rejecting counter overflow in one place.
    pub(crate) fn next_revision(&self) -> Result<u64> {
        self.revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Plan revision limit reached."))
    }
}

/// 生命周期目录内的计划文件路径；不再由 ID 推导。
pub(crate) fn path_for(file_name: &str, location: PlanLocation) -> String {
    format!(".pp/{}/{}", location.dir_name(), file_name)
}

pub(crate) fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 新 AI 命名与用户重命名的严格规则：英文 kebab-case 的 Markdown 文件名。
/// 仅 ASCII 小写字母/数字以单个连字符分段，总长 4..=100 字节。
pub(crate) fn validate_file_name(file_name: &str) -> Result<()> {
    let stem = file_name
        .strip_suffix(".md")
        .ok_or_else(|| anyhow!("Plan file name must end with .md."))?;
    ensure!(
        (4..=100).contains(&file_name.len())
            && !stem.is_empty()
            && stem
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && !stem.starts_with('-')
            && !stem.ends_with('-')
            && !stem.contains("--"),
        "Plan file name must be kebab-case Markdown, like fix-config-loading.md."
    );
    Ok(())
}

/// 旧文件名/目录扫描的宽松规则：合法的单个 normal path component，
/// 以 `.md` 结尾，长度 4..=255 字节；保留旧 Unicode 文件名。
pub(crate) fn validate_stored_file_name(file_name: &str) -> Result<()> {
    ensure!(
        (4..=255).contains(&file_name.len())
            && file_name != "."
            && file_name != ".."
            && file_name.ends_with(".md")
            && !file_name
                .chars()
                .any(|character| character == '/' || character == '\\' || character.is_control()),
        "Invalid plan file name."
    );
    Ok(())
}

pub(crate) fn validate_content(content: &str) -> Result<()> {
    ensure!(
        !content.trim().is_empty(),
        "Plan content must not be empty."
    );
    ensure!(
        content.len() <= PLAN_MAX_BYTES,
        "Plan content is too large."
    );
    ensure!(
        !content.contains('\0'),
        "Plan content must not contain NUL."
    );
    Ok(())
}

pub(crate) fn validate_title(title: &str) -> Result<()> {
    ensure!(
        !title.trim().is_empty()
            && title == title.trim()
            && title.chars().count() <= 120
            && !title.chars().any(char::is_control),
        "Plan title must contain 1 to 120 characters without control characters."
    );
    Ok(())
}

impl WorkflowState {
    pub(crate) fn mode_name(&self) -> &'static str {
        match self.mode {
            WorkMode::Execute => "execute",
            WorkMode::Plan => "plan",
        }
    }

    pub(crate) fn instructions(&self) -> String {
        let mut instructions = match self.mode {
            WorkMode::Plan => PLAN_INSTRUCTIONS.to_owned(),
            // 进度规则只在 Execute+Executed 注入；Completed/Abandoned 是
            // 只读历史，不再注入执行规则。
            WorkMode::Execute
                if self
                    .plan
                    .as_ref()
                    .is_some_and(|plan| plan.status == PlanStatus::Executed) =>
            {
                format!(
                    "EXECUTE MODE — the local user has explicitly handed this plan off for ordinary execution. Plan mode tool restrictions no longer apply.\n{PROGRESS_INSTRUCTIONS}"
                )
            }
            WorkMode::Execute if self.plan.is_some() => String::from(
                "EXECUTE MODE — the bound plan is no longer being executed. The Markdown document is read-only history; do not modify it and do not infer remaining work from it.",
            ),
            WorkMode::Execute => return String::new(),
        };
        if let Some(plan) = &self.plan {
            use std::fmt::Write;
            // 未保存的新绑定不打印假路径。
            let path = plan.path().unwrap_or_else(|| "Not saved yet".to_owned());
            let status_note = match plan.status {
                PlanStatus::Executed => {
                    "Executed means handed off, not all tasks completed. Read the current file before revising it; never restore missing files silently from a historical snapshot."
                }
                PlanStatus::Completed => {
                    "Completed means every approved task finished successfully and the runtime archived the file under .pp/done/. The document is read-only history."
                }
                PlanStatus::Abandoned => "Abandoned plans are read-only history.",
                PlanStatus::Draft | PlanStatus::Ready => {
                    "Read the current file before revising it; never restore missing files silently from a historical snapshot."
                }
            };
            write!(instructions, "\n\nCurrent plan id: {}\nProject-relative path: {}\nRegistered revision: {}\nStatus: {:?}. {status_note}", plan.id, path, plan.revision, plan.status).expect("writing to String");
        } else if self.mode == WorkMode::Plan {
            instructions.push_str(
                "\nNo plan document is currently bound. The next user task starts a new plan.",
            );
        }
        instructions
    }

    pub(crate) fn validate(&self) -> Result<()> {
        let invalid = "Invalid saved workflow state.";
        if let Some(plan) = &self.plan {
            ensure!(valid_id(&plan.id), "{invalid}");
            if plan.revision == 0 {
                ensure!(
                    plan.status == PlanStatus::Draft
                        && plan.title.is_empty()
                        && plan.content.is_empty()
                        && plan.file_name.is_none(),
                    "{invalid}"
                );
            } else {
                validate_title(&plan.title).map_err(|_| anyhow::anyhow!(invalid))?;
                validate_content(&plan.content).map_err(|_| anyhow::anyhow!(invalid))?;
                ensure!(!plan.origin_prompt.trim().is_empty(), "{invalid}");
                // 旧记录允许缺失 file_name（由登记协调补齐）；已命名的按
                // stored 宽松规则校验，保留旧 Unicode 文件名。
                if let Some(name) = &plan.file_name {
                    validate_stored_file_name(name).map_err(|_| anyhow::anyhow!(invalid))?;
                }
            }
            // 生命周期位置约束：旧 Draft/Ready/Abandoned 在 Plans 仍可
            // 反序列化，之后按迁移规则协调；Completed 只在 Done。
            ensure!(
                match plan.status {
                    PlanStatus::Draft | PlanStatus::Ready =>
                        matches!(plan.location, PlanLocation::Drafts | PlanLocation::Plans),
                    PlanStatus::Executed => plan.location == PlanLocation::Plans,
                    PlanStatus::Completed => plan.location == PlanLocation::Done,
                    PlanStatus::Abandoned => true,
                },
                "{invalid}"
            );
            ensure!(
                self.mode == WorkMode::Plan
                    || matches!(
                        plan.status,
                        PlanStatus::Executed | PlanStatus::Completed | PlanStatus::Abandoned
                    ),
                "{invalid}"
            );
        }
        // 计划来源的 todo 必须仍绑定当前计划；普通清单不需要来源。
        if let Some(id) = &self.todo_plan_id {
            let plan = self.plan.as_ref().ok_or_else(|| anyhow::anyhow!(invalid))?;
            ensure!(*id == plan.id, "{invalid}");
        }
        if let Some(report) = &self.review {
            let plan = self.plan.as_ref().ok_or_else(|| anyhow::anyhow!(invalid))?;
            ensure!(
                report.plan_id == plan.id
                    && report.revision >= 1
                    && report.revision <= plan.revision,
                "{invalid}"
            );
            let (provider, model) = report
                .model_reference
                .split_once('/')
                .ok_or_else(|| anyhow::anyhow!(invalid))?;
            ensure!(
                !provider.is_empty()
                    && !model.is_empty()
                    && !report.model_reference.chars().any(char::is_control),
                "{invalid}"
            );
            validate_content(&report.content).map_err(|_| anyhow::anyhow!(invalid))?;
        }
        Ok(())
    }
}

/// 一个真实 Markdown 任务项：去掉 checkbox 标记后的首段正文及勾选状态，
/// 以及所属阶段与源文件中的 checkbox 标记位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanTask {
    pub text: String,
    pub completed: bool,
    /// 所属 H2 阶段的完整显示文本；未归属 H2（文档开头或 H1 重置后）为
    /// `Tasks`。H3-H6 标题继承当前 H2。
    pub phase: String,
    /// 物理阶段区域序号：每个 H1/H2 标题都开启新区域，重复标题不合并；
    /// 仅用于按物理区域分组，不序列化为任务 ID。
    pub phase_index: usize,
    /// 源文件中 TaskListMarker 的字节区间；起点在 checkbox 的 `[` 之前
    /// （可能含少量前导空白），终点在 `]` 之后。回写时必须在区间内定位
    /// 唯一标准 checkbox，不能假定整个区间就是 checkbox。
    pub marker: Range<usize>,
}

/// 按正文出现顺序提取全部真实任务项，与 `task_progress` 共用 pulldown-cmark
/// 的 task list 解析（含源位置），因此围栏示例不计数；说明、普通子项目符号
/// 不成为任务。松散列表取项内首个段落，紧凑列表取 marker 后到该项首个换行
/// 为止。同一次遍历捕获 H2 阶段标题（ATX 与 setext 均可）：H1 重置阶段，
/// H3-H6 继承当前 H2，未归属 H2 的任务归入 `Tasks`；每个新的阶段区域有新的
/// `phase_index`，重复标题不合并、不重排文档任务。
pub(crate) fn plan_tasks(content: &str) -> Vec<PlanTask> {
    use pulldown_cmark::{Event, HeadingLevel, Tag, TagEnd};
    let parser =
        pulldown_cmark::Parser::new_ext(content, pulldown_cmark::Options::ENABLE_TASKLISTS)
            .into_offset_iter();
    let mut tasks = Vec::new();
    // (任务, 是否已进入段落)
    let mut open: Option<(PlanTask, bool)> = None;
    let mut phase = String::from("Tasks");
    let mut phase_index = 0usize;
    // 打开的标题内已聚合的显示文本。
    let mut heading: Option<String> = None;
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
            Event::Start(Tag::Heading { .. }) => {
                heading = Some(String::new());
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some(heading_text) = &mut heading {
                    heading_text.push_str(&text);
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

/// 将任务勾选状态回写到源正文：仅对状态变化的任务，在其 marker 源区间内
/// 定位唯一标准 checkbox（`[ ]`/`[x]`/`[X]`），验证旧状态与解析状态一致后
/// 修改中间一个 ASCII 字节。任务与勾选向量长度不一致、区间越界、区间内没有
/// 或出现多个标准 checkbox、或旧状态与解析状态不符（stale）时返回错误，不做
/// 任何位置猜测。全部无变化返回 `None`，不复制正文；有变化只复制一次，保留
/// CRLF、缩进、列表符号、内联 Markdown、代码围栏及所有其他字节。
pub(crate) fn patch_task_checks(
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
    let mut edits: Vec<(usize, u8)> = Vec::new();
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
        let bytes = marker.as_bytes();
        let mut found: Option<usize> = None;
        for offset in 0..bytes.len().saturating_sub(2) {
            if bytes[offset] == b'['
                && bytes[offset + 2] == b']'
                && matches!(bytes[offset + 1], b' ' | b'x' | b'X')
            {
                ensure!(
                    found.is_none(),
                    "Task {index} marker range {:?} contains multiple checkboxes.",
                    task.marker
                );
                found = Some(offset);
            }
        }
        let offset = found.ok_or_else(|| {
            anyhow!(
                "Task {index} marker range {:?} contains no standard checkbox.",
                task.marker
            )
        })?;
        ensure!(
            (bytes[offset + 1] != b' ') == task.completed,
            "Task {index} marker is stale; re-parse the plan before patching."
        );
        edits.push((
            task.marker.start + offset + 1,
            if checked { b'x' } else { b' ' },
        ));
    }
    if edits.is_empty() {
        return Ok(None);
    }
    let mut bytes = content.as_bytes().to_vec();
    for (position, byte) in edits {
        bytes[position] = byte;
    }
    Ok(Some(
        String::from_utf8(bytes).expect("only ASCII checkbox state bytes were replaced"),
    ))
}

pub(crate) fn task_progress(content: &str) -> (usize, usize) {
    let parser =
        pulldown_cmark::Parser::new_ext(content, pulldown_cmark::Options::ENABLE_TASKLISTS);
    let mut completed = 0;
    let mut total = 0;
    for event in parser {
        if let pulldown_cmark::Event::TaskListMarker(done) = event {
            total += 1;
            completed += usize::from(done);
        }
    }
    (completed, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::todo::{TodoItem, TodoStatus};

    #[test]
    fn todo_workflow_fields_stay_backward_compatible_in_serde() {
        // 旧会话记录没有 todo 字段：serde default 补齐为空，checkpoint 不升版。
        let legacy: WorkflowState =
            serde_json::from_str(r#"{"mode":"execute","plan":null}"#).unwrap();
        assert_eq!(legacy, WorkflowState::default());
        assert!(legacy.todos.is_empty());
        assert_eq!(legacy.todo_plan_id, None);

        // 空投影不写入保存的 workflow 快照。
        let text = serde_json::to_string(&WorkflowState::default()).unwrap();
        assert!(!text.contains("todos"));
        assert!(!text.contains("todo_plan_id"));

        // 非空投影与来源完整往返。
        let planned = WorkflowState {
            todos: vec![TodoPhase {
                name: "Write".into(),
                tasks: vec![TodoItem {
                    content: "A".into(),
                    status: TodoStatus::InProgress,
                    blocker: None,
                }],
            }],
            todo_plan_id: Some("0123456789abcdef0123456789abcdef".into()),
            ..Default::default()
        };
        let text = serde_json::to_string(&planned).unwrap();
        assert!(text.contains("todos") && text.contains("todo_plan_id"));
        assert_eq!(
            serde_json::from_str::<WorkflowState>(&text).unwrap(),
            planned
        );
    }

    #[test]
    fn todo_plan_source_validation_requires_current_plan_binding() {
        let mut plan = PlanDocument::new().unwrap();
        plan.revision = 1;
        plan.title = "Title".into();
        plan.content = "- [ ] task".into();
        plan.origin_prompt = "task".into();
        plan.status = PlanStatus::Executed;
        plan.location = PlanLocation::Plans;
        let base = WorkflowState {
            mode: WorkMode::Execute,
            plan: Some(plan.clone()),
            ..Default::default()
        };
        base.validate().unwrap();

        // 与当前 plan.id 一致的来源合法；投影条目可有可无。
        let mut bound = base.clone();
        bound.todo_plan_id = Some(plan.id.clone());
        bound.validate().unwrap();
        bound.todos = vec![TodoPhase {
            name: "Write".into(),
            tasks: vec![TodoItem {
                content: "task".into(),
                status: TodoStatus::InProgress,
                blocker: None,
            }],
        }];
        bound.validate().unwrap();

        // 来源与当前计划不一致拒绝。
        let mut wrong = bound.clone();
        wrong.todo_plan_id = Some("ffffffffffffffffffffffffffffffff".into());
        assert!(wrong.validate().is_err());

        // 有来源但未绑定计划拒绝。
        let mut detached = bound;
        detached.plan = None;
        assert!(detached.validate().is_err());

        // 无来源的普通清单合法。
        let mut ordinary = base;
        ordinary.todos = vec![TodoPhase {
            name: "Tasks".into(),
            tasks: vec![TodoItem {
                content: "standalone".into(),
                status: TodoStatus::Pending,
                blocker: None,
            }],
        }];
        ordinary.validate().unwrap();
    }

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

    #[test]
    fn plan_loading_rejects_privilege_and_revision_corruption() {
        let mut plan = PlanDocument::new().unwrap();
        plan.revision = 1;
        plan.title = "Create file".into();
        plan.content = "- [ ] Create file and verify bytes".into();
        plan.origin_prompt = "Create a file".into();
        plan.status = PlanStatus::Ready;
        plan.location = PlanLocation::Drafts;
        let mut workflow = WorkflowState {
            mode: WorkMode::Plan,
            plan: Some(plan),
            ..Default::default()
        };
        workflow.validate().unwrap();
        workflow.mode = WorkMode::Execute;
        assert!(workflow.validate().is_err());
        workflow.mode = WorkMode::Plan;
        workflow.plan.as_mut().unwrap().revision = 0;
        assert!(workflow.validate().is_err());
        workflow.plan.as_mut().unwrap().revision = 1;
        workflow.plan.as_mut().unwrap().id = "../escape".into();
        assert!(workflow.validate().is_err());
    }

    #[test]
    fn file_name_rules_keep_legacy_names_strict_for_new_names() {
        // 新 AI 命名：严格 kebab-case。
        assert!(validate_file_name("fix-config-loading.md").is_ok());
        assert!(validate_file_name("a.md").is_ok());
        assert!(validate_file_name(".md").is_err());
        assert!(validate_file_name("123.md").is_ok());
        assert!(validate_file_name("-lead.md").is_err());
        assert!(validate_file_name("trail-.md").is_err());
        assert!(validate_file_name("double--dash.md").is_err());
        assert!(validate_file_name("Upper.md").is_err());
        assert!(validate_file_name("no-extension").is_err());
        assert!(validate_file_name("nested/name.md").is_err());
        assert!(validate_file_name(&format!("{}.md", "a".repeat(97))).is_ok());
        assert!(validate_file_name(&format!("{}.md", "a".repeat(98))).is_err());

        // 旧存储命名：单个普通 component 的 `.md` 文件，保留 Unicode。
        assert!(validate_stored_file_name("计划-修订.md").is_ok());
        assert!(validate_stored_file_name(&format!("{}.md", "0".repeat(32))).is_ok());
        assert!(validate_stored_file_name(".md").is_err());
        assert!(validate_stored_file_name("nested/name.md").is_err());
        assert!(validate_stored_file_name("back\\slash.md").is_err());
        assert!(validate_stored_file_name("nul\0name.md").is_err());
        assert!(validate_stored_file_name("dot.md\0").is_err());
        assert!(validate_stored_file_name("../escape.md").is_err());
        assert!(validate_stored_file_name("..").is_err());
        assert!(validate_stored_file_name(&format!("{}.md", "长".repeat(84))).is_ok());
        assert!(validate_stored_file_name(&format!("{}.md", "长".repeat(85))).is_err());
    }

    #[test]
    fn document_path_resolves_lifecycle_and_legacy_locations() {
        // 未保存的新绑定没有路径。
        let fresh = PlanDocument::new().unwrap();
        assert_eq!(fresh.path(), None);
        assert_eq!(fresh.location, PlanLocation::Drafts);

        // 已命名文档按生命周期目录解析。
        let mut named = PlanDocument::new().unwrap();
        named.revision = 1;
        named.file_name = Some("fix-config-loading.md".into());
        assert_eq!(
            named.path().as_deref(),
            Some(".pp/drafts/fix-config-loading.md")
        );
        named.location = PlanLocation::Done;
        assert_eq!(
            named.path().as_deref(),
            Some(".pp/done/fix-config-loading.md")
        );
        assert_eq!(
            path_for("done-plan.md", PlanLocation::Done),
            ".pp/done/done-plan.md"
        );

        // 旧 revision>=1 缺失文件名：临时旧路径兜底。
        let mut legacy = PlanDocument::new().unwrap();
        legacy.revision = 1;
        legacy.location = PlanLocation::Plans;
        assert_eq!(
            legacy.path().as_deref(),
            Some(format!(".pp/plans/{}.md", legacy.id)).as_deref()
        );
    }

    #[test]
    fn workflow_validation_enforces_location_and_file_names() {
        let mut plan = PlanDocument::new().unwrap();
        plan.revision = 1;
        plan.title = "Title".into();
        plan.content = "- [ ] task".into();
        plan.origin_prompt = "task".into();
        plan.file_name = Some("fix-config-loading.md".into());
        let workflow = |plan: PlanDocument| WorkflowState {
            mode: WorkMode::Plan,
            plan: Some(plan),
            ..Default::default()
        };
        // Draft/Ready 在 Drafts 或 Plans（旧记录）。
        for location in [PlanLocation::Drafts, PlanLocation::Plans] {
            let mut document = plan.clone();
            document.location = location;
            workflow(document).validate().unwrap();
        }
        // 已命名时 Completed 只在 Done；Executed 只在 Plans。
        let mut completed = plan.clone();
        completed.status = PlanStatus::Completed;
        completed.location = PlanLocation::Done;
        workflow(completed.clone()).validate().unwrap();
        let mut wrong = completed.clone();
        wrong.location = PlanLocation::Plans;
        assert!(workflow(wrong).validate().is_err());
        let mut executed = plan.clone();
        executed.status = PlanStatus::Executed;
        executed.location = PlanLocation::Plans;
        workflow(executed.clone()).validate().unwrap();
        let mut wrong = executed.clone();
        wrong.location = PlanLocation::Drafts;
        assert!(workflow(wrong).validate().is_err());
        // 旧 Unicode 名称通过 stored 规则。
        let mut unicode = plan.clone();
        unicode.file_name = Some("旧计划.md".into());
        workflow(unicode).validate().unwrap();
        // 非法名称被拒绝。
        let mut escape = plan.clone();
        escape.file_name = Some("../escape.md".into());
        assert!(workflow(escape).validate().is_err());
        // 旧记录缺 file_name 仍合法（legacy 兜底路径）。
        let mut unnamed = plan;
        unnamed.file_name = None;
        unnamed.location = PlanLocation::Plans;
        workflow(unnamed).validate().unwrap();
    }
}

#[cfg(test)]
mod task_tests {
    use super::*;

    #[test]
    fn plan_tasks_extract_marker_text_in_order_and_ignore_prose_and_fences() {
        let body = "# Plan title\n\nIntro with [ ] text.\n\n```md\n- [ ] fenced example\n```\n\n## Phase one\n- [ ] Create the file; done when bytes match.\n  - Location: src/a.rs\n  - Acceptance: read-back equals input\n- [x] **Verify** `behavior` end to end.\n  - [ ] nested child action\n\n## Phase two\n- [ ] Loose item first paragraph.\n\n  Second paragraph is not the task text.\n";
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
    fn plan_phase_template_counts_exactly_two_tasks_and_ignores_headings() {
        let body = "# Title\n\n## Goal\nText.\n\n## Phase one\n- [ ] Action one.\n  - Location: x\n  - Change: y\n  - Acceptance: z\n\n## Phase two\n- [ ] Action two.\n\n## Verification\n```md\n- [ ] example only\n```\n";
        assert_eq!(task_progress(body), (0, 2));
        let tasks = plan_tasks(body);
        assert_eq!(tasks.len(), 2);
        let ticked = body.replacen("- [ ] Action one.", "- [x] Action one.", 1);
        assert_eq!(task_progress(&ticked), (1, 2));
        // 仅勾选状态变化不改变任务文本向量。
        let texts =
            |items: Vec<PlanTask>| items.into_iter().map(|task| task.text).collect::<Vec<_>>();
        assert_eq!(texts(plan_tasks(&ticked)), texts(tasks));
    }
}
