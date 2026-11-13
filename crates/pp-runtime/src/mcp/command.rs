//! `/mcp` 子命令表与 `View` 的文本渲染。

use super::View;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Help,
    List,
    Enable(String),
    Disable(String),
    Reconnect(String),
    Reload,
}
pub const SUBCOMMANDS: &[(&str, &str, bool)] = &[
    ("list", "List all MCP servers with their status", false),
    ("enable", "Enable an MCP server", true),
    ("disable", "Disable an MCP server", true),
    ("reconnect", "Reconnect an MCP server", true),
    ("reload", "Reload config and MCP servers", false),
    ("help", "Show MCP command help", false),
];
pub fn help() -> String {
    SUBCOMMANDS
        .iter()
        .map(|(name, description, arg)| {
            format!(
                "/mcp {name}{}  {description}",
                if *arg { " <name>" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
impl View {
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = self.diagnostics.clone();
        warnings.extend(
            self.entries
                .iter()
                .filter(|e| e.status.starts_with("failed") || e.status.starts_with("unsupported"))
                .map(|e| format!("MCP {}: {}", pp_core::text::display_text(&e.name), e.status)),
        );
        warnings
    }
    pub fn text(&self) -> String {
        let mut text = String::from("Configured MCP Servers\n");
        for entry in &self.entries {
            text.push_str(&format!(
                "{} ({})  {} — {}\n",
                entry.source,
                entry.path.display(),
                pp_core::text::display_text(&entry.name),
                entry.status
            ));
        }
        if self.entries.is_empty() {
            text.push_str("No MCP servers configured.\n");
        }
        for diagnostic in &self.diagnostics {
            text.push_str(diagnostic);
            text.push('\n');
        }
        text
    }
}
