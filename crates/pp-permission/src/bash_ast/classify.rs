//! 叶子命令的危险度分级、只读安全判定与规范前缀推导。

use brush_core::parser::word::{self, TildeExpr, WordPiece};

use super::wrappers::{
    SUBCOMMAND_PROGRAMS, extract_safe_subcommand_index, extract_subcommand_for,
    extract_subcommand_index, strip_privilege_prefix, tokens_start_with,
};
use super::{ArgToken, LeafCommand};

pub(super) const AUTH_FILES: &[&str] = &["/etc/passwd", "/etc/shadow", "/etc/sudoers"];

/// 仅用于读取或无副作用查询的内置/常见命令（绝不包含 `rm`/`chmod`/`mv` 等写入工具）。
const SAFE_READ_ONLY_COMMANDS: &[&str] = &[
    "pwd", "ls", "ll", "la", "dir", "tree", "stat", "file", "wc", "du", "df", "uname", "whoami",
    "id", "hostname", "date", "which", "whereis", "type", "printenv", "readlink", "realpath",
    "basename", "dirname", "test", "[", "cd", "cat", "head", "tail", "nl", "tac", "rev", "uniq",
    "cut", "tr", "fold", "column", "fmt", "diff", "cmp", "comm", "jq", "rg", "grep", "egrep",
    "fgrep", "ag", "ack",
];

const GIT_READ_ONLY_SUBCOMMANDS: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
    "blame",
    "annotate",
    "shortlog",
    "describe",
    "rev-parse",
    "rev-list",
    "ls-files",
    "ls-tree",
    "cat-file",
    "merge-base",
    "grep",
    "whatchanged",
];

impl LeafCommand {
    pub(crate) fn matches_prefix(&self, prefix: &[String]) -> bool {
        if self.dynamic_exec || self.is_function_call || prefix.is_empty() {
            return false;
        }
        if tokens_start_with(&self.tokens, prefix) {
            return true;
        }
        if let Some(normalized) = self.normalized_subcommand_tokens() {
            return tokens_start_with(&normalized, prefix);
        }
        false
    }

    fn normalized_subcommand_tokens(&self) -> Option<Vec<ArgToken>> {
        let program = self.program()?;
        if !SUBCOMMAND_PROGRAMS.contains(&program) {
            return None;
        }
        let (sub, rest_idx) = extract_safe_subcommand_index(program, &self.tokens[1..])?;
        let mut out = Vec::with_capacity(self.tokens.len());
        out.push(ArgToken::Literal(program.to_owned()));
        out.push(ArgToken::Literal(sub));
        if let Some(tail) = self.tokens.get(1 + rest_idx..) {
            out.extend_from_slice(tail);
        }
        Some(out)
    }

    /// 推导适合持久化到 `[[bash]]` 的命令前缀。
    pub(crate) fn suggested_prefix(&self) -> Option<Vec<String>> {
        if self.dynamic_exec || self.is_function_call {
            return None;
        }
        let program = self.program()?;
        if matches!(program, "eval" | "source" | ".") {
            return None;
        }
        if matches!(program, "bash" | "sh" | "zsh" | "dash" | "ksh") {
            return match self.tokens.get(1) {
                Some(ArgToken::Literal(script)) if !script.starts_with('-') => {
                    Some(vec![program.to_owned(), script.clone()])
                }
                _ => None,
            };
        }
        if matches!(program, "sudo" | "su" | "doas" | "runuser") {
            let (inner, _) = extract_subcommand_index(&self.tokens[1..], &[])?;
            let inner_base = inner.rsplit('/').next().unwrap_or(&inner).to_owned();
            return Some(vec![program.to_owned(), inner_base]);
        }
        if SUBCOMMAND_PROGRAMS.contains(&program) {
            let (sub, rest_idx) = extract_safe_subcommand_index(program, &self.tokens[1..])?;
            if program == "cargo"
                && sub == "nextest"
                && let Some((next, _)) =
                    extract_subcommand_index(self.tokens.get(1 + rest_idx..).unwrap_or(&[]), &[])
                && matches!(next.as_str(), "run" | "list")
            {
                return Some(vec![program.to_owned(), sub, next]);
            }
            return Some(vec![program.to_owned(), sub]);
        }
        Some(vec![program.to_owned()])
    }

    /// 是否高影响命令（`AcceptEdits` 下默认需确认，但允许按前缀记住）。
    pub(crate) fn is_high_impact(&self) -> bool {
        if self.dynamic_exec {
            return false;
        }
        let Some(program) = self.program() else {
            return false;
        };
        if matches!(program, "sudo" | "su" | "doas" | "runuser") {
            return true;
        }
        let literals: Vec<&str> = self
            .tokens
            .iter()
            .skip(1)
            .filter_map(ArgToken::as_literal)
            .collect();
        match program {
            "git" => match extract_subcommand_for("git", &self.tokens[1..]).as_deref() {
                Some("push") => true,
                Some("reset") => literals.iter().any(|a| matches!(*a, "--hard" | "--merge")),
                Some("clean") => literals.iter().any(|a| {
                    *a == "--force"
                        || (a.starts_with('-') && !a.starts_with("--") && a[1..].contains('f'))
                }),
                Some("branch") => literals.contains(&"-D"),
                Some("worktree") => literals.contains(&"remove"),
                _ => false,
            },
            "cargo" | "npm" | "pnpm" | "yarn" | "bun" => {
                extract_subcommand_for(program, &self.tokens[1..]).as_deref() == Some("publish")
            }
            _ => false,
        }
    }

    /// 是否静态可证明的纯只读命令（用于 `AlwaysAsk` 下减摩擦免弹窗）。
    pub(crate) fn is_safe_read_only(&self) -> bool {
        if self.dynamic_exec || self.is_function_call || self.has_write_redirect {
            return false;
        }
        let Some(program) = self.program() else {
            return false;
        };
        if SAFE_READ_ONLY_COMMANDS.contains(&program) {
            return true;
        }
        let literals: Vec<&str> = self
            .tokens
            .iter()
            .skip(1)
            .filter_map(ArgToken::as_literal)
            .collect();
        match program {
            "sort" => !self.tokens.iter().skip(1).any(|tok| {
                matches!(tok, ArgToken::Literal(s) if s == "-o" || s.starts_with("-o") || s == "--output" || s.starts_with("--output="))
            }),
            "sed" => !self.tokens.iter().skip(1).any(|tok| {
                matches!(tok, ArgToken::Literal(s) if s == "-i" || s.starts_with("-i") || s == "--in-place" || s.starts_with("--in-place="))
            }),
            "yq" => !self.tokens.iter().skip(1).any(|tok| {
                matches!(tok, ArgToken::Literal(s) if s == "-i" || s == "--inplace")
            }),
            "find" => !self.tokens.iter().skip(1).any(|tok| {
                matches!(tok, ArgToken::Literal(s) if matches!(
                    s.as_str(),
                    "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete" | "-fprint" | "-fprint0" | "-fprintf" | "-fls"
                ))
            }),
            "git" => match extract_subcommand_for("git", &self.tokens[1..]).as_deref() {
                Some(sub) if GIT_READ_ONLY_SUBCOMMANDS.contains(&sub) => true,
                Some("branch") => {
                    let has_mutating_flag = literals.iter().any(|a| {
                        matches!(
                            *a,
                            "-d" | "-D" | "-m" | "-M" | "-c" | "-C" | "--delete" | "--move" | "--copy" | "--edit-description" | "--set-upstream-to" | "--unset-upstream"
                        )
                    });
                    !has_mutating_flag
                        && (literals.iter().any(|a| {
                            matches!(*a, "--list" | "-l" | "-a" | "--all" | "-r" | "--remotes" | "--show-current")
                        }) || literals.iter().filter(|a| **a != "branch").all(|a| a.starts_with('-')))
                }
                Some("tag") => {
                    literals.iter().any(|a| matches!(*a, "-l" | "--list" | "-v" | "--verify"))
                        || literals.iter().filter(|a| **a != "tag").all(|a| a.starts_with('-'))
                }
                Some("remote") => literals
                    .iter()
                    .filter(|a| **a != "remote")
                    .all(|a| matches!(*a, "-v" | "--verbose" | "show" | "get-url") || a.starts_with('-')),
                Some("config") => literals
                    .iter()
                    .any(|a| matches!(*a, "--get" | "--get-all" | "--list" | "-l")),
                _ => false,
            },
            "cargo" => matches!(
                extract_subcommand_for("cargo", &self.tokens[1..]).as_deref(),
                Some("version" | "metadata" | "tree" | "pkgid" | "locate-project")
            ) || literals.iter().any(|a| matches!(*a, "--version" | "-V")),
            "rustc" => literals.iter().any(|a| matches!(*a, "--version" | "-V")),
            _ => false,
        }
    }
}

pub(super) fn leaf_critical_reason(leaf: &LeafCommand) -> Option<&'static str> {
    let (effective_tokens, effective_raws) = strip_privilege_prefix(&leaf.tokens, &leaf.raw_args);
    let ArgToken::Literal(raw_program) = effective_tokens.first()? else {
        return None;
    };
    let program = raw_program.rsplit('/').next().unwrap_or(raw_program);
    let args = &effective_tokens[1..];
    let raw_args = if effective_raws.len() > 1 {
        &effective_raws[1..]
    } else {
        &[]
    };

    if matches!(program, "shutdown" | "reboot" | "halt" | "poweroff") {
        return Some("system shutdown, reboot, halt or poweroff");
    }
    if program.starts_with("mkfs")
        || (program == "dd"
            && args
                .iter()
                .any(|tok| matches!(tok, ArgToken::Literal(s) if s.starts_with("of=/dev/"))))
    {
        return Some("disk destruction via mkfs or raw-device dd");
    }
    if program == "rm" {
        let mut end_of_flags = false;
        let mut recursive = false;
        let mut targets_root_or_home = false;
        for (idx, tok) in args.iter().enumerate() {
            if let ArgToken::Literal(s) = tok {
                if !end_of_flags && s == "--" {
                    end_of_flags = true;
                    continue;
                }
                if !end_of_flags && s.starts_with('-') && s.len() > 1 {
                    if s == "--recursive" || (!s.starts_with("--") && s[1..].contains(['r', 'R'])) {
                        recursive = true;
                    }
                    continue;
                }
                if s.starts_with('/') {
                    targets_root_or_home = true;
                }
            }
            if let Some(raw) = raw_args.get(idx)
                && is_tilde_path(raw)
            {
                targets_root_or_home = true;
            }
        }
        if recursive && targets_root_or_home {
            return Some("recursive force delete targeting a root path");
        }
    }
    if matches!(program, "tee" | "cp" | "mv" | "install" | "ln")
        && args
            .iter()
            .any(|tok| matches!(tok, ArgToken::Literal(s) if is_auth_path(s)))
    {
        return Some("write to system authentication files");
    }
    None
}

fn is_tilde_path(raw: &str) -> bool {
    let Ok(pieces) = word::parse(raw, &Default::default()) else {
        return raw.starts_with('~');
    };
    matches!(
        pieces.first().map(|p| &p.piece),
        Some(WordPiece::TildeExpansion(
            TildeExpr::Home | TildeExpr::UserHome(_)
        ))
    )
}

pub(super) fn is_auth_path(path: &str) -> bool {
    AUTH_FILES.contains(&path)
}

pub(super) fn is_fork_bomb(command: &str) -> bool {
    let compact: String = command.chars().filter(|c| !c.is_whitespace()).collect();
    compact.contains(":(){:|:&};:")
}

pub(super) fn fallback_critical(command: &str) -> Option<&'static str> {
    if is_fork_bomb(command) {
        return Some("fork bomb pattern");
    }
    let tokens: Vec<&str> = command.split_whitespace().collect();
    if tokens.iter().any(|t| {
        matches!(
            t.rsplit('/').next().unwrap_or(t),
            "shutdown" | "reboot" | "halt" | "poweroff"
        )
    }) {
        return Some("system shutdown, reboot, halt or poweroff");
    }
    if tokens.iter().any(|t| t.starts_with("mkfs"))
        || (tokens
            .iter()
            .any(|t| t.rsplit('/').next().unwrap_or(t) == "dd")
            && tokens.iter().any(|t| t.starts_with("of=/dev/")))
    {
        return Some("disk destruction via mkfs or raw-device dd");
    }
    if AUTH_FILES.iter().any(|p| command.contains(p))
        && (command.contains('>') || tokens.contains(&"tee"))
    {
        return Some("write to system authentication files");
    }
    None
}

pub(super) fn is_shell_interpreter_leaf(leaf: &LeafCommand) -> bool {
    let (tokens, _) = strip_privilege_prefix(&leaf.tokens, &leaf.raw_args);
    matches!(
        tokens.first(),
        Some(ArgToken::Literal(name))
            if matches!(
                name.rsplit('/').next().unwrap_or(name),
                "sh" | "bash" | "zsh" | "dash" | "ksh"
            )
    )
}

pub(super) fn is_remote_fetch_leaf(leaf: &LeafCommand) -> bool {
    let (tokens, _) = strip_privilege_prefix(&leaf.tokens, &leaf.raw_args);
    matches!(
        tokens.first(),
        Some(ArgToken::Literal(name))
            if matches!(name.rsplit('/').next().unwrap_or(name), "curl" | "wget")
    )
}
