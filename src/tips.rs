//! `/tips` 的本地参考清单：全部可见斜杠命令与全局 TUI 快捷键。
//!
//! 两组内按用户看到的键名忽略大小写稳定排序；对齐宽度与单行宽度由
//! [`key_width`]／[`row_width`] 派生，行模式对齐与 TUI 双列布局共用同一预算。

pub(crate) struct Tip {
    pub(crate) label: &'static str,
    pub(crate) description: &'static str,
}

pub(crate) struct Section {
    pub(crate) title: &'static str,
    pub(crate) tips: &'static [Tip],
}

const fn tip(label: &'static str, description: &'static str) -> Tip {
    Tip { label, description }
}

// 平台相关的组合键：macOS 为 Option，其他平台为 Ctrl（见 `shortcuts`）。
const fn platform_tip(macos: &'static str, other: &'static str, description: &'static str) -> Tip {
    Tip {
        label: crate::shortcuts::platform_text(macos, other),
        description,
    }
}

// 说明不超过 20 列、键名不超过 16 列，80 列终端（正文 78 列）仍能并排两列。
pub(crate) const SECTIONS: &[Section] = &[
    Section {
        title: "Commands",
        tips: &[
            tip("/agents", "list subagent roles"),
            tip("/changes", "review/undo edits"),
            tip("/compact", "summarize context"),
            tip("/effort", "set thinking level"),
            tip("/help", "list commands/usage"),
            tip("/init", "create AGENTS.md"),
            tip("/login", "log in subscription"),
            tip("/logout", "remove saved login"),
            tip("/mcp", "manage MCP servers"),
            tip("/model", "switch model"),
            tip("/new", "clear chat"),
            tip("/plan", "plan, then approve"),
            tip("/plan-list", "manage saved plans"),
            tip("/quit", "quit pp"),
            tip("/resume", "reopen a session"),
            tip("/Roles", "role model/effort"),
            tip("/skills", "toggle skills"),
            tip("/subagents", "control subagents"),
            tip("/task-test", "debug subagent batch"),
            tip("/tips", "show all tips"),
            tip("/tree", "browse & rewind chat"),
        ],
    },
    Section {
        title: "Shortcuts",
        tips: &[
            tip("Enter", "send or steer"),
            platform_tip("⌥ + Enter", "Alt + Enter", "queue a follow-up"),
            tip("Ctrl + J", "insert a newline"),
            tip("Esc", "cancel or dismiss"),
            tip("Esc Esc", "open session tree"),
            tip("F1", "show commands/usage"),
            tip("Tab", "complete input"),
            tip("Shift + Tab", "cycle thinking level"),
            tip("Ctrl + O", "toggle tool output"),
            tip("Ctrl + A", "jump to line start"),
            tip("Ctrl + E", "jump to line end"),
            tip("Ctrl + K", "delete to line end"),
            tip("Ctrl + C", "quit pp"),
            platform_tip("⌥ + P", "Ctrl + P", "open model picker"),
            tip("Ctrl + S", "save model default"),
            platform_tip("⌥ + S", "Ctrl + S", "save effort default"),
            tip("Ctrl + F", "favorite this model"),
            platform_tip("⌥ + Q", "Ctrl + Q", "list pending input"),
            platform_tip("⌥ + R", "Ctrl + R", "resume queued input"),
            platform_tip("⌥ + T", "Ctrl + T", "toggle TODO panel"),
            platform_tip("⌥ + PgUp/PgDn", "Ctrl + PgUp/PgDn", "page the TODO panel"),
            tip("PgUp/PgDn", "scroll transcript"),
            tip("↑ / ↓", "move, history, menu"),
        ],
    },
];

/// 过滤出当前构建可见的条目，并按用户看到的键名（忽略大小写）稳定排序。
///
/// 复用命令注册表的可见判据：它只对 debug 专用的 `/task-test` 返回 false，
/// 快捷键标签不以 `/` 开头，因此恒定通过。
pub(crate) fn sorted(section: &'static Section) -> Vec<&'static Tip> {
    let mut tips: Vec<&'static Tip> = section
        .tips
        .iter()
        .filter(|tip| crate::commands::visible(tip.label))
        .collect();
    tips.sort_by_key(|tip| tip.label.to_lowercase());
    tips
}

/// 键名对齐宽度：最长可见键名，下限 12 列保留窄屏观感。
pub(crate) fn key_width() -> usize {
    visible()
        .map(|tip| tip.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(12)
}

/// 单行条目宽度：对齐键名 + 分隔空格 + 最长说明。
pub(crate) fn row_width() -> usize {
    key_width()
        + 1
        + visible()
            .map(|tip| tip.description.chars().count())
            .max()
            .unwrap_or(0)
}

/// 行模式输出：`Tips:`、空行、分组标题与对齐条目，组间留空行。
pub(crate) fn text() -> String {
    let key_width = key_width();
    let mut lines = vec!["Tips:".to_owned()];
    for section in SECTIONS {
        lines.push(String::new());
        lines.push(section.title.to_owned());
        lines.extend(
            sorted(section)
                .into_iter()
                .map(|tip| format!("  {:<key_width$} {}", tip.label, tip.description)),
        );
    }
    lines.join("\n")
}

fn visible() -> impl Iterator<Item = &'static Tip> {
    SECTIONS.iter().flat_map(sorted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::COMMANDS;
    use std::collections::BTreeSet;

    fn section(title: &str) -> &'static Section {
        SECTIONS
            .iter()
            .find(|section| section.title == title)
            .expect("tips section")
    }

    #[test]
    fn tips_commands_match_registry() {
        let tips: BTreeSet<&str> = sorted(section("Commands"))
            .into_iter()
            .map(|tip| tip.label)
            .collect();
        let registry: BTreeSet<&str> = COMMANDS
            .iter()
            .filter(|spec| crate::commands::visible(spec.name))
            .map(|spec| spec.name)
            .collect();
        // 双向比较：既发现新增命令缺提示，也发现淘汰命令残留。
        assert_eq!(tips, registry);
        assert_eq!(
            tips.contains("/task-test"),
            cfg!(debug_assertions),
            "debug-only command must follow the registry visibility"
        );
    }

    #[test]
    fn tips_respect_layout_budget() {
        for tip in visible() {
            assert!(
                tip.label.chars().count() <= 16,
                "key too wide: {}",
                tip.label
            );
            assert!(
                !tip.description.is_empty(),
                "missing description: {}",
                tip.label
            );
            assert!(
                tip.description.chars().count() <= 20,
                "description too wide: {}",
                tip.label
            );
        }
        assert!(key_width() >= 12);
        // 80 列终端正文为 78 列，右侧取消先前须仍放得下两列条目。
        assert!(
            row_width() * 2 + 4 <= 78,
            "row width {} exceeds the two-column budget",
            row_width()
        );
    }

    #[test]
    fn tips_text_groups_and_aligns() {
        let key_width = key_width();
        let text = text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Tips:");
        assert_eq!(lines[1], "");
        let commands_title = lines
            .iter()
            .position(|line| *line == "Commands")
            .expect("commands title");
        let shortcuts_title = lines
            .iter()
            .position(|line| *line == "Shortcuts")
            .expect("shortcuts title");
        assert_eq!(commands_title, 2);
        assert!(commands_title < shortcuts_title);

        let mut index = commands_title + 1;
        for tip in sorted(section("Commands")) {
            let line = lines.get(index).expect("command row");
            assert!(line.starts_with("  "), "{line}");
            assert_eq!(
                description_column(line, tip.description),
                2 + key_width + 1,
                "{line} must align its description after the key column"
            );
            index += 1;
        }
        assert_eq!(lines[index], "");
        assert_eq!(lines[index + 1], "Shortcuts");

        index += 2;
        for tip in sorted(section("Shortcuts")) {
            let line = lines.get(index).expect("shortcut row");
            assert!(line.starts_with("  "), "{line}");
            assert_eq!(
                description_column(line, tip.description),
                2 + key_width + 1,
                "{line}"
            );
            index += 1;
        }
        assert_eq!(index, lines.len());
    }

    // 说明的起始列按字符计，与 `format!` 的键名补白口径一致。
    fn description_column(line: &str, description: &str) -> usize {
        let offset = line.find(description).expect("description present");
        line[..offset].chars().count()
    }
}
