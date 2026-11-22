//! `--allowed-tools` 解析、工具能力白名单与权限门预批准同步。

use anyhow::{Result, bail, ensure};
use std::collections::BTreeSet;

const VALID_TOOL_NAMES: &str = "Read, Grep (rg), Glob, Edit, Write, Bash, BashTask, Eval, WebSearch, Lsp, Todo, Wait, Task, mcp__*, mcp__<server>__*, *, none";

/// 工具能力白名单，同时驱动 schema 暴露、执行期门禁与 CLI 预批准。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllowedTools {
    exact: BTreeSet<String>,
    prefixes: Vec<String>,
    all_standard: bool,
}

impl AllowedTools {
    /// 非交互 `-p` 模式的默认只读白名单：`Read, Grep, Glob`。
    pub fn default_non_interactive() -> Self {
        Self::exact(["read".to_owned(), "rg".to_owned(), "glob".to_owned()])
    }

    /// 由精确内部工具名集合构造白名单（供子 Agent 与测试复用）。
    pub fn exact(names: impl IntoIterator<Item = String>) -> Self {
        Self {
            exact: names.into_iter().collect(),
            prefixes: Vec::new(),
            all_standard: false,
        }
    }

    /// 解析 CLI `--allowed-tools` 参数列表（支持逗号分隔与重复传入）。
    pub fn parse_cli(values: &[String]) -> Result<Self> {
        let mut parsed = Self::default();
        let mut saw_none = false;
        let mut token_count = 0usize;

        for raw in values {
            for part in raw.split(',') {
                let token = part.trim();
                ensure!(
                    !token.is_empty(),
                    "empty entry in --allowed-tools; valid tools: {VALID_TOOL_NAMES}"
                );
                token_count += 1;
                if token.eq_ignore_ascii_case("none") {
                    saw_none = true;
                    continue;
                }
                parsed.add_token(token)?;
            }
        }

        ensure!(
            token_count > 0,
            "--allowed-tools must not be empty; valid tools: {VALID_TOOL_NAMES}"
        );
        ensure!(
            !saw_none || token_count == 1,
            "`none` cannot be combined with other tools in --allowed-tools"
        );
        Ok(parsed)
    }

    fn add_token(&mut self, token: &str) -> Result<()> {
        if token == "*" || token.eq_ignore_ascii_case("all") {
            self.all_standard = true;
            return Ok(());
        }
        if token == "mcp__*" {
            self.push_prefix("mcp__".to_owned());
            return Ok(());
        }
        if let Some(prefix) = token.strip_suffix('*') {
            if prefix.starts_with("mcp__")
                && prefix.ends_with("__")
                && prefix.len() > "mcp____".len()
            {
                self.push_prefix(prefix.to_owned());
                return Ok(());
            }
            bail!(
                "unsupported wildcard `{token}` in --allowed-tools; valid tools: {VALID_TOOL_NAMES}"
            );
        }
        if let Some(rest) = token.strip_prefix("mcp__") {
            ensure!(
                !rest.is_empty() && !token.contains('*'),
                "invalid MCP tool `{token}` in --allowed-tools"
            );
            self.exact.insert(token.to_owned());
            return Ok(());
        }

        match token.to_ascii_lowercase().as_str() {
            "read" => {
                self.exact.insert("read".to_owned());
            }
            "grep" | "rg" => {
                self.exact.insert("rg".to_owned());
            }
            "glob" => {
                self.exact.insert("glob".to_owned());
            }
            "edit" => {
                self.exact.insert("edit".to_owned());
            }
            "write" => {
                self.exact.insert("write".to_owned());
            }
            "bash" => {
                self.exact.insert("bash".to_owned());
                self.exact.insert("bash_task".to_owned());
                self.exact.insert("wait".to_owned());
            }
            "bashtask" | "bash_task" => {
                self.exact.insert("bash_task".to_owned());
            }
            "eval" => {
                self.exact.insert("eval".to_owned());
            }
            "websearch" | "web_search" => {
                self.exact.insert("web_search".to_owned());
            }
            "lsp" => {
                self.exact.insert("lsp".to_owned());
            }
            "todo" => {
                self.exact.insert("todo".to_owned());
            }
            "wait" => {
                self.exact.insert("wait".to_owned());
            }
            "task" => {
                self.exact.insert("task".to_owned());
                self.exact.insert("wait".to_owned());
            }
            _ => {
                bail!("unknown tool `{token}` in --allowed-tools; valid tools: {VALID_TOOL_NAMES}")
            }
        }
        Ok(())
    }

    fn push_prefix(&mut self, prefix: String) {
        if !self.prefixes.contains(&prefix) {
            self.prefixes.push(prefix);
        }
    }

    /// 判断内部工具名是否被当前白名单允许。
    pub fn allows(&self, name: &str) -> bool {
        (self.all_standard && name != "task")
            || self.exact.contains(name)
            || self.prefixes.iter().any(|prefix| name.starts_with(prefix))
    }

    /// 非交互下只有显式包含 `Task` 才启用 subagent 宿主与 `task` 工具。
    pub fn enables_subagents(&self) -> bool {
        self.exact.contains("task")
    }

    /// 未允许任何 MCP 工具时跳过启动期 MCP 建连与等待。
    pub fn allows_any_mcp(&self) -> bool {
        self.all_standard
            || self.exact.iter().any(|name| name.starts_with("mcp__"))
            || self
                .prefixes
                .iter()
                .any(|prefix| prefix.starts_with("mcp__"))
    }

    /// 仅在允许 `lsp` 或存在文件修改工具（保存联动诊断）时初始化 LSP。
    pub fn allows_lsp_lifecycle(&self) -> bool {
        self.allows("lsp") || self.allows("edit") || self.allows("write")
    }

    /// 方案 B：将 `--allowed-tools` 同步为本次运行的权限门预批准集合。
    pub fn apply_permission_grants(&self, gate: &pp_permission::Gate) {
        gate.grant_allowed_tools(
            self.exact.iter().cloned(),
            self.prefixes.clone(),
            self.all_standard,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aliases_wildcards_and_exclusions() {
        let default = AllowedTools::default_non_interactive();
        assert!(default.allows("read"));
        assert!(default.allows("rg"));
        assert!(default.allows("glob"));
        assert!(!default.allows("edit"));
        assert!(!default.allows("bash"));
        assert!(!default.enables_subagents());
        assert!(!default.allows_any_mcp());
        assert!(!default.allows_lsp_lifecycle());

        let parsed = AllowedTools::parse_cli(&[
            "Read, Grep, Edit, Bash".to_owned(),
            "mcp__github__*".to_owned(),
        ])
        .unwrap();
        assert!(parsed.allows("read"));
        assert!(parsed.allows("rg"));
        assert!(parsed.allows("edit"));
        assert!(parsed.allows("bash"));
        assert!(parsed.allows("bash_task"));
        assert!(parsed.allows("wait"));
        assert!(parsed.allows("mcp__github__search"));
        assert!(!parsed.allows("mcp__other__search"));
        assert!(!parsed.enables_subagents());
        assert!(parsed.allows_any_mcp());
        assert!(parsed.allows_lsp_lifecycle());

        let wildcard = AllowedTools::parse_cli(&["*".to_owned()]).unwrap();
        assert!(wildcard.allows("write"));
        assert!(wildcard.allows("mcp__any__tool"));
        assert!(!wildcard.allows("task"));
        assert!(!wildcard.enables_subagents());

        let with_task = AllowedTools::parse_cli(&["*,Task".to_owned()]).unwrap();
        assert!(with_task.allows("task"));
        assert!(with_task.allows("wait"));
        assert!(with_task.enables_subagents());

        let none = AllowedTools::parse_cli(&["none".to_owned()]).unwrap();
        assert!(!none.allows("read"));
        assert!(!none.enables_subagents());
    }

    #[test]
    fn rejects_invalid_specs() {
        for bad in [
            vec![],
            vec!["".to_owned()],
            vec!["Read,".to_owned()],
            vec!["UnknownTool".to_owned()],
            vec!["none,Read".to_owned()],
            vec!["mcp____*".to_owned()],
            vec!["ed*".to_owned()],
        ] {
            assert!(AllowedTools::parse_cli(&bad).is_err(), "{bad:?}");
        }
    }
}
