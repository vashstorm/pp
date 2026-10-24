// 按使用重要性降序排列：宽屏双列布局取中点分左右两列，前半为左列；
// 每条说明不超过 24 列，确保最窄双列布局（76 列）仍能对齐。
pub(crate) const TIPS: &[(&str, &str)] = &[
    ("/help", "list commands and usage"),
    ("Esc", "cancel the running task"),
    ("/model", "switch model, keep chat"),
    ("/new", "clear chat, keep model"),
    ("/resume", "reopen a saved session"),
    ("/plan", "plan, then approve edits"),
    ("/compact", "summarize older context"),
    ("/tree", "browse & rewind history"),
    ("/effort", "set model thinking level"),
    ("/init", "create/update AGENTS.md"),
    ("/skills", "enable or disable skills"),
    ("Shift + Tab", "cycle thinking levels"),
    (
        crate::shortcuts::platform_text("⌥ + P", "Ctrl + P"),
        "open the model picker",
    ),
    ("F1", "show commands and usage"),
    ("Ctrl + J", "insert a newline"),
    ("Ctrl + C", "quit pp"),
];

pub(crate) fn text() -> String {
    std::iter::once("Tips:".to_owned())
        .chain(
            TIPS.iter()
                .map(|(key, description)| format!("  {key:<12}{description}")),
        )
        .collect::<Vec<_>>()
        .join("\n")
}
