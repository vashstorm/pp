use crate::cancellation::CancellationToken;
use anyhow::{Result, bail};
use async_openai::types::chat::FunctionObject;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;

mod arguments;

pub(crate) const RESERVED_CUSTOM_LABEL: &str = "Other (type your own)";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AskArgs {
    pub(crate) questions: Vec<AskQuestion>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AskQuestion {
    pub(crate) id: String,
    pub(crate) question: String,
    pub(crate) options: Vec<AskOption>,
    pub(crate) header: Option<String>,
    #[serde(default)]
    pub(crate) multi: bool,
    pub(crate) recommended: Option<usize>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AskOption {
    pub(crate) label: String,
    pub(crate) description: Option<String>,
    pub(crate) preview: Option<String>,
}
pub(crate) struct AskRequest {
    pub(crate) call_id: String,
    pub(crate) args: Arc<AskArgs>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AskAnswer {
    pub(crate) id: String,
    pub(crate) selected_options: Vec<String>,
    pub(crate) custom_input: Option<String>,
}
pub(crate) type AskHandler =
    Box<dyn Fn(AskRequest, &CancellationToken) -> anyhow::Result<Vec<AskAnswer>> + Send + Sync>;

pub(crate) fn definition() -> FunctionObject {
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
    pub(crate) fn parse(arguments: &str) -> Result<Self> {
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
    pub(crate) fn validate_answers(&self, answers: &[AskAnswer]) -> Result<()> {
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
    pub(crate) fn canonicalize_answers(&self, mut answers: Vec<AskAnswer>) -> Vec<AskAnswer> {
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
pub(crate) fn answer_result(answers: Vec<AskAnswer>) -> serde_json::Value {
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
    fn parse_accepts_empty_options() {
        let args = AskArgs::parse(
            r#"{"questions":[{"id":"free","question":"Anything to add?","options":[]}]}"#,
        )
        .expect("empty options must be valid");
        assert!(args.questions[0].options.is_empty());
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
    fn parse_allows_same_label_across_questions() {
        AskArgs::parse(
            r#"{"questions":[{"id":"a","question":"t","options":[{"label":"x"}]},
                              {"id":"b","question":"u","options":[{"label":"x"}]}]}"#,
        )
        .expect("identical labels in different questions are valid");
    }

    #[test]
    fn validate_answers_accepts_happy_paths() {
        let mut args = AskArgs {
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
        args.questions[1].multi = true;
        let answers = vec![
            answer("storage", &["Postgres"], None),
            answer("checks", &["unit", "lint"], Some("再检查中文")),
            answer("free", &[], Some("user text")),
        ];
        args.validate_answers(&answers).expect("valid answers");
    }

    #[test]
    fn validate_answers_accepts_empty_multiselect() {
        let mut args = AskArgs {
            questions: vec![question("checks", "Run what?", vec![option("unit")])],
        };
        args.questions[0].multi = true;
        args.validate_answers(&[answer("checks", &[], None)])
            .expect("empty multi is valid");
    }

    #[test]
    fn validate_answers_accepts_single_custom_only() {
        let args = AskArgs {
            questions: vec![question("free", "Notes?", vec![option("x")])],
        };
        args.validate_answers(&[answer("free", &[], Some("my own"))])
            .expect("custom-only single");
    }

    #[test]
    fn validate_answers_is_order_agnostic_for_valid_content() {
        let mut args = AskArgs {
            questions: vec![
                question(
                    "storage",
                    "Which?",
                    vec![option("SQLite"), option("Postgres")],
                ),
                question("checks", "Run?", vec![option("unit"), option("lint")]),
            ],
        };
        args.questions[1].multi = true;
        let answers = vec![
            answer("checks", &["lint", "unit"], None),
            answer("storage", &["Postgres"], None),
        ];
        args.validate_answers(&answers)
            .expect("out-of-order content is valid");
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
            assert_eq!(message, "invalid ask response");
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
            ],
        };
        args.questions[2].multi = true;
        let answers = vec![
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
    }

    #[test]
    fn canonicalize_answers_keeps_labels_and_count() {
        let mut args = AskArgs {
            questions: vec![question(
                "q",
                "t",
                vec![option("  spaced  "), option("中文 label")],
            )],
        };
        args.questions[0].multi = true;
        let answers = vec![answer("q", &["中文 label", "  spaced  "], None)];
        args.validate_answers(&answers).expect("valid");
        let ordered = args.canonicalize_answers(answers);
        assert_eq!(ordered.len(), 1);
        assert_eq!(
            ordered[0].selected_options,
            vec!["  spaced  ".to_string(), "中文 label".to_string()]
        );
    }

    #[test]
    fn answer_result_shape_is_exact() {
        let result = answer_result(vec![
            answer("storage", &["Postgres"], None),
            answer("free", &[], Some("再检查中文")),
        ]);
        assert_eq!(
            result,
            json!({
                "ok": true,
                "answers": [
                    {"id": "storage", "selected_options": ["Postgres"], "custom_input": null},
                    {"id": "free", "selected_options": [], "custom_input": "再检查中文"}
                ]
            })
        );
    }

    #[test]
    fn answer_result_preserves_original_labels() {
        let result = answer_result(vec![answer("q", &["  spaced  ", "中文 label"], None)]);
        assert_eq!(
            result["answers"][0]["selected_options"],
            json!(["  spaced  ", "中文 label"])
        );
    }
}
