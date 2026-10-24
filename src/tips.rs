pub(crate) const TIPS: &[&str] = &[
    "/help for commands",
    "/model to choose a model",
    "/effort to set thinking",
    "Enter steer",
    "Alt-Enter queue",
    "Ctrl-Q pending messages",
    "Ctrl-R resume queue",
    "Shift-Tab thinking",
    "Ctrl-P model",
    "F1 help",
    "Ctrl-J newline",
    "Ctrl-C quit",
];

pub(crate) fn text() -> String {
    TIPS.join("\n")
}
