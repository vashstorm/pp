//! CLI `--allowed-tools` / `--disallowed-tools` 预批准集合与 Bash AST 命令规则校验。

use crate::bash_ast::{self, LeafCommand};
use anyhow::{Result, bail};
use std::collections::HashSet;

/// Bash 命令级白名单/黑名单匹配规则。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BashCommandPattern {
    /// 命令前缀匹配，例如 `Bash(git status:*)` -> `["git", "status"]`。
    Prefix(Vec<String>),
    /// 精确命令词序列匹配，例如 `Bash(git status)` -> `["git", "status"]`。
    Exact(Vec<String>),
}

impl BashCommandPattern {
    pub(crate) fn matches_leaf(&self, leaf: &LeafCommand) -> bool {
        match self {
            Self::Prefix(prefix) => leaf.matches_prefix(prefix),
            Self::Exact(exact) => leaf.matches_exact(exact),
        }
    }
}

#[derive(Default)]
pub(crate) struct PreapprovedTools {
    pub(crate) exact: HashSet<String>,
    pub(crate) prefixes: Vec<String>,
    pub(crate) all_standard: bool,
    pub(crate) bash_patterns: Vec<BashCommandPattern>,
}

impl PreapprovedTools {
    pub(crate) fn contains(&self, name: &str) -> bool {
        (self.all_standard && name != "task")
            || self.exact.contains(name)
            || self.prefixes.iter().any(|prefix| name.starts_with(prefix))
    }

    pub(crate) fn matches_bash_leaf(&self, leaf: &LeafCommand) -> bool {
        self.bash_patterns
            .iter()
            .any(|pattern| pattern.matches_leaf(leaf))
    }
}

/// 基于 `brush-core` AST 检查整段 Bash 脚本的每个叶子命令是否满足 `--allowed-tools` / `--disallowed-tools` 规则。
pub fn check_bash_command_rules(
    arguments: &str,
    allowed: Option<&[BashCommandPattern]>,
    disallowed: &[BashCommandPattern],
) -> Result<()> {
    if allowed.is_none() && disallowed.is_empty() {
        return Ok(());
    }
    let Some(analysis) = bash_ast::analyze_bash_arguments(arguments) else {
        bail!("bash command cannot be statically verified against tool bash rules.");
    };
    if analysis.unanalyzable || analysis.leaves.is_empty() {
        bail!("bash command cannot be statically verified against tool bash rules.");
    }
    for leaf in &analysis.leaves {
        if leaf.dynamic_exec || leaf.is_function_call {
            bail!("dynamic bash command cannot be verified against tool bash rules.");
        }
        if disallowed.iter().any(|pattern| pattern.matches_leaf(leaf)) {
            bail!("bash command is blocked by --disallowed-tools rule.");
        }
        if let Some(allowed_patterns) = allowed
            && !allowed_patterns
                .iter()
                .any(|pattern| pattern.matches_leaf(leaf))
        {
            bail!("bash command is outside --allowed-tools bash rules.");
        }
    }
    Ok(())
}
