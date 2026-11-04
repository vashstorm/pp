use crossterm::event::KeyModifiers;

pub(crate) const FUNCTION_MODIFIER: KeyModifiers = if cfg!(target_os = "macos") {
    KeyModifiers::ALT
} else {
    KeyModifiers::CONTROL
};

pub(crate) const fn platform_text(macos: &'static str, other: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        macos
    } else {
        other
    }
}
