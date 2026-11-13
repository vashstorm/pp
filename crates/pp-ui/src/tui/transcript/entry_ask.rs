//! Ask 卡片的构造、结果收尾与正文布局。

use super::*;

impl AskBlock {
    pub(super) fn new(args: pp_core::interaction::AskArgs) -> Self {
        let previews = args
            .questions
            .iter()
            .map(|question| {
                question
                    .options
                    .iter()
                    .map(|option| {
                        option
                            .preview
                            .as_deref()
                            .filter(|text| !text.trim().is_empty())
                            .map(crate::tui::markdown::parse)
                    })
                    .collect()
            })
            .collect();
        Self {
            args,
            answers: None,
            previews,
        }
    }

    pub(super) fn finish(&mut self, result: &Value) -> bool {
        match result.get("ok").and_then(Value::as_bool) {
            Some(false) => return true,
            Some(true) => {}
            None => return false,
        }
        let Some(answers) = result.get("answers").and_then(|answers| {
            serde_json::from_value::<Vec<pp_core::interaction::AskAnswer>>(answers.clone()).ok()
        }) else {
            return false;
        };
        if self.args.validate_answers(&answers).is_err() {
            return false;
        }
        self.answers = Some(self.args.canonicalize_answers(answers));
        true
    }

    pub(super) fn layout(&self, width: u16) -> Vec<Line<'static>> {
        let mut rows = Vec::new();
        for (question_index, question) in self.args.questions.iter().enumerate() {
            rows.extend(wrap_block(
                block_line(
                    format!(
                        "{} · {}",
                        question.header.as_deref().unwrap_or(&question.id),
                        question.question
                    ),
                    Style::default()
                        .fg(palette::ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
                width,
                "",
            ));
            let answer = self
                .answers
                .as_ref()
                .and_then(|answers| answers.iter().find(|answer| answer.id == question.id));
            for (option_index, option) in question.options.iter().enumerate() {
                let selected =
                    answer.is_some_and(|answer| answer.selected_options.contains(&option.label));
                rows.extend(wrap_block(
                    block_line(
                        format!(
                            "{} {}",
                            crate::tui::ask::selection_icon(question.multi, selected),
                            option.label
                        ),
                        if selected {
                            Style::default()
                        } else {
                            Style::default().fg(palette::MUTED)
                        },
                    ),
                    width,
                    "  ",
                ));
                if let Some(description) = option
                    .description
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
                {
                    rows.extend(wrap_block(
                        block_line(
                            format!("  {description}"),
                            Style::default().fg(palette::MUTED),
                        ),
                        width,
                        "  ",
                    ));
                }
                if let Some(document) = &self.previews[question_index][option_index] {
                    rows.extend(document.layout(width.saturating_sub(2)).into_iter().map(
                        |mut line| {
                            line.spans.insert(0, Span::raw("  "));
                            line
                        },
                    ));
                }
            }
            if let Some(answer) = answer {
                if let Some(custom) = &answer.custom_input {
                    rows.extend(wrap_block(
                        block_line(format!("Other: {custom}"), Style::default()),
                        width,
                        "  ",
                    ));
                }
                if answer.selected_options.is_empty() && answer.custom_input.is_none() {
                    rows.extend(wrap_block(
                        block_line("No options selected", Style::default().fg(palette::MUTED)),
                        width,
                        "",
                    ));
                }
            }
        }
        rows
    }
}
