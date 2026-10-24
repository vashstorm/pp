// 按使用重要性降序排列：宽屏双列布局取中点分左右两列，前半为左列；
// Ctrl-C quit 固定末位（测试依赖）。
pub(crate) const TIPS: &[&str] = &[
    "/help for commands",
    "Esc cancel task",
    "/model to choose a model",
    "/new to reset session",
    "/resume to reopen sessions",
    "/plan research first",
    "/compact shrink context",
    "/tree rewind history",
    "/effort to set thinking",
    "/init write AGENTS.md",
    "/skills manage skills",
    "Shift-Tab thinking",
    "Ctrl-P model",
    "F1 help",
    "Ctrl-J newline",
    "Ctrl-C quit",
];

pub(crate) fn text() -> String {
    std::iter::once("Tips:".to_owned())
        .chain(TIPS.iter().map(|tip| {
            let (key, description) = tip.split_once(' ').unwrap_or((tip, ""));
            format!("  {key:<12}{description}")
        }))
        .collect::<Vec<_>>()
        .join("\n")
}
