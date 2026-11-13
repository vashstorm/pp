//! 命令惯用法：分页、`test`、`set`、语言标注、副作用徽标。

use brush_core::parser::ast::{self, CommandPrefixOrSuffixItem as Item};
use std::collections::HashMap;

use super::super::words;
use super::{arguments, items};

pub(super) fn tail_count(simple: &ast::SimpleCommand) -> Option<(String, u32)> {
    if simple.prefix.is_some() {
        return None;
    }
    let args = arguments(simple)
        .iter()
        .map(|w| words::literal(w))
        .collect::<Option<Vec<_>>>()?;
    let name = args.first()?;
    if !matches!(name.as_str(), "head" | "tail") {
        return None;
    }
    let count = match args.as_slice() {
        [_] => 10,
        [_, count] if count.starts_with('-') && count[1..].chars().all(|c| c.is_ascii_digit()) => {
            count[1..].parse().ok()?
        }
        [_, flag, count]
            if (flag == "-n" || flag == "--lines")
                && !count.is_empty()
                && count.chars().all(|c| c.is_ascii_digit()) =>
        {
            count.parse().ok()?
        }
        [_, count]
            if count.starts_with("--lines=") && count[8..].chars().all(|c| c.is_ascii_digit()) =>
        {
            count[8..].parse().ok()?
        }
        _ => return None,
    };
    Some((name.clone(), count))
}

pub(super) fn test(values: &[String], constants: &HashMap<String, String>) -> String {
    let raw = values
        .iter()
        .map(|v| words::literal(v).unwrap_or_else(|| v.clone()))
        .collect::<Vec<_>>();
    let render = |i: usize| words::render(values[i].as_str(), true, constants);
    match raw.as_slice() {
        [op, _] => {
            let function = match op.as_str() {
                "-f" => Some("is_file"),
                "-d" => Some("is_dir"),
                "-e" => Some("exists"),
                "-s" => Some("has_content"),
                "-x" => Some("is_executable"),
                "-L" => Some("is_symlink"),
                _ => None,
            };
            if let Some(function) = function {
                format!("{function}({})", render(1))
            } else if op == "-n" || op == "-z" {
                format!(
                    "{} {} \"\"",
                    render(1),
                    if op == "-n" { "!=" } else { "==" }
                )
            } else if op == "!" {
                format!("not {}", render(1))
            } else {
                values.join(" ")
            }
        }
        [_, op, _] => format!(
            "{} {} {}",
            render(0),
            match op.as_str() {
                "=" | "==" | "-eq" => "==",
                "-ne" => "!=",
                "-lt" => "<",
                "-le" => "<=",
                "-gt" => ">",
                "-ge" => ">=",
                other => other,
            },
            render(2)
        ),
        _ => values.join(" "),
    }
}

pub(super) fn set_meaning(value: &str) -> Option<&'static str> {
    match value {
        "-e" => Some("stop on first error"),
        "+e" => Some("keep going on errors"),
        "-u" => Some("error on unset vars"),
        "-a" => Some("auto-export assignments"),
        "+a" => Some("stop auto-export"),
        "-m" => Some("job control on"),
        "+m" => Some("job control off"),
        "-x" => Some("trace commands"),
        "-euo" => Some("strict mode"),
        _ => None,
    }
}

pub(super) fn argument_language(name: &str, args: &[String], index: usize) -> Option<&'static str> {
    if index == 0 {
        return None;
    }
    let previous = args.get(index - 1).map(String::as_str).unwrap_or_default();
    if matches!(name, "bash" | "sh" | "zsh") && previous.starts_with('-') && previous.contains('c')
    {
        Some("bash")
    } else if matches!(name, "python" | "python3") && previous == "-c" {
        Some("python")
    } else if matches!(name, "node" | "bun")
        && matches!(previous, "-e" | "--eval" | "-p" | "--print")
    {
        Some("javascript")
    } else if matches!(name, "grep" | "rg" | "egrep") && regex_argument(args, index) {
        Some("regex")
    } else {
        None
    }
}

pub(super) fn effect(simple: &ast::SimpleCommand) -> String {
    let args = arguments(simple)
        .iter()
        .filter_map(|w| words::literal(w))
        .collect::<Vec<_>>();
    let name = args.first().map_or("", String::as_str);
    let has = |values: &[&str]| {
        args.iter()
            .skip(1)
            .any(|arg| values.contains(&arg.as_str()))
    };
    let mut out = String::new();
    if matches!(name, "curl" | "wget" | "gh" | "ssh" | "scp" | "rsync")
        || name == "git" && has(&["fetch", "push", "pull", "clone"])
    {
        out.push('⇅');
    }
    if matches!(name, "rm" | "kill" | "pkill")
        || name == "git"
            && (has(&["reset"]) && has(&["--hard"])
                || has(&["branch"]) && has(&["-D", "-d", "--delete"])
                || has(&["worktree"]) && has(&["remove"]))
    {
        out.push('×');
    }
    if matches!(
        name,
        "mkdir" | "touch" | "cp" | "mv" | "ln" | "tee" | "install"
    ) || name == "sed" && args.iter().any(|a| a.starts_with("-i"))
        || name == "git"
            && has(&[
                "add",
                "commit",
                "rebase",
                "checkout",
                "switch",
                "merge",
                "cherry-pick",
                "stash",
                "tag",
                "branch",
                "worktree",
            ])
    {
        out.push('✎');
    }
    if matches!(name, "bun" | "npm" | "pnpm" | "yarn") && has(&["install", "add", "i"]) {
        out.push_str("⇅✎");
    }
    if matches!(name, "sudo" | "su" | "runuser" | "chmod" | "chown") {
        out.push('⇪');
    }
    if name == "docker" && has(&["exec", "run", "compose", "start", "stop", "restart"]) {
        out.push('⇉');
    }
    if name == "security"
        && args
            .iter()
            .any(|arg| arg.starts_with("find-") && arg.ends_with("password"))
        || matches!(name, "source" | ".") && args.iter().any(|arg| arg.contains(".env"))
    {
        out.push('◆');
    }
    let writes = items(simple).any(|item| match item {
        Item::IoRedirect(ast::IoRedirect::File(
            _,
            ast::IoFileRedirectKind::Write
            | ast::IoFileRedirectKind::Append
            | ast::IoFileRedirectKind::Clobber
            | ast::IoFileRedirectKind::ReadAndWrite,
            target,
        )) => !target.to_string().starts_with("/dev/"),
        Item::IoRedirect(ast::IoRedirect::OutputAndError(target, _)) => {
            !target.value.contains("/dev/")
        }
        _ => false,
    });
    if writes && !out.contains('✎') {
        out.push('✎');
    }
    out
}

pub(super) fn pipe_stderr(command: &ast::Command) -> bool {
    if let ast::Command::Simple(simple) = command {
        items(simple).any(|item| {
            matches!(
                item,
                Item::IoRedirect(ast::IoRedirect::File(
                    Some(2),
                    ast::IoFileRedirectKind::DuplicateOutput,
                    ast::IoFileRedirectTarget::Fd(1)
                ))
            )
        })
    } else {
        false
    }
}

pub(super) fn timeout_duration(value: &str) -> bool {
    let value = value.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(value);
    !value.is_empty()
        && value.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
        && value
            .parse::<f64>()
            .is_ok_and(|value| value.is_finite() && value >= 0.0)
}

pub(super) fn lone_simple(list: &ast::CompoundList) -> Option<&ast::SimpleCommand> {
    if list.0.len() != 1 || !list.0[0].0.additional.is_empty() || list.0[0].0.first.seq.len() != 1 {
        return None;
    }
    match &list.0[0].0.first.seq[0] {
        ast::Command::Simple(simple) => Some(simple),
        _ => None,
    }
}

pub(super) fn function_parameters(source: &str) -> String {
    use brush_core::parser::word::{
        self, Parameter, ParameterExpr, WordPiece, WordPieceWithSource,
    };
    fn count(parts: &[WordPieceWithSource]) -> u32 {
        parts
            .iter()
            .map(|part| match &part.piece {
                WordPiece::ParameterExpansion(ParameterExpr::Parameter {
                    parameter: Parameter::Positional(index),
                    indirect: false,
                }) => *index,
                WordPiece::DoubleQuotedSequence(parts) => count(parts),
                _ => 0,
            })
            .max()
            .unwrap_or(0)
    }
    match word::parse(source, &Default::default()) {
        Ok(parts) => {
            let count = count(&parts);
            if count > 64 {
                return "*args".into();
            }
            (1..=count)
                .map(|index| format!("arg{index}"))
                .collect::<Vec<_>>()
                .join(", ")
        }
        Err(_) => "*args".into(),
    }
}

pub(super) fn regex_argument(args: &[String], index: usize) -> bool {
    let mut at = 1;
    let mut positional = false;
    let mut explicit = false;
    while at < args.len() {
        let arg = args[at].as_str();
        if arg == "--" {
            return !explicit && at + 1 == index;
        }
        if matches!(arg, "-e" | "--regexp") {
            if at + 1 == index {
                return true;
            }
            explicit = true;
            at += 2;
            continue;
        }
        if matches!(arg, "-f" | "--file") {
            explicit = true;
            at += 2;
            continue;
        }
        if matches!(
            arg,
            "-A" | "-B"
                | "-C"
                | "-m"
                | "-g"
                | "-t"
                | "-T"
                | "-M"
                | "-j"
                | "--after-context"
                | "--before-context"
                | "--context"
                | "--max-count"
                | "--glob"
                | "--iglob"
                | "--type"
                | "--type-not"
                | "--encoding"
                | "--max-columns"
                | "--threads"
                | "--color"
                | "--colors"
        ) {
            at += 2;
            continue;
        }
        if !arg.starts_with('-') {
            if !positional && !explicit && at == index {
                return true;
            }
            positional = true;
        }
        at += 1;
    }
    false
}
