//! Tool and argument classification lives in the dispatch layer; permission state and rules are owned by pp-permission.

pub use pp_permission::{
    ApprovedBashScope, Gate, PathWriteKind, PermissionDecision, PermissionHandler,
    PermissionRequest, Tier,
};

/// Built-in tools are tiered by risk; MCP tools without read-only authorization are treated as write tier,
/// and unknown names as exec (a safe default).
pub fn tier_of(name: &str) -> Tier {
    match name {
        "read" | "rg" | "glob" | "web_search" | "todo" | "ask" | "mcp_search" => Tier::Read,
        "edit" | "write" => Tier::Write,
        "bash" | "bash_task" | "eval" | "task" => Tier::Exec,
        "wait" => Tier::Read,
        other if other.starts_with("mcp__") => Tier::Write,
        _ => Tier::Exec,
    }
}

/// Tier with arguments: the action-level tier of `lsp` is resolved by the LSP tool (write actions always
/// Write, workspace diagnostics Exec, invalid arguments conservatively Write); other tools
/// stay fixed by name.
pub fn tier_of_arguments(name: &str, arguments: &str) -> Tier {
    if name == "lsp" {
        return pp_lsp::tool::permission_tier(arguments);
    }
    tier_of(name)
}

#[cfg(test)]
#[path = "permission_tests.rs"]
mod tests;
