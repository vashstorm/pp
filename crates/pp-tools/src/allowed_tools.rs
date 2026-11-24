//! `--allowed-tools` / `--disallowed-tools` 解析、工具能力白名单与权限门预批准同步。

use anyhow::{Result, bail, ensure};
use pp_permission::BashCommandPattern;
use std::collections::BTreeSet;

const VALID_TOOL_NAMES: &str = "Read, Grep (rg), Glob, Edit, Write, Bash, Bash(cmd:*), BashTask, Eval, WebSearch, Lsp, Todo, Wait, Task, mcp__*, mcp__<server>__*, *, none";

/// 工具能力白名单与黑名单，同时驱动 schema 暴露、执行期门禁与 CLI 预批准。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllowedTools {
    exact: BTreeSet<String>,
    prefixes: Vec<String>,
    all_standard: bool,
    bash_allowed: Vec<BashCommandPattern>,
    disallowed_exact: BTreeSet<String>,
    disallowed_prefixes: Vec<String>,
    disallowed_all: bool,
    bash_disallowed: Vec<BashCommandPattern>,
    /// 仅当由 `--allowed-tools`（含 `-p` 默认只读集合）构造时才同步写入权限门预批准；
    /// 仅传 `--disallowed-tools` 的限制集合不得反向赋予预批准。
    preapprove: bool,
}

impl AllowedTools {
    /// 非交互 `-p` 模式的默认只读白名单：`Read, Grep, Glob`。
    pub fn default_non_interactive() -> Self {
        Self {
            preapprove: true,
            ..Self::exact(["read".to_owned(), "rg".to_owned(), "glob".to_owned()])
        }
    }

    /// 由精确内部工具名集合构造白名单（供子 Agent 与测试复用）。
    pub fn exact(names: impl IntoIterator<Item = String>) -> Self {
        Self {
            exact: names.into_iter().collect(),
            ..Self::default()
        }
    }

    /// 将子 Agent 工具白名单与父级 `--allowed-tools` / `--disallowed-tools` 求交集，
    /// 并继承父级的黑名单及 `Bash(...)` AST 规则。
    pub fn restrict_for_child(&self, child_names: impl IntoIterator<Item = String>) -> Self {
        let exact: BTreeSet<String> = child_names
            .into_iter()
            .filter(|name| name != "task" && self.allows(name))
            .collect();
        let bash_allowed = if exact.contains("bash") && !self.has_unbounded_bash() {
            self.bash_allowed.clone()
        } else {
            Vec::new()
        };
        Self {
            exact,
            prefixes: Vec::new(),
            all_standard: false,
            bash_allowed,
            disallowed_exact: self.disallowed_exact.clone(),
            disallowed_prefixes: self.disallowed_prefixes.clone(),
            disallowed_all: self.disallowed_all,
            bash_disallowed: self.bash_disallowed.clone(),
            preapprove: self.preapprove,
        }
    }

    /// 交互模式仅指定 `--disallowed-tools` 时使用的全量受限基线（不赋予 CLI 预批准）。
    fn all_with_task_no_preapprove() -> Self {
        Self {
            all_standard: true,
            exact: BTreeSet::from(["task".to_owned()]),
            preapprove: false,
            ..Self::default()
        }
    }

    /// 解析 CLI `--allowed-tools` 参数列表（支持逗号分隔、括号内 Bash 规则与重复传入）。
    pub fn parse_cli(values: &[String]) -> Result<Self> {
        let mut parsed = Self {
            preapprove: true,
            ..Self::default()
        };
        let mut saw_none = false;
        let mut token_count = 0usize;

        for raw in values {
            for token in split_tool_specs(raw)? {
                token_count += 1;
                if token.eq_ignore_ascii_case("none") {
                    saw_none = true;
                    continue;
                }
                parsed.add_allowed_token(token)?;
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

    /// 基于可选白名单叠加 `--disallowed-tools` 黑名单（黑名单优先级高于白名单）。
    pub fn with_disallowed(
        base: Option<Self>,
        disallowed_values: &[String],
    ) -> Result<Option<Self>> {
        if disallowed_values.is_empty() {
            return Ok(base);
        }
        let mut combined = base.unwrap_or_else(Self::all_with_task_no_preapprove);
        let mut token_count = 0usize;
        for raw in disallowed_values {
            for token in split_tool_specs(raw)? {
                token_count += 1;
                combined.add_disallowed_token(token)?;
            }
        }
        ensure!(
            token_count > 0,
            "--disallowed-tools must not be empty; valid tools: {VALID_TOOL_NAMES}"
        );
        Ok(Some(combined))
    }

    fn add_allowed_token(&mut self, token: &str) -> Result<()> {
        if let Some(inner) = parse_bash_wrapper(token)? {
            match parse_bash_pattern(inner)? {
                Some(pattern) => {
                    if !self.bash_allowed.contains(&pattern) {
                        self.bash_allowed.push(pattern);
                    }
                }
                None => {
                    self.exact.insert("bash".to_owned());
                    self.exact.insert("bash_task".to_owned());
                    self.exact.insert("wait".to_owned());
                }
            }
            return Ok(());
        }
        if token == "*" || token.eq_ignore_ascii_case("all") {
            self.all_standard = true;
            return Ok(());
        }
        if token == "mcp__*" {
            push_unique(&mut self.prefixes, "mcp__".to_owned());
            return Ok(());
        }
        if let Some(prefix) = token.strip_suffix('*') {
            if prefix.starts_with("mcp__")
                && prefix.ends_with("__")
                && prefix.len() > "mcp____".len()
            {
                push_unique(&mut self.prefixes, prefix.to_owned());
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

    fn add_disallowed_token(&mut self, token: &str) -> Result<()> {
        if let Some(inner) = parse_bash_wrapper(token)? {
            match parse_bash_pattern(inner)? {
                Some(pattern) => {
                    if !self.bash_disallowed.contains(&pattern) {
                        self.bash_disallowed.push(pattern);
                    }
                }
                None => {
                    self.disallowed_exact.insert("bash".to_owned());
                    self.disallowed_exact.insert("bash_task".to_owned());
                }
            }
            return Ok(());
        }
        if token == "*" || token.eq_ignore_ascii_case("all") {
            self.disallowed_all = true;
            return Ok(());
        }
        if token == "mcp__*" {
            push_unique(&mut self.disallowed_prefixes, "mcp__".to_owned());
            return Ok(());
        }
        if let Some(prefix) = token.strip_suffix('*') {
            if prefix.starts_with("mcp__")
                && prefix.ends_with("__")
                && prefix.len() > "mcp____".len()
            {
                push_unique(&mut self.disallowed_prefixes, prefix.to_owned());
                return Ok(());
            }
            bail!(
                "unsupported wildcard `{token}` in --disallowed-tools; valid tools: {VALID_TOOL_NAMES}"
            );
        }
        if let Some(rest) = token.strip_prefix("mcp__") {
            ensure!(
                !rest.is_empty() && !token.contains('*'),
                "invalid MCP tool `{token}` in --disallowed-tools"
            );
            self.disallowed_exact.insert(token.to_owned());
            return Ok(());
        }

        match token.to_ascii_lowercase().as_str() {
            "read" => {
                self.disallowed_exact.insert("read".to_owned());
            }
            "grep" | "rg" => {
                self.disallowed_exact.insert("rg".to_owned());
            }
            "glob" => {
                self.disallowed_exact.insert("glob".to_owned());
            }
            "edit" => {
                self.disallowed_exact.insert("edit".to_owned());
            }
            "write" => {
                self.disallowed_exact.insert("write".to_owned());
            }
            "bash" => {
                self.disallowed_exact.insert("bash".to_owned());
                self.disallowed_exact.insert("bash_task".to_owned());
            }
            "bashtask" | "bash_task" => {
                self.disallowed_exact.insert("bash_task".to_owned());
            }
            "eval" => {
                self.disallowed_exact.insert("eval".to_owned());
            }
            "websearch" | "web_search" => {
                self.disallowed_exact.insert("web_search".to_owned());
            }
            "lsp" => {
                self.disallowed_exact.insert("lsp".to_owned());
            }
            "todo" => {
                self.disallowed_exact.insert("todo".to_owned());
            }
            "wait" => {
                self.disallowed_exact.insert("wait".to_owned());
            }
            "task" => {
                self.disallowed_exact.insert("task".to_owned());
            }
            "none" => {
                bail!("`none` is not valid in --disallowed-tools; valid tools: {VALID_TOOL_NAMES}");
            }
            _ => {
                bail!(
                    "unknown tool `{token}` in --disallowed-tools; valid tools: {VALID_TOOL_NAMES}"
                )
            }
        }
        Ok(())
    }

    fn is_disallowed(&self, name: &str) -> bool {
        self.disallowed_all
            || self.disallowed_exact.contains(name)
            || self
                .disallowed_prefixes
                .iter()
                .any(|prefix| name.starts_with(prefix))
    }

    fn has_unbounded_bash(&self) -> bool {
        (self.all_standard || self.exact.contains("bash")) && !self.is_disallowed("bash")
    }

    /// 判断内部工具名是否被当前能力配置允许（黑名单优先于白名单）。
    pub fn allows(&self, name: &str) -> bool {
        if self.is_disallowed(name) {
            return false;
        }
        (self.all_standard && name != "task")
            || self.exact.contains(name)
            || (name == "bash" && !self.bash_allowed.is_empty())
            || self.prefixes.iter().any(|prefix| name.starts_with(prefix))
    }

    /// 调用期硬门禁：除工具级能力判定外，在 `Bash(...)` 规则生效时对 AST 叶子命令做强制检查。
    pub fn authorize_call(&self, name: &str, arguments: &str) -> Result<()> {
        ensure!(
            self.allows(name),
            "Tool is outside this agent capability set."
        );
        if name == "bash" {
            let allowed_patterns =
                (!self.has_unbounded_bash()).then_some(self.bash_allowed.as_slice());
            pp_permission::check_bash_command_rules(
                arguments,
                allowed_patterns,
                &self.bash_disallowed,
            )?;
        }
        Ok(())
    }

    /// 非交互下只有显式包含 `Task` 且未被 `--disallowed-tools Task` 禁用时才启用 subagent。
    pub fn enables_subagents(&self) -> bool {
        self.exact.contains("task") && !self.is_disallowed("task")
    }

    /// 未允许任何未被禁用的 MCP 工具时跳过启动期 MCP 建连与等待。
    pub fn allows_any_mcp(&self) -> bool {
        if self.disallowed_all
            || self
                .disallowed_prefixes
                .iter()
                .any(|prefix| prefix == "mcp__")
        {
            return false;
        }
        self.all_standard
            || self
                .exact
                .iter()
                .any(|name| name.starts_with("mcp__") && !self.is_disallowed(name))
            || self
                .prefixes
                .iter()
                .any(|prefix| prefix.starts_with("mcp__") && !self.is_disallowed(prefix))
    }

    /// 仅在允许 `lsp` 或存在文件修改工具（保存联动诊断）时初始化 LSP。
    pub fn allows_lsp_lifecycle(&self) -> bool {
        self.allows("lsp") || self.allows("edit") || self.allows("write")
    }

    /// 方案 B：将 `--allowed-tools`（扣减黑名单后）同步为本次运行的权限门预批准集合。
    pub fn apply_permission_grants(&self, gate: &pp_permission::Gate) {
        if !self.preapprove || self.disallowed_all {
            return;
        }
        let exact = self
            .exact
            .iter()
            .filter(|name| !self.is_disallowed(name))
            .cloned();
        let prefixes = self
            .prefixes
            .iter()
            .filter(|prefix| !self.is_disallowed(prefix))
            .cloned()
            .collect();
        let bash_patterns = if self.has_unbounded_bash() {
            Vec::new()
        } else {
            self.bash_allowed.clone()
        };
        gate.grant_allowed_tools(exact, prefixes, self.all_standard, bash_patterns);
    }
}

fn push_unique(list: &mut Vec<String>, item: String) {
    if !list.contains(&item) {
        list.push(item);
    }
}

fn split_tool_specs(raw: &str) -> Result<Vec<&str>> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, ch) in raw.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                ensure!(depth > 0, "unmatched `)` in tool specification `{raw}`");
                depth -= 1;
            }
            ',' if depth == 0 => {
                let token = raw[start..idx].trim();
                ensure!(
                    !token.is_empty(),
                    "empty entry in tool list; valid tools: {VALID_TOOL_NAMES}"
                );
                parts.push(token);
                start = idx + 1;
            }
            _ => {}
        }
    }
    ensure!(depth == 0, "unclosed `(` in tool specification `{raw}`");
    let tail = raw[start..].trim();
    ensure!(
        !tail.is_empty(),
        "empty entry in tool list; valid tools: {VALID_TOOL_NAMES}"
    );
    parts.push(tail);
    Ok(parts)
}

fn parse_bash_wrapper(token: &str) -> Result<Option<&str>> {
    let Some(open) = token.find('(') else {
        return Ok(None);
    };
    let head = token[..open].trim();
    ensure!(
        head.eq_ignore_ascii_case("bash") && token.ends_with(')'),
        "unsupported parameterized tool `{token}`; only Bash(...) is supported"
    );
    Ok(Some(&token[open + 1..token.len() - 1]))
}

/// 解析 `Bash(...)` 内部模式：`None` 表示无界 `Bash(*)`，`Some` 表示前缀或精确规则。
fn parse_bash_pattern(inner: &str) -> Result<Option<BashCommandPattern>> {
    let trimmed = inner.trim();
    ensure!(!trimmed.is_empty(), "empty Bash(...) pattern");
    if trimmed == "*" {
        return Ok(None);
    }
    if let Some(prefix_raw) = trimmed.strip_suffix(":*") {
        let tokens: Vec<String> = prefix_raw.split_whitespace().map(str::to_owned).collect();
        ensure!(
            !tokens.is_empty(),
            "empty command prefix in Bash({trimmed})"
        );
        return Ok(Some(BashCommandPattern::Prefix(tokens)));
    }
    if let Some(before_star) = trimmed.strip_suffix('*')
        && before_star.ends_with(char::is_whitespace)
    {
        let tokens: Vec<String> = before_star.split_whitespace().map(str::to_owned).collect();
        ensure!(
            !tokens.is_empty(),
            "empty command prefix in Bash({trimmed})"
        );
        return Ok(Some(BashCommandPattern::Prefix(tokens)));
    }
    let tokens: Vec<String> = trimmed.split_whitespace().map(str::to_owned).collect();
    ensure!(!tokens.is_empty(), "empty Bash(...) pattern");
    Ok(Some(BashCommandPattern::Exact(tokens)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aliases_wildcards_bash_rules_and_disallowed_overrides() {
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
            "Read, Grep, Edit, Bash(git status:*), mcp__github__*".to_owned(),
        ])
        .unwrap();
        assert!(parsed.allows("read"));
        assert!(parsed.allows("rg"));
        assert!(parsed.allows("edit"));
        assert!(parsed.allows("bash"));
        assert!(!parsed.allows("bash_task"));
        assert!(!parsed.allows("wait"));
        assert!(parsed.allows("mcp__github__search"));
        assert!(!parsed.allows("mcp__other__search"));
        parsed
            .authorize_call("bash", r#"{"command":"git status -s"}"#)
            .unwrap();
        assert!(
            parsed
                .authorize_call("bash", r#"{"command":"git status && pwd"}"#)
                .is_err()
        );

        let exact_bash = AllowedTools::parse_cli(&["Bash(git status)".to_owned()]).unwrap();
        exact_bash
            .authorize_call("bash", r#"{"command":"git status"}"#)
            .unwrap();
        assert!(
            exact_bash
                .authorize_call("bash", r#"{"command":"git status -s"}"#)
                .is_err()
        );

        let wildcard = AllowedTools::parse_cli(&["*".to_owned()]).unwrap();
        let restricted = AllowedTools::with_disallowed(
            Some(wildcard),
            &["Write, Bash(rm:*), mcp__danger__*".to_owned()],
        )
        .unwrap()
        .unwrap();
        assert!(restricted.allows("edit"));
        assert!(!restricted.allows("write"));
        assert!(restricted.allows("mcp__safe__read"));
        assert!(!restricted.allows("mcp__danger__drop"));
        restricted
            .authorize_call("bash", r#"{"command":"echo ok"}"#)
            .unwrap();
        assert!(
            restricted
                .authorize_call("bash", r#"{"command":"rm -f tmp"}"#)
                .is_err()
        );

        let disallowed_only = AllowedTools::with_disallowed(None, &["Task, Write".to_owned()])
            .unwrap()
            .unwrap();
        assert!(!disallowed_only.enables_subagents());
        assert!(!disallowed_only.allows("write"));
        assert!(disallowed_only.allows("read"));
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
            vec!["Bash()".to_owned()],
            vec!["Bash(:*)".to_owned()],
            vec!["Bash(echo".to_owned()],
            vec!["Read(foo)".to_owned()],
        ] {
            assert!(AllowedTools::parse_cli(&bad).is_err(), "{bad:?}");
        }
        assert!(AllowedTools::with_disallowed(None, &["none".to_owned()]).is_err());
    }
}
