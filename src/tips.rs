pub(crate) const TIPS: &[&str] = &[
    "/help for commands",
    "/model to choose a model",
    "/effort to set thinking",
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
