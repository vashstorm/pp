//! Completion reminders (a port of the awaiting-user-answer logic from the upstream
//! todo-tracker).
//!
//! This module only provides pure functions: the state budget ([`TodoReminderState`]),
//! eligibility ([`eligible`]), open-item collection ([`open_todos`]), the reminder body
//! ([`reminder_text`]) and awaiting-user detection on the final answer text
//! ([`is_awaiting_user_answer`]). Budget resets, latch release, developer history appends
//! and event emission in run_loop are the agent main flow's responsibility.

use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;

use pp_core::plan::PlanStatus;
use pp_core::plan::WorkMode;
use pp_core::plan::WorkflowState;
use pp_core::todo::TodoItem;
use pp_core::todo::TodoPhase;
use pp_core::todo::TodoStatus;

/// Maximum completion reminders sent per run/continue (3 by default upstream).
pub(super) const TODO_REMINDERS_MAX: u8 = 3;

/// Reminder budget local to a single run_loop: emitted count and latch.
/// A sent reminder sets `awaiting_progress`; any completed tool result (including errors and
/// view) releases it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct TodoReminderState {
    pub(super) count: u8,
    pub(super) awaiting_progress: bool,
}

/// Decide whether a reminder is a candidate before the final answer: only a plain list in
/// Execute mode or a projection still bound to the current Executed plan, with a
/// pending/in_progress item, no awaiting user answer, the latch closed and budget left.
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

/// Collect open (pending/in_progress) tasks in phase order for the reminder event.
/// blocked is an external wait, completed/abandoned are closed, so none of them is a reminder
/// target.
pub(super) fn open_todos(phases: &[TodoPhase]) -> Vec<TodoItem> {
    phases
        .iter()
        .flat_map(|phase| phase.tasks.iter())
        .filter(|task| matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress))
        .cloned()
        .collect()
}

/// Full reminder body (wrapped in `<system-reminder>`) listing open items line by line per
/// phase.
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
// Awaiting-user-answer detection (ported line by line from the upstream todo-tracker.ts)
// =============================================================================

/// JS `\s` includes the BOM; add it on top of `char::is_whitespace`.
fn is_js_whitespace(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// Exclusion set for JS `.`: newline and related line terminators.
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
/// The connective itself; the trailing lookahead is implemented equivalently by
/// [`CONNECTIVE_CUE_RE`] over the remaining text.
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

/// `/(\*\*|__)(.*?)\1/g` → `"$2"`: implemented equivalently with a string scan.
/// `.*?` never crosses lines (JS `.` excludes line terminators); matches are
/// non-overlapping and go left to right.
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
        // Lazy inner part: try the closing marker at every position first, then consume one
        // character.
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
            // No match at this start: the engine advances one character and retries.
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// `^(\*\*\*|\*\*|\*|___|__|_)([\s\S]+)\1$` → `"$2"`: in the upstream alternative order
/// the first marker that wraps the whole text wins; strikethrough is deliberately not
/// unwrapped.
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

/// lookahead equivalent: match the connective prefix first, then verify the remaining text
/// starts with a cue.
fn strip_leading_connective(text: &str) -> String {
    if let Some(m) = LEADING_CONNECTIVE_RE.find(text) {
        let rest = &text[m.end()..];
        if CONNECTIVE_CUE_RE.is_match(rest) {
            return rest.to_string();
        }
    }
    text.to_string()
}

/// How the closing paragraph hands control back: cue (asks the user to act) > request
/// (explicit question) > question (question shape only).
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

/// String-scan equivalent of `(?<=[.!?…])\s+|(?<=[。！？])`: a Western terminator swallows
/// the whitespace run after it (the `?` in `foo?: string` is followed by `:`, so no sentence
/// break), while a CJK terminator breaks with zero width and stays in the preceding segment.
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

/// The last two sentences of the closing paragraph; stops at a list item.
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
    // No dictionary can recognize a non-English follow-up question or a non-English
    // self-answer; missing a pending request costs more.
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

/// Whether the final answer text is awaiting a user answer. `text` is this turn's assistant
/// text (extracted by the caller; the whole history is not scanned).
pub(super) fn is_awaiting_user_answer(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let paragraphs: Vec<&str> = PARAGRAPH_SPLIT_RE.split(text).map(str::trim).collect();
    let last_paragraph = paragraphs[paragraphs.len() - 1];
    if closing_ask(last_paragraph) != ClosingAsk::None {
        return true;
    }
    // A single-line closing note does not retract the previous paragraph's directive; the
    // question there may have been answered by the note itself.
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
    // Options plus a recommendation do not answer on the user's behalf; stop at any other
    // body text, while a self-answered question may continue.
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
    use pp_core::plan::PlanLocation;

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
    fn todo_awaiting_user_answer_positive_cases() {
        for text in [
            // English question sentences and explicit response cues
            "Which file do you want me to edit?",
            "What's next?",
            "Done with the refactor. Let me know if you want the details.",
            "Please confirm the database credentials.",
            "So please confirm the port number.",
            // Chinese questions (require a Chinese question mark)
            "还需要我补充测试吗？",
            "你选哪个方案？",
            // Q/Ask labels
            "Q: Should we proceed?",
            "Ask 2: which database?",
            "Question: what's the status?",
            // Still recognized after emphasis markers are stripped
            "*Which approach should we take?*",
            "**Should we proceed?**",
            "***Which one do you prefer?***",
            "__Should I continue?__",
            "**Heads up**: Should we proceed?",
            // Markdown prefix stripping
            "> - Which file do you need?",
            "1. Should I proceed?",
            "- Can you deploy tonight?",
            // Choice confirmation questions
            "Stick with SQLite?",
            "Go with option B?",
            "Proceed with the migration?",
            // Option list plus a recommendation
            "What next?\n- A: SQLite\n- B: Postgres\nI'd recommend option A.",
            "What next?\n- A: SQLite\n- B: Postgres\nMy recommendation: option A.",
            // Permission requests survive a single-line note
            "If you confirm, I'll run the migration.",
            "Deployment is ready.\n\nIf you approve, I'll proceed.",
            "If you approve, I'll proceed.\n\nRepository unchanged, no commits.",
            // Positive cases for the last-two-sentences rule
            "We shipped the feature. Tell me if you want a follow-up.",
            "Tell me if you want a follow-up. We shipped the feature.",
            "Feel free to skip.\nWhich format do you prefer?",
            // Non-English question
            "Хотите продолжить? Отдельно скажите, как оформить доступ.",
        ] {
            assert!(is_awaiting_user_answer(text), "expected awaiting: {text}");
        }
    }

    #[test]
    fn todo_awaiting_user_answer_negative_and_boundaries() {
        for text in [
            // Plain statements and whitespace
            "All tasks complete.",
            "",
            "   \n  ",
            "Finally, answer validation is complete.",
            "Finished the refactor. Build passes.",
            // No Chinese question mark / untagged question
            "需要我再补充一些测试吗",
            "全部任务已完成。",
            "Q: proceed",
            // `?:` inside code
            "Added the field `enabled?: boolean` and ran the suite.",
            "The type uses `retries?: number`. Tests pass.",
            // Strikethrough means retracted
            "~~Should we proceed?~~",
            "Stick with SQLite.",
            // No recommendation, a single option, or an option already answered
            "What next?\n- A: SQLite\n- B: Postgres",
            "What next?\n- A: SQLite",
            "What next?\n- A: SQLite\n- B: Postgres\nWent with option A, committed.",
            // A self-answer or multi-line closing note overrides the earlier cue
            "Do you need the report?\n\nNo, the report is already saved.",
            "If you approve, I'll proceed.\n\nNotes:\n- build ok\n- tests ok",
            // Only the last two sentences are examined / the trailing sentence overrides an
            // earlier question
            "The refactor is done. Tell me if you want a follow-up. We shipped the feature.",
            "Which format do you prefer?\nFeel free to skip.",
        ] {
            assert!(
                !is_awaiting_user_answer(text),
                "unexpected awaiting: {text}"
            );
        }
    }

    #[test]
    fn todo_open_reminder_and_eligibility_gates() {
        let filter_todos = vec![
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
        assert_eq!(
            open_todos(&filter_todos)
                .iter()
                .map(|t| t.content.as_str())
                .collect::<Vec<_>>(),
            ["B", "C"]
        );

        let reminder_todos = vec![
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
        let text = reminder_text(&reminder_todos, 1);
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

        let open = || {
            vec![phase(
                "Tasks",
                vec![
                    item("A", TodoStatus::Pending),
                    item("B", TodoStatus::Blocked),
                ],
            )]
        };
        // Execute with a plain list that has open items.
        assert!(eligible(
            &workflow(WorkMode::Execute, open()),
            &TodoReminderState::default(),
            false
        ));
        // blocked is not reminded.
        assert!(!eligible(
            &workflow(
                WorkMode::Execute,
                vec![phase("Tasks", vec![item("A", TodoStatus::Blocked)])]
            ),
            &TodoReminderState::default(),
            false
        ));
        // Plan mode is never reminded.
        assert!(!eligible(
            &workflow(WorkMode::Plan, open()),
            &TodoReminderState::default(),
            false
        ));
        // Latch, awaiting user, budget exhausted.
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

        let executed_plan = pp_core::plan::PlanDocument {
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
        // Executed and still bound to the current plan: remind.
        let bound = WorkflowState {
            plan: Some(executed_plan.clone()),
            todo_plan_id: Some("plan-1".into()),
            ..workflow(WorkMode::Execute, open())
        };
        assert!(eligible(&bound, &TodoReminderState::default(), false));
        // A read-only historical plan (Completed) or a mismatched source is not reminded.
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
        // No source (a plain list after detaching) is unaffected by the old plan.
        let standalone = WorkflowState {
            plan: Some(executed_plan),
            ..workflow(WorkMode::Execute, open())
        };
        assert!(eligible(&standalone, &TodoReminderState::default(), false));
    }
}
