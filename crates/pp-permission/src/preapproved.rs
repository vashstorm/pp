//! CLI `--allowed-tools` / `--disallowed-tools` pre-approval sets and Bash AST command rule validation.

use crate::bash_ast::{self, LeafCommand};
use anyhow::{Result, bail};
use std::collections::HashSet;

/// Bash command-level allowlist/denylist matching pattern.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BashCommandPattern {
    /// Command prefix match, e.g. `Bash(git status:*)` -> `["git", "status"]`.
    Prefix(Vec<String>),
    /// Exact command word sequence match, e.g. `Bash(git status)` -> `["git", "status"]`.
    Exact(Vec<String>),
}

impl BashCommandPattern {
    pub(crate) fn matches_leaf(&self, leaf: &LeafCommand) -> bool {
        match self {
            Self::Prefix(prefix) => leaf.matches_prefix(prefix),
            Self::Exact(exact) => leaf.matches_exact(exact),
        }
    }

    pub(crate) fn matches_leaf_restrictive(&self, leaf: &LeafCommand) -> bool {
        match self {
            Self::Prefix(prefix) => leaf.matches_restrictive_prefix(prefix),
            Self::Exact(exact) => leaf.matches_restrictive_exact(exact),
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

/// Uses the `brush-core` AST to check whether every leaf command in a whole Bash script satisfies the `--allowed-tools` / `--disallowed-tools` rules.
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
        if disallowed
            .iter()
            .any(|pattern| pattern.matches_leaf_restrictive(leaf))
        {
            bail!("bash command is blocked by --disallowed-tools rule.");
        }
        if let Some(allowed_patterns) = allowed
            && (analysis.has_write_redirection
                || !allowed_patterns
                    .iter()
                    .any(|pattern| pattern.matches_leaf(leaf)))
        {
            bail!("bash command is outside --allowed-tools bash rules.");
        }
    }
    Ok(())
}
