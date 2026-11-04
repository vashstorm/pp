use std::borrow::Cow;

pub fn display_text(text: &str) -> Cow<'_, str> {
    fn unsafe_control(c: char) -> bool {
        (c <= '\u{1f}' && c != '\n' && c != '\t') || c == '\u{7f}'
    }
    if !text.chars().any(unsafe_control) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .map(|c| if unsafe_control(c) { '\u{fffd}' } else { c })
            .collect(),
    )
}
