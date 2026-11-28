use pp_config::config::{IconMode, ToolStyle};

#[derive(Clone, Copy)]
pub(super) struct Glyphs {
    pub rail: &'static str,
    pub rail_end: &'static str,
    pub last: &'static str,
    pub hang: &'static str,
    pub rule: &'static str,
    pub bar: &'static str,
    pub dot: &'static str,
    pub read_group: &'static str,
}

pub(super) fn for_style(style: ToolStyle, icons: IconMode) -> Glyphs {
    if icons == IconMode::Ascii {
        return Glyphs {
            rail: "|",
            rail_end: "`-",
            last: "`-",
            hang: "\\-",
            rule: "-",
            bar: "|",
            dot: "*",
            read_group: "+",
        };
    }
    match style {
        ToolStyle::Classic | ToolStyle::Classic2 => Glyphs {
            rail: "│",
            rail_end: "└─",
            last: "└─",
            hang: "⎿",
            rule: "─",
            bar: "│",
            dot: "●",
            read_group: "◆",
        },
        ToolStyle::Rail => Glyphs {
            rail: "│",
            rail_end: "╰─",
            last: "╰ ",
            hang: "⎿",
            rule: "─",
            bar: "│",
            dot: "●",
            read_group: "◆",
        },
        ToolStyle::Slab => Glyphs {
            rail: "│",
            rail_end: "└─",
            last: "└─",
            hang: "⎿",
            rule: "─",
            bar: "▌",
            dot: "●",
            read_group: "◆",
        },
        ToolStyle::Badge => Glyphs {
            rail: "│",
            rail_end: "└─",
            last: "└─",
            hang: "⎿",
            rule: "┄",
            bar: "│",
            dot: "●",
            read_group: "◆",
        },
        ToolStyle::Hanging => Glyphs {
            rail: "│",
            rail_end: "└─",
            last: "└─",
            hang: "⎿",
            rule: "─",
            bar: "│",
            dot: "●",
            read_group: "●",
        },
        ToolStyle::Ledger => Glyphs {
            rail: "│",
            rail_end: "└─",
            last: "└─",
            hang: "⎿",
            rule: "─",
            bar: "│",
            dot: "●",
            read_group: "◆",
        },
    }
}
