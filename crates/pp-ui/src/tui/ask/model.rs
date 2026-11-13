//! 答案收集、菜单重建与文本／图标辅助。

use super::*;

impl Panel {
    /// 按问题原顺序构造答案；多选 label 按原选项顺序，custom 保留原文。
    pub(super) fn collect_answers(&self) -> Vec<AskAnswer> {
        self.questions
            .questions
            .iter()
            .enumerate()
            .map(|(index, question)| {
                let state = &self.states[index];
                let selected_options = state
                    .selected
                    .iter()
                    .filter_map(|&item| question.options.get(item))
                    .map(|option| option.label.clone())
                    .collect();
                let custom_input = (!state.custom.is_empty()).then(|| state.custom.clone());
                AskAnswer {
                    id: question.id.clone(),
                    selected_options,
                    custom_input,
                }
            })
            .collect()
    }

    pub(super) fn rebuild_menu(&mut self, index: usize) {
        self.review = None;
        let question = &self.questions.questions[index];
        let state = &mut self.states[index];
        let custom = !state.custom.is_empty();
        for item in state.menu.items_mut() {
            let selected = if item.value == question.options.len() {
                custom
            } else {
                state.selected.contains(&item.value)
            };
            item.icon = Some(selection_icon(question.multi, selected));
            item.current = selected;
            if item.value == question.options.len() {
                item.description = menu_text(&state.custom);
            }
        }
    }
}

/// 选项列表：原选项加推荐徽标，末尾固定追加自定义回答入口。
pub(super) fn insert_custom_text(editor: &mut TextArea<'static>, text: &str) {
    // TextArea::insert_str strips one CR before every LF and at EOF. Duplicate
    // only those CRs in its input so the stored editor text remains byte-exact.
    if !text.contains("\r\n") && !text.ends_with('\r') {
        editor.insert_str(text);
        return;
    }
    let mut insertion = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            insertion.push('\n');
        }
        insertion.push_str(line);
        if line.ends_with('\r') {
            insertion.push('\r');
        }
    }
    editor.insert_str(insertion);
}

pub(super) fn menu_text(text: &str) -> String {
    let safe = display_text(text);
    let mut output = String::with_capacity(safe.len());
    let mut column = 0;
    for part in safe.split_inclusive(['\n', '\t']) {
        let (plain, separator) = if let Some(text) = part.strip_suffix('\n') {
            (text, Some('\n'))
        } else if let Some(text) = part.strip_suffix('\t') {
            (text, Some('\t'))
        } else {
            (part, None)
        };
        for grapheme in Span::raw(plain).styled_graphemes(Style::default()) {
            output.push_str(grapheme.symbol);
            column += Span::raw(grapheme.symbol).width();
        }
        if let Some(separator) = separator {
            let spaces = if separator == '\t' { 4 - column % 4 } else { 1 };
            output.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        }
    }
    output
}

pub(super) fn menu_items(
    question: &pp_core::interaction::AskQuestion,
    selected: &[usize],
    custom: bool,
) -> Vec<MenuItem<usize>> {
    let count = question.options.len();
    let mut items = Vec::with_capacity(count + 1);
    for (index, option) in question.options.iter().enumerate() {
        let chosen = selected.contains(&index);
        let icon = selection_icon(question.multi, chosen);
        let mut label = menu_text(&option.label);
        if question.recommended == Some(index) {
            label.push_str(" (Recommended)");
        }
        items.push(MenuItem {
            value: index,
            label,
            description: option
                .description
                .as_deref()
                .map(menu_text)
                .unwrap_or_default(),
            icon: Some(icon),
            current: chosen,
        });
    }
    let other_icon = selection_icon(question.multi, custom);
    items.push(MenuItem {
        value: count,
        label: OTHER_LABEL.to_string(),
        description: String::new(),
        icon: Some(other_icon),
        current: custom,
    });
    items
}

pub(in crate::tui) fn selection_icon(multi: bool, selected: bool) -> &'static str {
    // 使用同系列的大轮廓配对字形，避免选中后因字体回退而缩小。
    match (multi, selected) {
        (true, true) => "󰄲",
        (true, false) => "󰄱",
        (false, true) => "󰐾",
        (false, false) => "󰐽",
    }
}

pub(super) fn hint_line(hint: &'static str) -> Line<'static> {
    Line::styled(hint, Style::default().fg(DIM))
}
