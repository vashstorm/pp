//! 工具与参数分类在分发层维护，权限状态与规则由 pp-permission 管理。

pub use pp_permission::{
    ApprovedBashScope, Gate, PathWriteKind, PermissionDecision, PermissionHandler,
    PermissionRequest, Tier,
};

/// 内置工具按风险分级；未获只读授权的 MCP 工具按写档处理，
/// 未知名按 exec 处理（安全默认）。
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

/// 结合参数的 tier：`lsp` 的动作级分档由 LSP 工具解析（写入动作一律按
/// Write，工作区 diagnostics 按 Exec，参数无效时保守按 Write）；其余工具
/// 仍按名称固定分档。
pub fn tier_of_arguments(name: &str, arguments: &str) -> Tier {
    if name == "lsp" {
        return pp_lsp::tool::permission_tier(arguments);
    }
    tier_of(name)
}

#[cfg(test)]
#[path = "permission_tests.rs"]
mod tests;
