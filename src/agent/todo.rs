//! 完成提醒（上游 todo-tracker 的 awaiting-user-answer 移植）。
//!
//! 本模块只提供纯函数：状态预算（[`TodoReminderState`]）、资格判定
//! （[`eligible`]）、open 项收集（[`open_todos`]）、提醒正文
//! （[`reminder_text`]) 以及终答文本的等待用户识别
//! （[`is_awaiting_user_answer`])。run_loop 的预算重置、门闩解除、
//! developer 历史追加与事件发布由 agent 主流程负责。

use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;

use crate::plan::{PlanStatus, WorkMode, WorkflowState};
use crate::tools::todo::{TodoItem, TodoPhase, TodoStatus};

/// 每个 run/continue 最多发送的完成提醒次数（上游默认 3）。
pub(super) const TODO_REMINDERS_MAX: u8 = 3;

/// 单次 run_loop 局部的提醒预算：已发次数与门闩。
/// 已发提醒置 `awaiting_progress`；任何完成的工具结果（含错误/view）解除。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct TodoReminderState {
    pub(super) count: u8,
    pub(super) awaiting_progress: bool,
}

/// 终答前判定是否候选提醒：仅 Execute 模式下普通清单或仍绑定当前
/// Executed 计划的投影；存在 pending/in_progress、未等用户回答、
/// 门闩未闭且预算未用尽。
pub(super) fn eligible(
    workflow: &WorkflowState,
    state: &TodoReminderState,
    awaiting_user_answer: bool,
) -> bool {
    if workflow.mode != WorkMode::Execute
        || state.awaiting_progress
        || awaiting_user_answer
        || state.count >= TODO_REMINDERS_MAX
    {
        return false;
    }
    if let Some(id) = &workflow.todo_plan_id {
        match workflow.plan.as_ref() {
            Some(plan) if &plan.id == id && plan.status == PlanStatus::Executed => {}
            _ => return false,
        }
    }
    !open_todos(&workflow.todos).is_empty()
}

/// 按阶段顺序收集 open（pending/in_progress）任务，供提醒事件使用。
/// blocked 是外部等待、completed/abandoned 已关闭，均不算提醒对象。
pub(super) fn open_todos(phases: &[TodoPhase]) -> Vec<TodoItem> {
    phases
        .iter()
        .flat_map(|phase| phase.tasks.iter())
        .filter(|task| matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress))
        .cloned()
        .collect()
}

/// 完整提醒正文（含 `<system-reminder>` 包裹），按阶段逐行列出 open 项。
pub(super) fn reminder_text(phases: &[TodoPhase], attempt: u8) -> String {
    let mut count = 0usize;
    let mut lines = String::new();
    for phase in phases {
        for task in &phase.tasks {
            if matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress) {
                count += 1;
                let _ = writeln!(lines, "- {}: {}", phase.name, task.content);
            }
        }
    }
    let mut text = String::new();
    let _ = write!(
        text,
        "<system-reminder>\nYou stopped with {count} incomplete todo item(s):\n{lines}\n\
         Please continue working on these tasks or mark them complete if finished.\n\
         (Reminder {attempt}/{TODO_REMINDERS_MAX})\n</system-reminder>"
    );
    text
}

// =============================================================================
// Awaiting-user-answer detection（上游 todo-tracker.ts 逐条移植）
// =============================================================================

/// JS `\s` 含 BOM；`char::is_whitespace` 之外单独补上。
fn is_js_whitespace(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// JS `.` 的排除集：换行及相关行终结符。
fn is_js_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

static MARKDOWN_PROMPT_PREFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:>\s*)?(?:(?:[-*+]|\d+[.)])\s+)*").unwrap());
static PROMPT_LABEL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:q(?:uestion)?|ask)\s*\d*\s*[:.)-]\s*").unwrap());
static OPTION_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[-*+]|\d+[.)])\s+\S").unwrap());
static RECOMMENDATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:i(?:'d| would)?\s+recommend|(?:my\s+)?recommendation)\b").unwrap()
});
static CHOICE_CONFIRMATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:go|proceed|continue|stick)\s+with\b").unwrap());
static QUESTION_PROMPT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:what|which|when|where|why|how|who|whom|whose|do|does|did|can|could|would|will|should|is|are|am|may|shall)\b",
    )
    .unwrap()
});
static USER_DIRECTED_PROMPT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:you|your|we|our)\b").unwrap());
static ADDRESSEE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:you|your)\b").unwrap());
static USER_RESPONSE_CUE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:please\s+)?(?:confirm|reply|choose|pick|decide|advise|answer|approve)\b|^(?:please\s+)?(?:let\s+me\s+know|(?:tell|message|ping)\s+me)\b|^(?:if|once|when)\s+you\s+(?:confirm|approve|agree|allow|okay|ok|give)\b",
    )
    .unwrap()
});
/// 连接词本体；后随的 lookahead 由 [`CONNECTIVE_CUE_RE`] 在剩余文本上等价实现。
static LEADING_CONNECTIVE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:and|or|so|then|also|otherwise|alternatively|separately|finally)\b,?\s*")
        .unwrap()
});
static CONNECTIVE_CUE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:please\b|let\s+me\s+know\b|(?:tell|message|ping)\s+me\b|(?:if|once|when)\s+you\b)",
    )
    .unwrap()
});
static QUESTION_TAIL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[?？]\s*$").unwrap());
static PARAGRAPH_SPLIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\r?\n\s*\r?\n").unwrap());

/// `/(\*\*|__)(.*?)\1/g` → `"$2"`：反引引用用字符串扫描等价实现。
/// `.*?` 不跨行（JS `.` 排除行终结符），匹配不重叠、从左到右。
fn strip_inline_emphasis(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < n {
        let opener = match chars[i] {
            '*' if i + 1 < n && chars[i + 1] == '*' => Some('*'),
            '_' if i + 1 < n && chars[i + 1] == '_' => Some('_'),
            _ => None,
        };
        let Some(marker) = opener else {
            out.push(chars[i]);
            i += 1;
            continue;
        };
        // 懒惰内层：每个位置先试闭合标记，再消费一个字符。
        let mut j = i + 2;
        let mut matched = false;
        while j + 1 < n {
            if chars[j] == marker && chars[j + 1] == marker {
                for &c in &chars[i + 2..j] {
                    out.push(c);
                }
                i = j + 2;
                matched = true;
                break;
            }
            if is_js_line_terminator(chars[j]) {
                break;
            }
            j += 1;
        }
        if !matched {
            // 该起点无匹配：引擎前进一个字符后重试。
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// `^(\*\*\*|\*\*|\*|___|__|_)([\s\S]+)\1$` → `"$2"`：按上游备选顺序，
/// 首个可完整包裹的标记生效；删除线刻意不解包。
fn unwrap_wrapped_emphasis(text: &str) -> String {
    const MARKERS: [&str; 6] = ["***", "**", "*", "___", "__", "_"];
    for marker in MARKERS {
        let m = marker.len();
        if text.len() > 2 * m && text.starts_with(marker) && text.ends_with(marker) {
            return text[m..text.len() - m].to_string();
        }
    }
    text.to_string()
}

struct PromptLine {
    text: String,
    had_prompt_label: bool,
}

fn prompt_line(line: &str) -> PromptLine {
    let without_markdown_prefix = MARKDOWN_PROMPT_PREFIX_RE
        .replace(&strip_inline_emphasis(line.trim()), "")
        .trim()
        .to_string();
    let without_prompt_label = PROMPT_LABEL_RE
        .replace(&without_markdown_prefix, "")
        .trim()
        .to_string();
    let without_emphasis = unwrap_wrapped_emphasis(&without_prompt_label)
        .trim()
        .to_string();
    PromptLine {
        had_prompt_label: without_prompt_label != without_markdown_prefix,
        text: without_emphasis,
    }
}

fn is_question_prompt_line(line: &str) -> bool {
    let candidate = prompt_line(line);
    if !QUESTION_TAIL_RE.is_match(&candidate.text) {
        return false;
    }
    candidate.had_prompt_label
        || QUESTION_PROMPT_RE.is_match(&candidate.text)
        || CHOICE_CONFIRMATION_RE.is_match(&candidate.text)
        || USER_DIRECTED_PROMPT_RE.is_match(&candidate.text)
        || !candidate.text.is_ascii()
}

fn is_response_cue_line(line: &str) -> bool {
    let candidate = prompt_line(line)
        .text
        .trim_end_matches(['.', '!', '?', '。', '！', '？'])
        .to_string();
    let candidate = strip_leading_connective(&candidate);
    USER_RESPONSE_CUE_RE.is_match(candidate.trim())
}

/// lookahead 等价：先匹配连接词前缀，再验证剩余文本以 cue 起始。
fn strip_leading_connective(text: &str) -> String {
    if let Some(m) = LEADING_CONNECTIVE_RE.find(text) {
        let rest = &text[m.end()..];
        if CONNECTIVE_CUE_RE.is_match(rest) {
            return rest.to_string();
        }
    }
    text.to_string()
}

/// 结尾段落如何交还控制权：cue（指示用户行动）> request（明确提问）>
/// question（仅问句形状）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClosingAsk {
    None,
    Question,
    Request,
    Cue,
}

fn ask_of(span: &str) -> ClosingAsk {
    if is_response_cue_line(span) {
        return ClosingAsk::Cue;
    }
    if !is_question_prompt_line(span) {
        return ClosingAsk::None;
    }
    let candidate = prompt_line(span);
    if candidate.had_prompt_label
        || ADDRESSEE_RE.is_match(&candidate.text)
        || !candidate.text.is_ascii()
    {
        ClosingAsk::Request
    } else {
        ClosingAsk::Question
    }
}

/// `(?<=[.!?…])\s+|(?<=[。！？])` 的字符串扫描等价：西文终结符吞掉
/// 其后的空白串（`foo?: string` 的 `?` 后是 `:`，不会断句），CJK
/// 终结符之后零宽断开、终结符留在前段。
fn split_sentences(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut sentences: Vec<String> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        if i > 0 {
            let prev = chars[i - 1];
            if matches!(prev, '.' | '!' | '?' | '…') && is_js_whitespace(chars[i]) {
                sentences.push(chars[start..i].iter().collect());
                while i < chars.len() && is_js_whitespace(chars[i]) {
                    i += 1;
                }
                start = i;
                continue;
            }
            if matches!(prev, '。' | '！' | '？') {
                sentences.push(chars[start..i].iter().collect());
                start = i;
            }
        }
        i += 1;
    }
    sentences.push(chars[start..].iter().collect());
    sentences.retain(|sentence| !sentence.is_empty());
    sentences
}

/// 结尾段落的最后两句；遇到列表项即停。
fn closing_sentences(paragraph: &str) -> Vec<String> {
    let mut sentences: Vec<String> = Vec::new();
    for line in paragraph.split('\n').rev() {
        if sentences.len() >= 2 {
            break;
        }
        let line = line.trim();
        if OPTION_LINE_RE.is_match(line) {
            break;
        }
        let mut line_sentences = split_sentences(line);
        let skip = line_sentences
            .len()
            .saturating_sub(2usize.saturating_sub(sentences.len()));
        let mut combined = line_sentences.split_off(skip);
        combined.append(&mut sentences);
        sentences = combined;
    }
    sentences
}

fn closing_ask(paragraph: &str) -> ClosingAsk {
    let last_line = paragraph.rsplit('\n').next().unwrap_or("").trim();
    let sentences = closing_sentences(paragraph);
    let mut asks = vec![ask_of(last_line)];
    if let Some(last) = sentences.last() {
        asks.push(ask_of(last));
    }
    // 无词典能识别非英文追问或非英文自答；漏掉未决请求代价更高。
    let foreign_question =
        sentences.len() == 2 && !sentences[0].is_ascii() && is_question_prompt_line(&sentences[0]);
    if asks.contains(&ClosingAsk::Cue) {
        return ClosingAsk::Cue;
    }
    if foreign_question || asks.contains(&ClosingAsk::Request) {
        return ClosingAsk::Request;
    }
    if asks.contains(&ClosingAsk::Question) {
        ClosingAsk::Question
    } else {
        ClosingAsk::None
    }
}

/// 终答文本是否在等待用户回答。`text` 是本次终答的 assistant 文本
/// （调用方提取，不扫描全部历史）。
pub(super) fn is_awaiting_user_answer(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let paragraphs: Vec<&str> = PARAGRAPH_SPLIT_RE.split(text).map(str::trim).collect();
    let last_paragraph = paragraphs[paragraphs.len() - 1];
    if closing_ask(last_paragraph) != ClosingAsk::None {
        return true;
    }
    // 单行收尾备注不撤回前一段的指示；那里的问句可能是被备注自答的。
    if paragraphs.len() >= 2 {
        let previous = paragraphs[paragraphs.len() - 2];
        if !previous.is_empty()
            && !last_paragraph.contains('\n')
            && !OPTION_LINE_RE.is_match(last_paragraph)
            && closing_ask(previous) == ClosingAsk::Cue
        {
            return true;
        }
    }
    // 选项加推荐不代表替用户回答；遇到其他正文即停，自答问句允许继续。
    let mut option_count = 0usize;
    let mut has_recommendation = false;
    for line in text.split('\n').rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if OPTION_LINE_RE.is_match(line) {
            option_count += 1;
            continue;
        }
        let candidate = prompt_line(line);
        if option_count > 0 {
            return option_count >= 2
                && is_question_prompt_line(line)
                && (has_recommendation
                    || candidate.had_prompt_label
                    || USER_DIRECTED_PROMPT_RE.is_match(&candidate.text)
                    || CHOICE_CONFIRMATION_RE.is_match(&candidate.text));
        }
        if RECOMMENDATION_RE.is_match(&candidate.text) {
            has_recommendation = true;
            continue;
        }
        return false;
    }
    false
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlanLocation;

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.to_string(),
            status,
            blocker: None,
        }
    }

    fn phase(name: &str, tasks: Vec<TodoItem>) -> TodoPhase {
        TodoPhase {
            name: name.to_string(),
            tasks,
        }
    }

    fn workflow(mode: WorkMode, todos: Vec<TodoPhase>) -> WorkflowState {
        WorkflowState {
            mode,
            todos,
            ..WorkflowState::default()
        }
    }

    #[test]
    fn todo_english_question_and_cue_detected() {
        assert!(is_awaiting_user_answer(
            "Which file do you want me to edit?"
        ));
        assert!(is_awaiting_user_answer("What's next?"));
        assert!(is_awaiting_user_answer(
            "Done with the refactor. Let me know if you want the details."
        ));
        assert!(is_awaiting_user_answer(
            "Please confirm the database credentials."
        ));
        assert!(is_awaiting_user_answer(
            "So please confirm the port number."
        ));
    }

    #[test]
    fn todo_plain_statements_not_awaiting() {
        assert!(!is_awaiting_user_answer("All tasks complete."));
        assert!(!is_awaiting_user_answer(""));
        assert!(!is_awaiting_user_answer("   \n  "));
        assert!(!is_awaiting_user_answer(
            "Finally, answer validation is complete."
        ));
        assert!(!is_awaiting_user_answer(
            "Finished the refactor. Build passes."
        ));
    }

    #[test]
    fn todo_chinese_question_detected_only_with_question_mark() {
        assert!(is_awaiting_user_answer("还需要我补充测试吗？"));
        assert!(is_awaiting_user_answer("你选哪个方案？"));
        assert!(!is_awaiting_user_answer("需要我再补充一些测试吗"));
        assert!(!is_awaiting_user_answer("全部任务已完成。"));
    }

    #[test]
    fn todo_code_question_marks_excluded() {
        // `?` 后跟 `:` 而非空白，既不断句也不构成问句。
        assert!(!is_awaiting_user_answer(
            "Added the field `enabled?: boolean` and ran the suite."
        ));
        assert!(!is_awaiting_user_answer(
            "The type uses `retries?: number`. Tests pass."
        ));
    }

    #[test]
    fn todo_prompt_labels_promote_to_request() {
        assert!(is_awaiting_user_answer("Q: Should we proceed?"));
        assert!(is_awaiting_user_answer("Ask 2: which database?"));
        assert!(is_awaiting_user_answer("Question: what's the status?"));
        assert!(!is_awaiting_user_answer("Q: proceed"));
    }

    #[test]
    fn todo_emphasis_stripped_before_detection() {
        assert!(is_awaiting_user_answer("*Which approach should we take?*"));
        assert!(is_awaiting_user_answer("**Should we proceed?**"));
        assert!(is_awaiting_user_answer("***Which one do you prefer?***"));
        assert!(is_awaiting_user_answer("__Should I continue?__"));
        assert!(is_awaiting_user_answer("**Heads up**: Should we proceed?"));
        // 删除线代表作者已撤回该问句，不解包。
        assert!(!is_awaiting_user_answer("~~Should we proceed?~~"));
    }

    #[test]
    fn todo_markdown_prefixes_stripped() {
        assert!(is_awaiting_user_answer("> - Which file do you need?"));
        assert!(is_awaiting_user_answer("1. Should I proceed?"));
        assert!(is_awaiting_user_answer("- Can you deploy tonight?"));
    }

    #[test]
    fn todo_choice_confirmation_questions() {
        assert!(is_awaiting_user_answer("Stick with SQLite?"));
        assert!(is_awaiting_user_answer("Go with option B?"));
        assert!(is_awaiting_user_answer("Proceed with the migration?"));
        assert!(!is_awaiting_user_answer("Stick with SQLite."));
    }

    #[test]
    fn todo_options_with_recommendation_await() {
        assert!(is_awaiting_user_answer(
            "What next?\n- A: SQLite\n- B: Postgres\nI'd recommend option A."
        ));
        assert!(is_awaiting_user_answer(
            "What next?\n- A: SQLite\n- B: Postgres\nMy recommendation: option A."
        ));
        // 无推荐、问句又不指向用户时不替用户作答。
        assert!(!is_awaiting_user_answer(
            "What next?\n- A: SQLite\n- B: Postgres"
        ));
        // 单个选项不构成未决选择。
        assert!(!is_awaiting_user_answer("What next?\n- A: SQLite"));
        // 选项后遇到普通正文即停（自答）。
        assert!(!is_awaiting_user_answer(
            "What next?\n- A: SQLite\n- B: Postgres\nWent with option A, committed."
        ));
    }

    #[test]
    fn todo_permission_cue_survives_note() {
        assert!(is_awaiting_user_answer(
            "If you confirm, I'll run the migration."
        ));
        assert!(is_awaiting_user_answer(
            "Deployment is ready.\n\nIf you approve, I'll proceed."
        ));
        // 单行收尾备注不撤回前段的许可请求。
        assert!(is_awaiting_user_answer(
            "If you approve, I'll proceed.\n\nRepository unchanged, no commits."
        ));
    }

    #[test]
    fn todo_self_answered_question_not_awaiting() {
        assert!(!is_awaiting_user_answer(
            "Do you need the report?\n\nNo, the report is already saved."
        ));
        // 多行收尾使前段的 cue 规则不适用，选项走查遇到普通正文即停。
        assert!(!is_awaiting_user_answer(
            "If you approve, I'll proceed.\n\nNotes:\n- build ok\n- tests ok"
        ));
    }

    #[test]
    fn todo_only_last_two_sentences_count() {
        assert!(is_awaiting_user_answer(
            "We shipped the feature. Tell me if you want a follow-up."
        ));
        // 段首 cue 锚定整段：中段的 cue 指示被末句陈述覆盖。
        assert!(is_awaiting_user_answer(
            "Tell me if you want a follow-up. We shipped the feature."
        ));
        assert!(!is_awaiting_user_answer(
            "The refactor is done. Tell me if you want a follow-up. We shipped the feature."
        ));
        // 前一句提问、后一句收尾陈述：以最后一句为准。
        assert!(!is_awaiting_user_answer(
            "Which format do you prefer?\nFeel free to skip."
        ));
        assert!(is_awaiting_user_answer(
            "Feel free to skip.\nWhich format do you prefer?"
        ));
    }

    #[test]
    fn todo_foreign_question_promotes_to_request() {
        assert!(is_awaiting_user_answer(
            "Хотите продолжить? Отдельно скажите, как оформить доступ."
        ));
    }

    #[test]
    fn todo_open_todos_filters_status() {
        let todos = vec![
            phase(
                "Write",
                vec![
                    item("A", TodoStatus::Completed),
                    item("B", TodoStatus::InProgress),
                ],
            ),
            phase(
                "Verify",
                vec![
                    item("C", TodoStatus::Pending),
                    item("D", TodoStatus::Blocked),
                    item("E", TodoStatus::Abandoned),
                ],
            ),
        ];
        let open = open_todos(&todos);
        assert_eq!(
            open.iter().map(|t| t.content.as_str()).collect::<Vec<_>>(),
            ["B", "C"]
        );
    }

    #[test]
    fn todo_reminder_text_lists_open_by_phase() {
        let todos = vec![
            phase(
                "Write",
                vec![
                    item("Write canary bytes.", TodoStatus::Completed),
                    item("Rewrite docs.", TodoStatus::Blocked),
                ],
            ),
            phase(
                "Verify",
                vec![item("Read back canary bytes.", TodoStatus::Pending)],
            ),
        ];
        let text = reminder_text(&todos, 1);
        assert!(text.starts_with("<system-reminder>\n"));
        assert!(text.contains("You stopped with 1 incomplete todo item(s):"));
        assert!(text.contains("- Verify: Read back canary bytes."));
        assert!(!text.contains("Write canary bytes."));
        assert!(!text.contains("Rewrite docs."));
        assert!(
            text.contains(
                "Please continue working on these tasks or mark them complete if finished."
            )
        );
        assert!(text.contains("(Reminder 1/3)"));
        assert!(text.ends_with("</system-reminder>"));
    }

    #[test]
    fn todo_eligibility_gates() {
        let open = || {
            vec![phase(
                "Tasks",
                vec![
                    item("A", TodoStatus::Pending),
                    item("B", TodoStatus::Blocked),
                ],
            )]
        };
        // Execute 普通清单且有 open 项。
        assert!(eligible(
            &workflow(WorkMode::Execute, open()),
            &TodoReminderState::default(),
            false
        ));
        // blocked 不催。
        assert!(!eligible(
            &workflow(
                WorkMode::Execute,
                vec![phase("Tasks", vec![item("A", TodoStatus::Blocked)])]
            ),
            &TodoReminderState::default(),
            false
        ));
        // Plan 模式一律不催。
        assert!(!eligible(
            &workflow(WorkMode::Plan, open()),
            &TodoReminderState::default(),
            false
        ));
        // 门闩、等待用户、预算用尽。
        assert!(!eligible(
            &workflow(WorkMode::Execute, open()),
            &TodoReminderState {
                awaiting_progress: true,
                ..TodoReminderState::default()
            },
            false
        ));
        assert!(!eligible(
            &workflow(WorkMode::Execute, open()),
            &TodoReminderState::default(),
            true
        ));
        assert!(!eligible(
            &workflow(WorkMode::Execute, open()),
            &TodoReminderState {
                count: TODO_REMINDERS_MAX,
                awaiting_progress: false
            },
            false
        ));

        let executed_plan = crate::plan::PlanDocument {
            id: "plan-1".into(),
            revision: 1,
            title: String::new(),
            content: String::new(),
            status: PlanStatus::Executed,
            origin_prompt: String::new(),
            file_name: None,
            location: PlanLocation::Plans,
            approved_tasks: None,
        };
        // Executed 且仍绑定当前计划：可催。
        let bound = WorkflowState {
            plan: Some(executed_plan.clone()),
            todo_plan_id: Some("plan-1".into()),
            ..workflow(WorkMode::Execute, open())
        };
        assert!(eligible(&bound, &TodoReminderState::default(), false));
        // 只读历史计划（Completed）与来源不匹配均不催。
        let mut completed = executed_plan.clone();
        completed.status = PlanStatus::Completed;
        let readonly = WorkflowState {
            plan: Some(completed),
            ..bound.clone()
        };
        assert!(!eligible(&readonly, &TodoReminderState::default(), false));
        let mut detached = bound.clone();
        detached.todo_plan_id = Some("other".into());
        assert!(!eligible(&detached, &TodoReminderState::default(), false));
        // 无来源（脱离后普通清单）不受旧计划影响。
        let standalone = WorkflowState {
            plan: Some(executed_plan),
            ..workflow(WorkMode::Execute, open())
        };
        assert!(eligible(&standalone, &TodoReminderState::default(), false));
    }
}
