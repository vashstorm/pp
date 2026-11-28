//! Hazard classification of leaf commands, read-only safety checks and canonical prefix derivation.

use std::path::{Component, Path, PathBuf};

use brush_core::parser::word::{self, TildeExpr, WordPiece};

use super::wrappers::{
    SUBCOMMAND_PROGRAMS, extract_hazard_subcommand_for, extract_hazard_subcommand_index,
    extract_safe_subcommand_index, extract_subcommand_for, strip_privilege_prefix,
    tokens_start_with,
};
use super::{ArgToken, LeafCommand};
use crate::rules::{BashRule, MatchMode};

pub(super) const AUTH_FILES: &[&str] = &["/etc/passwd", "/etc/shadow", "/etc/sudoers"];

/// Builtin/common commands used only for reading or side-effect-free queries (never write tools such as `rm`/`chmod`/`mv`).
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
    pub(crate) fn matches_rule(&self, rule: &BashRule) -> bool {
        match rule.match_mode {
            MatchMode::Exact => self.matches_exact(&rule.pattern),
            MatchMode::Prefix => self.matches_prefix(&rule.pattern),
        }
    }

    pub(crate) fn matches_restrictive_rule(&self, rule: &BashRule) -> bool {
        match rule.match_mode {
            MatchMode::Exact => self.matches_restrictive_exact(&rule.pattern),
            MatchMode::Prefix => self.matches_restrictive_prefix(&rule.pattern),
        }
    }

    pub(crate) fn matches_prefix(&self, prefix: &[String]) -> bool {
        self.matches_prefix_with(prefix, false)
    }

    /// Used by `Deny`/`Prompt` and `--disallowed-tools`: skips value-carrying global options to find the real subcommand.
    pub(crate) fn matches_restrictive_prefix(&self, prefix: &[String]) -> bool {
        self.matches_prefix_with(prefix, true)
    }

    fn matches_prefix_with(&self, prefix: &[String], restrictive: bool) -> bool {
        if self.dynamic_exec || self.is_function_call || prefix.is_empty() {
            return false;
        }
        if tokens_start_with(&self.tokens, prefix) {
            return true;
        }
        if let Some(normalized) = self.normalized_subcommand_tokens(restrictive) {
            return tokens_start_with(&normalized, prefix);
        }
        false
    }

    pub(crate) fn matches_exact(&self, expected: &[String]) -> bool {
        self.matches_exact_with(expected, false)
    }

    pub(crate) fn matches_restrictive_exact(&self, expected: &[String]) -> bool {
        self.matches_exact_with(expected, true)
    }

    fn matches_exact_with(&self, expected: &[String], restrictive: bool) -> bool {
        if self.dynamic_exec || self.is_function_call || expected.is_empty() {
            return false;
        }
        let matches = |tokens: &[ArgToken]| -> bool {
            tokens.len() == expected.len()
                && tokens
                    .iter()
                    .zip(expected)
                    .all(|(token, literal)| matches!(token, ArgToken::Literal(value) if value == literal))
        };
        if matches(&self.tokens) {
            return true;
        }
        self.normalized_subcommand_tokens(restrictive)
            .is_some_and(|normalized| matches(&normalized))
    }

    fn normalized_subcommand_tokens(&self, restrictive: bool) -> Option<Vec<ArgToken>> {
        let program = self.program()?;
        if !SUBCOMMAND_PROGRAMS.contains(&program) {
            return None;
        }
        let (sub, rest_idx) = if restrictive {
            extract_hazard_subcommand_index(program, &self.tokens[1..])?
        } else {
            extract_safe_subcommand_index(program, &self.tokens[1..])?
        };
        let mut out = Vec::with_capacity(self.tokens.len());
        out.push(ArgToken::Literal(program.to_owned()));
        out.push(ArgToken::Literal(sub));
        if let Some(tail) = self.tokens.get(1 + rest_idx..) {
            out.extend_from_slice(tail);
        }
        Some(out)
    }

    /// Derives a command rule fine-grained down to concrete arguments/files (for `[[rules]]` `match = "exact"`).
    ///
    /// - Dynamic commands, function calls, shell interpreter scripts, `eval`/`source` and unsafe
    ///   configuration override arguments (`git -c`, `cargo --config`, `npm --registry`, ...) are never remembered; only one-off confirmation is allowed.
    /// - All safe arguments and sanitized file arguments are kept, so approving `git add aaa.txt` does not also
    ///   allow `git add bbb.txt` or `git add .`.
    pub(crate) fn suggested_exact_pattern(&self) -> Option<Vec<String>> {
        if self.dynamic_exec || self.is_function_call {
            return None;
        }
        let program = self.program()?;
        // Shell interpreters, code evaluators and scripting-language interpreters never get remembered rules,
        // otherwise remembering `python3 script.py` would let an unapproved write/edit rewrite the script and bypass approval.
        if matches!(
            program,
            "eval"
                | "source"
                | "."
                | "bash"
                | "sh"
                | "zsh"
                | "dash"
                | "ksh"
                | "python"
                | "python2"
                | "python3"
                | "pypy"
                | "pypy3"
                | "node"
                | "nodejs"
                | "ruby"
                | "perl"
                | "php"
                | "lua"
                | "luajit"
                | "osascript"
                | "pwsh"
                | "powershell"
                | "awk"
                | "gawk"
                | "nawk"
                | "mawk"
                | "xargs"
        ) {
            return None;
        }
        // Path-form executables (e.g. `./script.sh`, `scripts/run.sh`, `/tmp/tool`, `~/bin/run`)
        // live in potentially writable or dynamic locations, so they are never remembered automatically.
        if self
            .raw_args
            .first()
            .is_some_and(|raw| raw.contains('/') || raw.starts_with('~'))
        {
            return None;
        }
        if matches!(program, "bun" | "deno") {
            let sub = extract_hazard_subcommand_for(program, &self.tokens[1..]);
            let has_eval_flag = self.tokens[1..].iter().any(|tok| {
                matches!(
                    tok.as_literal(),
                    Some("-e" | "--eval" | "-p" | "--print" | "eval")
                )
            });
            if has_eval_flag
                || matches!(sub.as_deref(), Some("run" | "eval" | "x"))
                || sub.as_deref().is_some_and(|s| {
                    s.contains('/')
                        || s.ends_with(".js")
                        || s.ends_with(".ts")
                        || s.ends_with(".mjs")
                        || s.ends_with(".cjs")
                        || s.ends_with(".mts")
                        || s.ends_with(".cts")
                })
            {
                return None;
            }
        }
        if program == "uv"
            && matches!(
                extract_hazard_subcommand_for("uv", &self.tokens[1..]).as_deref(),
                Some("run" | "tool")
            )
        {
            return None;
        }
        if program == "find"
            && self.tokens[1..].iter().any(|tok| {
                matches!(
                    tok.as_literal(),
                    Some("-exec" | "-execdir" | "-ok" | "-okdir" | "-delete")
                )
            })
        {
            return None;
        }
        if SUBCOMMAND_PROGRAMS.contains(&program) {
            extract_safe_subcommand_index(program, &self.tokens[1..])?;
        }
        let mut out = Vec::with_capacity(self.tokens.len());
        for (idx, token) in self.tokens.iter().enumerate() {
            let ArgToken::Literal(value) = token else {
                return None;
            };
            if value.is_empty() || value.chars().any(char::is_control) {
                return None;
            }
            if idx == 0 {
                out.push(value.clone());
            } else {
                out.push(normalize_arg_token(value));
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// Statically detects whether the command needs to write inside protected directories of the workspace (`.git`, `.agents`, `.pp`).
    pub(crate) fn protected_write_targets(&self, cwd: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut push_unique = |path: PathBuf| {
            if !out.contains(&path) {
                out.push(path);
            }
        };
        if let Some(program) = self.program()
            && program == "git"
            && !self.is_safe_read_only()
        {
            let sub = extract_hazard_subcommand_for("git", &self.tokens[1..]);
            // Pure remote queries (e.g. `git ls-remote`) do not write `.git`; every other non-read-only git command writes into the `.git` directory by default.
            if sub.as_deref() != Some("ls-remote") {
                push_unique(cwd.join(".git"));
            }
        }
        // For commands that are not purely read-only (or that carry a write redirect), inspect literal arguments for paths into .git / .agents / .pp.
        if !self.is_safe_read_only() {
            for token in self.tokens.iter().skip(1) {
                if let ArgToken::Literal(arg) = token
                    && !arg.starts_with('-')
                    && let Some(prot) = match_protected_path(arg, cwd)
                {
                    push_unique(prot);
                }
            }
        }
        out
    }

    /// Statically detects commands that clearly need outbound internet access.
    pub(crate) fn needs_network(&self) -> bool {
        let (tokens, _) = strip_privilege_prefix(&self.tokens, &self.raw_args);
        let Some(ArgToken::Literal(raw_program)) = tokens.first() else {
            return false;
        };
        let program = raw_program.rsplit('/').next().unwrap_or(raw_program);
        let args = &tokens[1..];
        let has_offline = args
            .iter()
            .filter_map(ArgToken::as_literal)
            .any(|arg| arg == "--offline" || arg == "-o");
        match program {
            "curl" | "wget" | "ssh" | "scp" | "sftp" | "nc" | "ncat" | "telnet" | "ftp"
            | "ping" | "dig" | "nslookup" | "host" | "gh" | "aws" | "gcloud" | "az" => true,
            "rsync" => args
                .iter()
                .filter_map(ArgToken::as_literal)
                .any(|arg| arg.contains(':')),
            "git" => matches!(
                extract_hazard_subcommand_for("git", args).as_deref(),
                Some("push" | "pull" | "fetch" | "clone" | "ls-remote" | "submodule")
            ),
            "cargo" => {
                !has_offline
                    && matches!(
                        extract_hazard_subcommand_for("cargo", args).as_deref(),
                        Some("publish" | "install" | "update" | "search" | "fetch" | "login")
                    )
            }
            "npm" | "pnpm" | "yarn" | "bun" => {
                !has_offline
                    && matches!(
                        extract_hazard_subcommand_for(program, args).as_deref(),
                        Some(
                            "install" | "i" | "add" | "update" | "up" | "publish" | "dlx" | "login"
                        )
                    )
            }
            "pip" | "pip3" | "uv" => {
                !has_offline
                    && matches!(
                        extract_hazard_subcommand_for(program, args).as_deref(),
                        Some("install" | "download" | "publish" | "sync" | "pip")
                    )
            }
            "brew" => matches!(
                extract_hazard_subcommand_for("brew", args).as_deref(),
                Some("install" | "upgrade" | "update" | "reinstall" | "fetch")
            ),
            _ => false,
        }
    }

    /// Whether this is a high-impact command (confirm by default outside YOLO).
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
        match program {
            "git" => !self.is_safe_read_only(),
            "cargo" | "npm" | "pnpm" | "yarn" | "bun" => {
                extract_hazard_subcommand_for(program, &self.tokens[1..]).as_deref()
                    == Some("publish")
            }
            _ => false,
        }
    }

    /// Whether this is a statically provable read-only command (used under `AlwaysAsk` to skip the prompt and reduce friction).
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
        if program == "git" {
            return git_is_read_only(&self.tokens[1..]);
        }
        let has_dynamic_arg = self
            .tokens
            .iter()
            .skip(1)
            .any(|tok| matches!(tok, ArgToken::Dynamic));
        let literals: Vec<&str> = self
            .tokens
            .iter()
            .skip(1)
            .filter_map(ArgToken::as_literal)
            .collect();
        let has_short_flag =
            |s: &str, ch: char| !s.starts_with("--") && s.starts_with('-') && s[1..].contains(ch);
        match program {
            "sort" => {
                !has_dynamic_arg
                    && !literals.iter().any(|s| {
                        *s == "-o"
                            || has_short_flag(s, 'o')
                            || *s == "--output"
                            || s.starts_with("--output=")
                    })
            }
            "yq" => {
                !has_dynamic_arg
                    && !literals
                        .iter()
                        .any(|s| *s == "-i" || has_short_flag(s, 'i') || *s == "--inplace")
            }
            "find" => {
                !has_dynamic_arg
                    && !literals.iter().any(|s| {
                        matches!(
                            *s,
                            "-exec"
                                | "-execdir"
                                | "-ok"
                                | "-okdir"
                                | "-delete"
                                | "-fprint"
                                | "-fprint0"
                                | "-fprintf"
                                | "-fls"
                        )
                    })
            }
            "cargo" => {
                matches!(
                    extract_subcommand_for("cargo", &self.tokens[1..]).as_deref(),
                    Some("version" | "metadata" | "tree" | "pkgid" | "locate-project")
                ) || (!has_dynamic_arg
                    && self.tokens.len() == 2
                    && matches!(literals.as_slice(), ["--version" | "-V"]))
            }
            "rustc" => {
                !has_dynamic_arg
                    && self.tokens.len() == 2
                    && matches!(literals.as_slice(), ["--version" | "-V"])
            }
            _ => false,
        }
    }
}

/// Git may only skip approval for clearly recognized queries; configuration overrides, dynamic arguments and unknown subcommands all need confirmation.
fn git_is_read_only(args: &[ArgToken]) -> bool {
    if args.is_empty()
        || matches!(args, [ArgToken::Literal(flag)] if matches!(flag.as_str(), "--version" | "-v" | "--help" | "-h"))
    {
        return true;
    }
    let Some((subcommand, args_start)) = extract_safe_subcommand_index("git", args) else {
        return false;
    };
    let Some(args) = args
        .iter()
        .map(ArgToken::as_literal)
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    let args = &args[args_start..];
    match subcommand.as_str() {
        "branch" => {
            let listing_flags = args
                .iter()
                .copied()
                .take_while(|flag| {
                    matches!(
                        *flag,
                        "-a" | "--all" | "-r" | "--remotes" | "-v" | "-vv" | "--verbose"
                    )
                })
                .count();
            let args = &args[listing_flags..];
            // `branch -l <name>` on older Git versions creates the branch and a reflog.
            matches!(args, [] | ["--show-current"])
                || matches!(args, ["--list", patterns @ ..] if git_positional_args(patterns))
        }
        "tag" => {
            args.is_empty()
                || matches!(args, ["-l" | "--list" | "-v" | "--verify", names @ ..] if git_positional_args(names))
        }
        "remote" => {
            matches!(args, [] | ["-v" | "--verbose" | "show"])
                || matches!(args, ["get-url", name] if !name.starts_with('-'))
        }
        "config" => {
            let scope_flags = args
                .iter()
                .copied()
                .take_while(|flag| {
                    matches!(*flag, "--local" | "--global" | "--system" | "--worktree")
                })
                .count();
            matches!(
                &args[scope_flags..],
                ["get" | "--get" | "--get-all" | "--get-regexp" | "list" | "-l" | "--list", operands @ ..]
                    if git_positional_args(operands)
            )
        }
        subcommand if GIT_READ_ONLY_SUBCOMMANDS.contains(&subcommand) => {
            // diff and some history queries can write their result to a given file, including recognizable option abbreviations.
            !args.iter().take_while(|arg| **arg != "--").any(|arg| {
                let flag = arg.split('=').next().unwrap_or(arg);
                flag.starts_with("--") && "--output".starts_with(flag)
            })
        }
        _ => false,
    }
}

fn git_positional_args(args: &[&str]) -> bool {
    args.iter().all(|arg| !arg.starts_with('-'))
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

fn normalize_arg_token(raw: &str) -> String {
    if raw.starts_with("./") && raw.len() > 2 && !raw.contains("..") {
        let stripped = raw.trim_start_matches("./");
        if !stripped.is_empty() && !stripped.starts_with('-') {
            return stripped.to_owned();
        }
    }
    raw.to_owned()
}

pub(super) fn match_protected_path(raw: &str, cwd: &Path) -> Option<PathBuf> {
    let candidate = Path::new(raw);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    };
    let mut cleaned = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                cleaned.pop();
            }
            _ => cleaned.push(comp.as_os_str()),
        }
    }
    let rel = cleaned.strip_prefix(cwd).ok()?;
    let mut comps = rel.components();
    let Some(Component::Normal(first)) = comps.next() else {
        return None;
    };
    if matches!(first.to_str(), Some(".git" | ".agents" | ".pp")) {
        return Some(cwd.join(first));
    }
    None
}
