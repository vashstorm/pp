//! `--allowed-tools` / `--disallowed-tools` parsing, tool capability allowlist and permission-gate pre-approval sync.

use anyhow::{Result, bail, ensure};
use pp_permission::BashCommandPattern;
use std::collections::BTreeSet;

const VALID_TOOL_NAMES: &str = "Read, Grep (rg), Glob, Edit, Write, Bash, Bash(cmd:*), BashTask, Eval, WebSearch, Lsp, Todo, Wait, Task, mcp__*, mcp__<server>__*, *, none";

/// Tool capability allowlist and denylist; drives schema exposure, execution-time gating and CLI pre-approval.
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
    /// Pre-approvals are synced into the permission gate only when built from `--allowed-tools`
    /// (including the `-p` default read-only set); a `--disallowed-tools`-only restriction set must not grant them.
    preapprove: bool,
}

impl AllowedTools {
    /// Default read-only allowlist for non-interactive `-p` mode: `Read, Grep, Glob`.
    pub fn default_non_interactive() -> Self {
        Self {
            preapprove: true,
            ..Self::exact(["read".to_owned(), "rg".to_owned(), "glob".to_owned()])
        }
    }

    /// Build the allowlist from an exact set of internal tool names (reused by subagents and tests).
    pub fn exact(names: impl IntoIterator<Item = String>) -> Self {
        Self {
            exact: names.into_iter().collect(),
            ..Self::default()
        }
    }

    /// Intersect a subagent tool allowlist with the parent `--allowed-tools` / `--disallowed-tools`,
    /// inheriting the parent denylist and `Bash(...)` AST rules.
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

    /// Full restricted baseline used in interactive mode when only `--disallowed-tools` is given (no CLI pre-approval).
    fn all_with_task_no_preapprove() -> Self {
        Self {
            all_standard: true,
            exact: BTreeSet::from(["task".to_owned()]),
            preapprove: false,
            ..Self::default()
        }
    }

    /// Parse the CLI `--allowed-tools` argument list (comma separation, Bash rules in parentheses, repeated flags).
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

    /// Layer the `--disallowed-tools` denylist onto an optional allowlist (the denylist wins).
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

    /// Whether the current capability configuration allows an internal tool name (denylist wins).
    pub fn allows(&self, name: &str) -> bool {
        if matches!(name, "mcp_search" | "mcp_call") {
            return self.allows_any_mcp();
        }
        if self.is_disallowed(name) {
            return false;
        }
        (self.all_standard && name != "task")
            || self.exact.contains(name)
            || (name == "bash" && !self.bash_allowed.is_empty())
            || self.prefixes.iter().any(|prefix| name.starts_with(prefix))
    }

    /// Call-time hard gate: besides the tool-level capability check, enforce a check on AST leaf commands while `Bash(...)` rules are active.
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

    /// In non-interactive mode, enable subagents only when `Task` is explicit and not disabled by `--disallowed-tools Task`.
    pub fn enables_subagents(&self) -> bool {
        self.exact.contains("task") && !self.is_disallowed("task")
    }

    /// Skip startup MCP connection and waiting when no enabled MCP tool is allowed.
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

    /// Initialize LSP only when `lsp` or a file-modifying tool (save-linked diagnostics) is allowed.
    pub fn allows_lsp_lifecycle(&self) -> bool {
        self.allows("lsp") || self.allows("edit") || self.allows("write")
    }

    /// Option B: sync `--allowed-tools` (after denylist subtraction) into this run's permission-gate pre-approval set.
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

/// Parse the `Bash(...)` inner pattern: `None` is unbounded `Bash(*)`, `Some` is a prefix or exact rule.
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
