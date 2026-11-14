use crate::cancellation::CancellationToken;
use anyhow::{Result, bail};
use async_openai::types::chat::FunctionObject;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;

mod arguments;

pub(crate) const RESERVED_CUSTOM_LABEL: &str = "Other (type your own)";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskArgs {
    pub questions: Vec<AskQuestion>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskQuestion {
    pub id: String,
    pub question: String,
    pub options: Vec<AskOption>,
    pub header: Option<String>,
    #[serde(default)]
    pub multi: bool,
    pub recommended: Option<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskOption {
    pub label: String,
    pub description: Option<String>,
    pub preview: Option<String>,
}
pub struct AskRequest {
    pub call_id: String,
    pub args: Arc<AskArgs>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskAnswer {
    pub id: String,
    pub selected_options: Vec<String>,
    pub custom_input: Option<String>,
}
pub type AskCallback =
    dyn Fn(AskRequest, &CancellationToken) -> anyhow::Result<Vec<AskAnswer>> + Send + Sync;
pub type AskHandler = Box<AskCallback>;
pub type SharedAskHandler = Arc<AskCallback>;

pub fn definition() -> FunctionObject {
    FunctionObject {
        name: "ask".into(),
        description: Some(
            "Ask the user one or more questions and wait for their answers. Call this only when a decision genuinely requires the user's input; batch related questions into a single call instead of asking repeatedly. Each question needs a unique id, the question text, and options; empty options allows only a free-form answer. Set multi to true to let the user pick several options. Prefer recommended as a zero-based option index on the question object, alongside id, question and options. Alternatively, set recommended:true on one option. Do not provide conflicting recommendations. recommended is advisory only; it never auto-selects and never grants authorization. This tool does not approve file writes or any other tool execution, and it does not block other tools in the same batch. Answers are returned in question order using the original option labels exactly."
                .into(),
        ),
        parameters: Some(json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "question": {"type": "string"},
                            "header": {"type": "string"},
                            "multi": {"type": "boolean", "default": false},
                            "recommended": {
                                "type": "integer", "minimum": 0,
                                "description": "Optional zero-based index into this question's options. Omit when there is no recommendation."
                            },
                            "options": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string"},
                                        "description": {"type": "string"},
                                        "preview": {"type": "string"},
                                        "recommended": {
                                            "type": "boolean",
                                            "description": "Optional advisory hint: true recommends this option. Mark at most one option; it must agree with any question-level recommended index."
                                        }
                                    },
                                    "required": ["label"],
                                    "additionalProperties": false
                                }
                            }
                        },
                        "required": ["id", "question", "options"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        })),
        ..Default::default()
    }
}

impl AskArgs {
    /// 归一化 provider 参数后统一校验；不回显输入或解析器原文。
    pub fn parse(arguments: &str) -> Result<Self> {
        let args = arguments::parse(arguments)?;
        args.validate()?;
        Ok(args)
    }

    fn validate(&self) -> Result<()> {
        if self.questions.is_empty() {
            bail!("ask requires at least one question");
        }
        let mut seen_ids: HashSet<&str> = HashSet::new();
        for question in &self.questions {
            if question.id.trim().is_empty() {
                bail!("ask question id must not be blank");
            }
            if !seen_ids.insert(question.id.as_str()) {
                bail!("duplicate ask question id");
            }
            if question.question.trim().is_empty() {
                bail!("ask question text must not be blank");
            }
            let mut seen_labels: HashSet<&str> = HashSet::new();
            for option in &question.options {
                if option.label.trim().is_empty() {
                    bail!("ask option label must not be blank");
                }
                if option.label == RESERVED_CUSTOM_LABEL {
                    bail!("ask option label is reserved");
                }
                if !seen_labels.insert(option.label.as_str()) {
                    bail!("duplicate ask option label");
                }
            }
            if let Some(index) = question.recommended
                && index >= question.options.len()
            {
                bail!("ask recommended index is out of range");
            }
        }
        Ok(())
    }

    /// Validate handler answers against this batch. Content-strict and
    /// order-agnostic: unknown or duplicate question ids, unknown or duplicate
    /// option labels, blank custom input, and single-choice violations fail;
    /// ordering does not. Call `canonicalize_answers` afterwards to impose the
    /// prescribed question/option order before serializing.
    pub fn validate_answers(&self, answers: &[AskAnswer]) -> Result<()> {
        if answers.len() != self.questions.len() {
            bail!("invalid ask response");
        }
        let mut seen_ids: HashSet<&str> = HashSet::new();
        for answer in answers {
            let question = self
                .questions
                .iter()
                .find(|question| question.id == answer.id)
                .ok_or_else(|| anyhow::anyhow!("invalid ask response"))?;
            if !seen_ids.insert(answer.id.as_str()) {
                bail!("invalid ask response");
            }
            let mut selected: HashSet<&str> = HashSet::new();
            for label in &answer.selected_options {
                if !question.options.iter().any(|option| &option.label == label) {
                    bail!("invalid ask response");
                }
                if !selected.insert(label.as_str()) {
                    bail!("invalid ask response");
                }
            }
            let has_custom = answer
                .custom_input
                .as_deref()
                .is_some_and(|custom| !custom.trim().is_empty());
            if answer.custom_input.is_some() && !has_custom {
                bail!("invalid ask response");
            }
            if !question.multi {
                let selected = answer.selected_options.len();
                if selected > 1 || (selected == 1 && has_custom) || (selected == 0 && !has_custom) {
                    bail!("invalid ask response");
                }
            }
        }
        Ok(())
    }

    /// Reorder validated answers into the prescribed output order: answers in
    /// question order, multi-select labels in original option order. Never
    /// rewrites labels or drops answers. Call after `validate_answers`.
    pub fn canonicalize_answers(&self, mut answers: Vec<AskAnswer>) -> Vec<AskAnswer> {
        answers.sort_by_key(|answer| {
            self.questions
                .iter()
                .position(|question| question.id == answer.id)
                .unwrap_or(usize::MAX)
        });
        for answer in &mut answers {
            if let Some(question) = self
                .questions
                .iter()
                .find(|question| question.id == answer.id)
            {
                answer.selected_options.sort_by_key(|label| {
                    question
                        .options
                        .iter()
                        .position(|option| &option.label == label)
                        .unwrap_or(usize::MAX)
                });
            }
        }
        answers
    }
}

/// Serialize validated answers in the exact success shape. Only call after
/// `validate_answers` and `canonicalize_answers`; ordering is guaranteed by
/// canonicalization, not here.
pub fn answer_result(answers: Vec<AskAnswer>) -> serde_json::Value {
    json!({ "ok": true, "answers": answers })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(label: &str) -> AskOption {
        AskOption {
            label: label.into(),
            description: None,
            preview: None,
        }
    }

    fn question(id: &str, text: &str, options: Vec<AskOption>) -> AskQuestion {
        AskQuestion {
            id: id.into(),
            question: text.into(),
            options,
            header: None,
            multi: false,
            recommended: None,
        }
    }

    fn answer(id: &str, selected: &[&str], custom: Option<&str>) -> AskAnswer {
        AskAnswer {
            id: id.into(),
            selected_options: selected.iter().map(|label| (*label).to_string()).collect(),
            custom_input: custom.map(|custom| custom.to_string()),
        }
    }

    fn err(result: Result<()>) -> String {
        result.expect_err("expected error").to_string()
    }

    #[test]
    fn parse_rejects_invalid_json_without_echo() {
        for input in ["", "not json", "{}", "[]", r#"{"questions":{}}"#] {
            let message = AskArgs::parse(input).expect_err("must fail").to_string();
            assert_eq!(message, "invalid ask arguments", "input: {input}");
        }
    }

    #[test]
    fn parse_rejects_unknown_fields() {
        let message = AskArgs::parse(r#"{"questions":[],"extra":1}"#)
            .expect_err("must fail")
            .to_string();
        assert_eq!(message, "invalid ask arguments");
    }

    #[test]
    fn parse_normalizes_provider_recommendations_and_preserves_content() {
        let cases = [
            (
                json!([{"label":" A "},{"label":"B"},{"label":"C"}]),
                Some(json!(1)),
                Some(1),
            ),
            (
                json!([{"label":" A ","recommended":false},{"label":"B","recommended":true},{"label":"C"}]),
                None,
                Some(1),
            ),
            (
                json!([{"label":" A ","recommended":2},{"label":"B"},{"label":"C"}]),
                None,
                Some(2),
            ),
            (
                json!([{"label":" A ","recommended":"2"},{"label":"B"},{"label":"C"}]),
                None,
                Some(2),
            ),
            (json!([" A ", "B", "C"]), Some(json!("1")), Some(1)),
            (
                json!([{"label":" A ","recommended":false},{"label":"B","recommended":null},{"label":"C"}]),
                Some(json!(null)),
                None,
            ),
            (
                json!([{"label":" A "},{"label":"B","recommended":true},{"label":"C"}]),
                Some(json!(1)),
                Some(1),
            ),
        ];
        for (options, hint, expected) in cases {
            let mut question =
                json!({"id":"choice","question":"完整问题？","multi":null,"options":options});
            if let Some(hint) = hint {
                question["recommended"] = hint;
            }
            let input = json!({"questions":[question]}).to_string();
            // 同时覆盖普通参数与 provider 再次编码成 JSON 字符串的参数。
            for input in [input.clone(), serde_json::to_string(&input).unwrap()] {
                let args = AskArgs::parse(&input).unwrap();
                let question = &args.questions[0];
                assert_eq!(question.recommended, expected);
                assert_eq!(question.question, "完整问题？");
                assert!(!question.multi);
                assert_eq!(
                    question
                        .options
                        .iter()
                        .map(|option| option.label.as_str())
                        .collect::<Vec<_>>(),
                    [" A ", "B", "C"]
                );
                let canonical = serde_json::to_string(&args).unwrap();
                assert_eq!(
                    serde_json::to_string(&AskArgs::parse(&canonical).unwrap()).unwrap(),
                    canonical
                );
            }
        }
    }

    #[test]
    fn parse_keeps_rejecting_duplicate_fields() {
        for input in [
            r#"{"questions":[{"id":"a","question":"t","options":[],"options":[]}]}"#,
            r#"{"questions":[{"id":"a","question":"t","options":[{"label":"x","label":"y"}]}]}"#,
            r#"{"questions":[{"id":"a","question":"t","options":[{"label":"x","recommended":true,"recommended":false}]}]}"#,
            r#"{"questions":[],"questions":[]}"#,
        ] {
            assert_eq!(
                AskArgs::parse(input).unwrap_err().to_string(),
                "invalid ask arguments"
            );
        }
    }

    #[test]
    fn parse_rejects_conflicting_recommendations_and_invalid_shapes_without_echo() {
        let cases = [
            (
                json!({"options":[{"label":"A","recommended":true},{"label":"B","recommended":true}]}),
                "conflicting ask recommended hints",
            ),
            (
                json!({"recommended":0,"options":[{"label":"A"},{"label":"B","recommended":true}]}),
                "conflicting ask recommended hints",
            ),
            (
                json!({"options":[{"label":"A","recommended":9}]}),
                "ask recommended index is out of range",
            ),
            (
                json!({"options":[{"label":"A","recommended":"private-value"}]}),
                "invalid ask recommended index",
            ),
            (
                json!({"recommended":"-1","options":["A"]}),
                "invalid ask recommended index",
            ),
            (
                json!({"recommended":"9999999999999999999999999999999","options":["A"]}),
                "invalid ask recommended index",
            ),
            (
                json!({"recommended":true,"options":["A"]}),
                "invalid ask arguments",
            ),
            (
                json!({"options":[{"label":"A","private-field":true}]}),
                "invalid ask arguments",
            ),
        ];
        for (mut question, expected) in cases {
            question["id"] = json!("private-id");
            question["question"] = json!("private-question");
            let input = json!({"questions":[question]}).to_string();
            assert_eq!(AskArgs::parse(&input).unwrap_err().to_string(), expected);
        }
        let valid = r#"{"questions":[{"id":"a","question":"t","options":[]}]}"#;
        let twice = serde_json::to_string(valid).unwrap();
        let triple = serde_json::to_string(&twice).unwrap();
        assert_eq!(
            AskArgs::parse(&triple).unwrap_err().to_string(),
            "invalid ask arguments"
        );
    }

    #[test]
    fn parse_accepts_valid_shapes() {
        let cases = [
            (
                r#"{"questions":[{"id":"free","question":"Anything to add?","options":[]}]}"#,
                vec![Vec::new()],
            ),
            (
                r#"{"questions":[{"id":"a","question":"t","options":[{"label":"x"}]},
                                  {"id":"b","question":"u","options":[{"label":"x"}]}]}"#,
                vec![vec!["x"], vec!["x"]],
            ),
        ];
        for (input, expected) in cases {
            let args = AskArgs::parse(input).expect("valid ask arguments");
            let labels = args
                .questions
                .iter()
                .map(|question| {
                    question
                        .options
                        .iter()
                        .map(|option| option.label.as_str())
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            assert_eq!(labels, expected, "input: {input}");
        }
    }

    #[test]
    fn parse_rejects_semantic_violations() {
        let cases: [(&str, &str); 9] = [
            (r#"{"questions":[]}"#, "ask requires at least one question"),
            (
                r#"{"questions":[{"id":" ","question":"t","options":[]}]}"#,
                "ask question id must not be blank",
            ),
            (
                r#"{"questions":[{"id":"a","question":"  ","options":[]}]}"#,
                "ask question text must not be blank",
            ),
            (
                r#"{"questions":[{"id":"a","question":"t","options":[{"label":"  "}]}]}"#,
                "ask option label must not be blank",
            ),
            (
                r#"{"questions":[{"id":"a","question":"t","options":[]},{"id":"a","question":"u","options":[]}]}"#,
                "duplicate ask question id",
            ),
            (
                r#"{"questions":[{"id":"a","question":"t","options":[{"label":"x"},{"label":"x"}]}]}"#,
                "duplicate ask option label",
            ),
            (
                r#"{"questions":[{"id":"a","question":"t","options":[{"label":"Other (type your own)"}]}]}"#,
                "ask option label is reserved",
            ),
            (
                r#"{"questions":[{"id":"a","question":"t","options":[{"label":"x"}],"recommended":1}]}"#,
                "ask recommended index is out of range",
            ),
            (
                r#"{"questions":[{"id":"a","question":"t","options":[],"recommended":0}]}"#,
                "ask recommended index is out of range",
            ),
        ];
        for (input, expected) in cases {
            let message = AskArgs::parse(input).expect_err("must fail").to_string();
            assert_eq!(message, expected, "input: {input}");
        }
    }

    #[test]
    fn validate_answers_accepts_valid_shapes() {
        let mut empty_multi = AskArgs {
            questions: vec![question("checks", "Run what?", vec![option("unit")])],
        };
        empty_multi.questions[0].multi = true;
        let custom_only = AskArgs {
            questions: vec![question("free", "Notes?", vec![option("x")])],
        };
        let mut out_of_order = AskArgs {
            questions: vec![
                question(
                    "storage",
                    "Which storage?",
                    vec![option("SQLite"), option("Postgres")],
                ),
                question("checks", "Run what?", vec![option("unit"), option("lint")]),
                question("free", "Notes?", vec![]),
            ],
        };
        out_of_order.questions[1].multi = true;
        let cases: [(AskArgs, Vec<AskAnswer>, &str); 3] = [
            (
                empty_multi,
                vec![answer("checks", &[], None)],
                "empty multi",
            ),
            (
                custom_only,
                vec![answer("free", &[], Some("my own"))],
                "custom-only single",
            ),
            (
                out_of_order,
                vec![
                    answer("checks", &["lint", "unit"], Some("再检查中文")),
                    answer("storage", &["Postgres"], None),
                    answer("free", &[], Some("user text")),
                ],
                "out-of-order content",
            ),
        ];
        for (args, answers, shape) in cases {
            args.validate_answers(&answers).expect(shape);
        }
    }

    #[test]
    fn validate_answers_rejects_violations() {
        let base = |questions: Vec<AskQuestion>| AskArgs { questions };
        let single = || {
            let mut q = question(
                "storage",
                "Which?",
                vec![option("SQLite"), option("Postgres")],
            );
            q.recommended = Some(0);
            vec![q]
        };
        let multi = || {
            let mut q = question("checks", "Run?", vec![option("unit"), option("lint")]);
            q.multi = true;
            vec![q]
        };
        let cases: [(AskArgs, Vec<AskAnswer>, &str); 10] = [
            (base(single()), vec![], "invalid ask response"),
            (
                base(single()),
                vec![answer("bogus", &["SQLite"], None)],
                "invalid ask response",
            ),
            (
                base(vec![question("a", "t", vec![])]),
                vec![answer("a", &[], Some("x")), answer("a", &[], Some("y"))],
                "invalid ask response",
            ),
            (
                base(vec![question("a", "t", vec![]), question("b", "u", vec![])]),
                vec![answer("a", &[], Some("x")), answer("a", &[], Some("y"))],
                "invalid ask response",
            ),
            (
                base(single()),
                vec![answer("storage", &["Nope"], None)],
                "invalid ask response",
            ),
            (
                base(single()),
                vec![answer("storage", &["SQLite", "Postgres"], None)],
                "invalid ask response",
            ),
            (
                base(single()),
                vec![answer("storage", &["SQLite"], Some("also this"))],
                "invalid ask response",
            ),
            (
                base(single()),
                vec![answer("storage", &[], None)],
                "invalid ask response",
            ),
            (
                base(multi()),
                vec![answer("checks", &["unit", "unit"], None)],
                "invalid ask response",
            ),
            (
                base(multi()),
                vec![answer("checks", &["unit"], Some("   "))],
                "invalid ask response",
            ),
        ];
        for (args, answers, expected) in cases {
            let message = err(args.validate_answers(&answers));
            assert_eq!(message, expected);
        }
    }

    #[test]
    fn canonicalize_answers_restores_prescribed_order() {
        let mut args = AskArgs {
            questions: vec![
                question(
                    "storage",
                    "Which?",
                    vec![option("SQLite"), option("Postgres")],
                ),
                question("free", "Notes?", vec![]),
                question(
                    "checks",
                    "Run?",
                    vec![option("lint"), option("unit"), option("docs")],
                ),
                question(
                    "labels",
                    "Which label?",
                    vec![option("  spaced  "), option("中文 label")],
                ),
            ],
        };
        args.questions[2].multi = true;
        args.questions[3].multi = true;
        let answers = vec![
            answer("labels", &["中文 label", "  spaced  "], None),
            answer("checks", &["docs", "unit", "lint"], None),
            answer("storage", &["Postgres"], None),
            answer("free", &[], Some("再检查中文")),
        ];
        args.validate_answers(&answers).expect("valid content");
        let ordered = args.canonicalize_answers(answers);
        assert_eq!(ordered[0].id, "storage");
        assert_eq!(ordered[0].selected_options, vec!["Postgres".to_string()]);
        assert_eq!(ordered[1].id, "free");
        assert_eq!(ordered[1].custom_input.as_deref(), Some("再检查中文"));
        assert_eq!(ordered[2].id, "checks");
        assert_eq!(
            ordered[2].selected_options,
            vec!["lint".to_string(), "unit".to_string(), "docs".to_string()]
        );
        // 前后空格与中文 label 原样保留，只按选项声明顺序重排。
        assert_eq!(ordered[3].id, "labels");
        assert_eq!(
            ordered[3].selected_options,
            vec!["  spaced  ".to_string(), "中文 label".to_string()]
        );
    }
}

/// 所有 Agent 共用一条可取消的 FIFO 交互队列；释放许可后才显示下一项。
#[derive(Default)]
pub struct DialogQueue {
    state: std::sync::Mutex<(u64, std::collections::VecDeque<u64>)>,
    changed: std::sync::Condvar,
}
pub struct DialogPermit<'a> {
    queue: &'a DialogQueue,
    ticket: u64,
}
impl DialogQueue {
    pub fn acquire(&self, token: &CancellationToken) -> Result<DialogPermit<'_>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0 += 1;
        let ticket = state.0;
        state.1.push_back(ticket);
        loop {
            if token.is_cancelled() {
                state.1.retain(|id| *id != ticket);
                self.changed.notify_all();
                return Err(crate::cancellation::Cancelled.into());
            }
            if state.1.front() == Some(&ticket) {
                return Ok(DialogPermit {
                    queue: self,
                    ticket,
                });
            }
            let (next, _) = self
                .changed
                .wait_timeout(state, std::time::Duration::from_millis(25))
                .unwrap_or_else(|e| e.into_inner());
            state = next;
        }
    }
}
impl Drop for DialogPermit<'_> {
    fn drop(&mut self) {
        let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
        state.1.retain(|id| *id != self.ticket);
        self.queue.changed.notify_all();
    }
}

#[cfg(test)]
mod dialog_tests {
    use super::*;
    #[test]
    fn fifo_dialog_queue_removes_cancelled_waiter_and_admits_next() {
        let queue = Arc::new(DialogQueue::default());
        let held = queue.acquire(&CancellationToken::new()).unwrap();
        let cancelled = CancellationToken::new();
        let token = cancelled.clone();
        let second = queue.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = second.acquire(&token);
            tx.send(result.is_err()).unwrap();
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while queue.state.lock().unwrap().1.len() < 2 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        cancelled.cancel();
        assert!(rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap());
        worker.join().unwrap();
        drop(held);
        assert!(queue.acquire(&CancellationToken::new()).is_ok());
    }
}
