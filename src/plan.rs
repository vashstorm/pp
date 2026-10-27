use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};

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

/// 一个真实 Markdown 任务项：去掉 checkbox 标记后的首段正文及勾选状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanTask {
    pub text: String,
    pub completed: bool,
}

/// 按正文出现顺序提取全部真实任务项，与 `task_progress` 共用 pulldown-cmark
/// 的 task list 解析，因此围栏示例不计数；说明、普通子项目符号不成为任务。
/// 松散列表取项内首个段落，紧凑列表取 marker 后到该项首个换行为止。
pub(crate) fn plan_tasks(content: &str) -> Vec<PlanTask> {
    use pulldown_cmark::{Event, Tag, TagEnd};
    let parser =
        pulldown_cmark::Parser::new_ext(content, pulldown_cmark::Options::ENABLE_TASKLISTS);
    let mut tasks = Vec::new();
    // (任务, 是否已进入段落)
    let mut open: Option<(PlanTask, bool)> = None;
    for event in parser {
        if let Some((task, in_paragraph)) = &mut open {
            let keep = match &event {
                Event::Text(text) | Event::Code(text) => {
                    task.text.push_str(text);
                    true
                }
                Event::SoftBreak | Event::HardBreak if *in_paragraph => {
                    task.text.push(' ');
                    true
                }
                Event::Start(Tag::Paragraph) if task.text.is_empty() => {
                    *in_paragraph = true;
                    true
                }
                Event::Start(
                    Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. },
                )
                | Event::End(
                    TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link,
                ) => true,
                _ => false,
            };
            if keep {
                continue;
            }
            if let Some((mut task, _)) = open.take() {
                task.text = task.text.trim().to_owned();
                tasks.push(task);
            }
        }
        if let Event::TaskListMarker(completed) = event {
            open = Some((
                PlanTask {
                    text: String::new(),
                    completed,
                },
                false,
            ));
        }
    }
    if let Some((mut task, _)) = open {
        task.text = task.text.trim().to_owned();
        tasks.push(task);
    }
    tasks
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
            review: None,
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
            review: None,
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
        assert_eq!(
            tasks,
            vec![
                PlanTask {
                    text: "Create the file; done when bytes match.".into(),
                    completed: false
                },
                PlanTask {
                    text: "Verify behavior end to end.".into(),
                    completed: true
                },
                PlanTask {
                    text: "nested child action".into(),
                    completed: false
                },
                PlanTask {
                    text: "Loose item first paragraph.".into(),
                    completed: false
                },
            ]
        );
        assert_eq!(task_progress(body), (1, 4));
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
