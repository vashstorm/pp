//! 透明命令包装器剥离、提权前缀处理与多子命令 CLI 子命令提取。

use super::ArgToken;

/// 可安全剥离的透明命令包装器；`sudo`/`su`/`doas` 等提权命令绝不在此剥离。
const TRANSPARENT_WRAPPERS: &[&str] = &[
    "env", "command", "builtin", "nohup", "time", "nice", "timeout", "stdbuf",
];

/// 多子命令 CLI 工具：推导持久前缀时取 `[program, subcommand]`。
pub(super) const SUBCOMMAND_PROGRAMS: &[&str] = &[
    "git",
    "cargo",
    "npm",
    "pnpm",
    "yarn",
    "bun",
    "rustup",
    "docker",
    "kubectl",
    "gh",
    "uv",
    "pip",
    "pip3",
    "brew",
    "apt",
    "apt-get",
    "systemctl",
    "helm",
    "terraform",
];

pub(super) fn strip_privilege_prefix<'a>(
    tokens: &'a [ArgToken],
    raws: &'a [String],
) -> (&'a [ArgToken], &'a [String]) {
    let mut idx = 0;
    while let Some(ArgToken::Literal(cmd)) = tokens.get(idx) {
        let base = cmd.rsplit('/').next().unwrap_or(cmd);
        if base == "sudo" {
            idx += 1;
            while let Some(ArgToken::Literal(arg)) = tokens.get(idx) {
                if arg == "--" {
                    idx += 1;
                    break;
                }
                if !arg.starts_with('-') {
                    break;
                }
                idx += 1;
                if matches!(
                    arg.as_str(),
                    "-u" | "-g" | "-C" | "-h" | "-p" | "-r" | "-t" | "-U"
                ) {
                    idx += 1;
                }
            }
        } else if matches!(base, "su" | "doas" | "runuser") {
            idx += 1;
            while let Some(ArgToken::Literal(arg)) = tokens.get(idx) {
                if !arg.starts_with('-') {
                    break;
                }
                idx += 1;
            }
        } else {
            break;
        }
    }
    let rem_tokens = tokens.get(idx..).unwrap_or(&[]);
    let rem_raws = raws.get(idx..).unwrap_or(&[]);
    (rem_tokens, rem_raws)
}

pub(super) fn strip_transparent_wrappers(
    tokens: &[ArgToken],
    raws: &[String],
) -> (Vec<ArgToken>, Vec<String>, bool) {
    let mut idx = 0;
    let mut dynamic_exec = false;
    while idx < tokens.len() {
        let Some(ArgToken::Literal(cmd)) = tokens.get(idx) else {
            break;
        };
        let base = cmd.rsplit('/').next().unwrap_or(cmd);
        if !TRANSPARENT_WRAPPERS.contains(&base) {
            break;
        }
        let next = match base {
            "env" => {
                let mut j = idx + 1;
                while j < tokens.len() {
                    match &tokens[j] {
                        ArgToken::Literal(arg) => {
                            if arg == "-S"
                                || arg == "--split-string"
                                || arg.starts_with("--split-string=")
                            {
                                dynamic_exec = true;
                                break;
                            }
                            if arg == "--" {
                                j += 1;
                                break;
                            }
                            if matches!(arg.as_str(), "-u" | "--unset" | "-C" | "--chdir") {
                                j += 2;
                                continue;
                            }
                            if arg.starts_with('-') || looks_like_env_assign(arg) {
                                j += 1;
                                continue;
                            }
                            break;
                        }
                        ArgToken::Dynamic => {
                            dynamic_exec = true;
                            break;
                        }
                    }
                }
                j
            }
            "command" => {
                // `command -v` / `command -V` 是查询，不剥离为外层执行命令。
                if tokens.get(idx + 1..).is_some_and(|rest| {
                    rest.iter()
                        .any(|t| matches!(t, ArgToken::Literal(s) if s == "-v" || s == "-V"))
                }) {
                    break;
                }
                let mut j = idx + 1;
                while let Some(ArgToken::Literal(arg)) = tokens.get(j) {
                    if arg == "--" {
                        j += 1;
                        break;
                    }
                    if arg == "-p" {
                        j += 1;
                        continue;
                    }
                    break;
                }
                j
            }
            "builtin" | "nohup" => idx + 1,
            "time" => {
                let mut j = idx + 1;
                if matches!(tokens.get(j), Some(ArgToken::Literal(s)) if s == "-p") {
                    j += 1;
                }
                j
            }
            "nice" => {
                let mut j = idx + 1;
                while let Some(ArgToken::Literal(arg)) = tokens.get(j) {
                    if arg == "-n" {
                        j += 2;
                    } else if arg.starts_with("--adjustment=")
                        || (arg.starts_with('-')
                            && arg.len() > 1
                            && arg[1..].bytes().all(|b| b.is_ascii_digit()))
                    {
                        j += 1;
                    } else {
                        break;
                    }
                }
                j
            }
            "timeout" => {
                let mut j = idx + 1;
                while let Some(ArgToken::Literal(arg)) = tokens.get(j) {
                    if arg == "--" {
                        j += 1;
                        break;
                    }
                    if matches!(arg.as_str(), "-s" | "--signal" | "-k" | "--kill-after") {
                        j += 2;
                    } else if arg.starts_with('-') {
                        j += 1;
                    } else {
                        j += 1;
                        break;
                    }
                }
                j
            }
            "stdbuf" => {
                let mut j = idx + 1;
                while let Some(ArgToken::Literal(arg)) = tokens.get(j) {
                    if matches!(arg.as_str(), "-i" | "-o" | "-e") {
                        j += 2;
                    } else if arg.starts_with("--input=")
                        || arg.starts_with("--output=")
                        || arg.starts_with("--error=")
                    {
                        j += 1;
                    } else {
                        break;
                    }
                }
                j
            }
            _ => break,
        };
        if next >= tokens.len() || dynamic_exec {
            break;
        }
        idx = next;
    }
    (
        tokens[idx..].to_vec(),
        raws.get(idx..).unwrap_or(&[]).to_vec(),
        dynamic_exec,
    )
}

fn looks_like_env_assign(arg: &str) -> bool {
    let Some((name, _)) = arg.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub(super) fn xargs_command_start(args: &[ArgToken]) -> Option<usize> {
    let mut idx = 0;
    while idx < args.len() {
        match &args[idx] {
            ArgToken::Literal(arg) => {
                if arg == "--" {
                    return (idx + 1 < args.len()).then_some(idx + 1);
                }
                if matches!(
                    arg.as_str(),
                    "-a" | "-d"
                        | "-E"
                        | "-I"
                        | "-L"
                        | "-n"
                        | "-P"
                        | "-s"
                        | "--arg-file"
                        | "--delimiter"
                        | "--eof"
                        | "--replace"
                        | "--max-lines"
                        | "--max-args"
                        | "--max-procs"
                        | "--max-chars"
                ) {
                    idx += 2;
                } else if arg.starts_with('-') {
                    idx += 1;
                } else {
                    return Some(idx);
                }
            }
            ArgToken::Dynamic => return None,
        }
    }
    None
}

pub(super) fn tokens_start_with(tokens: &[ArgToken], prefix: &[String]) -> bool {
    if tokens.len() < prefix.len() {
        return false;
    }
    tokens
        .iter()
        .zip(prefix.iter())
        .all(|(token, expected)| match token {
            ArgToken::Literal(actual) => actual == expected,
            ArgToken::Dynamic => false,
        })
}

pub(super) fn extract_subcommand_for(program: &str, args: &[ArgToken]) -> Option<String> {
    extract_safe_subcommand_index(program, args).map(|(sub, _)| sub)
}

/// 提取子命令及其在 `args` 后的索引位置；会拒绝不安全的配置覆盖参数（如 `git -c`、`cargo --config`）。
pub(super) fn extract_safe_subcommand_index(
    program: &str,
    args: &[ArgToken],
) -> Option<(String, usize)> {
    match program {
        "git" => {
            let mut idx = 0;
            while idx < args.len() {
                match &args[idx] {
                    ArgToken::Literal(arg) => {
                        if arg == "-c"
                            || arg.starts_with("-c")
                            || arg == "--config-env"
                            || arg.starts_with("--config-env=")
                            || arg == "--exec-path"
                            || arg.starts_with("--exec-path=")
                        {
                            return None;
                        }
                        if arg == "--" {
                            idx += 1;
                            return match args.get(idx) {
                                Some(ArgToken::Literal(sub)) if !sub.starts_with('-') => {
                                    Some((sub.clone(), idx + 1))
                                }
                                _ => None,
                            };
                        }
                        if matches!(
                            arg.as_str(),
                            "-C" | "--git-dir" | "--work-tree" | "--namespace"
                        ) {
                            idx += 2;
                        } else if arg.starts_with('-') {
                            idx += 1;
                        } else {
                            return Some((arg.clone(), idx + 1));
                        }
                    }
                    ArgToken::Dynamic => return None,
                }
            }
            None
        }
        "cargo" => {
            let mut idx = 0;
            while idx < args.len() {
                match &args[idx] {
                    ArgToken::Literal(arg) => {
                        if arg == "--config" || arg.starts_with("--config=") || arg == "-Z" {
                            return None;
                        }
                        if arg.starts_with('+') {
                            idx += 1;
                        } else if matches!(arg.as_str(), "--manifest-path" | "-C") {
                            idx += 2;
                        } else if arg.starts_with('-') {
                            idx += 1;
                        } else {
                            return Some((arg.clone(), idx + 1));
                        }
                    }
                    ArgToken::Dynamic => return None,
                }
            }
            None
        }
        _ => extract_subcommand_index(args, &[]),
    }
}

pub(super) fn extract_subcommand_index(
    args: &[ArgToken],
    valued_flags: &[&str],
) -> Option<(String, usize)> {
    let mut idx = 0;
    while idx < args.len() {
        match &args[idx] {
            ArgToken::Literal(arg) => {
                if arg == "--" {
                    idx += 1;
                    return match args.get(idx) {
                        Some(ArgToken::Literal(sub)) if !sub.starts_with('-') => {
                            Some((sub.clone(), idx + 1))
                        }
                        _ => None,
                    };
                }
                if valued_flags.contains(&arg.as_str()) {
                    idx += 2;
                } else if arg.starts_with('-') {
                    idx += 1;
                } else {
                    return Some((arg.clone(), idx + 1));
                }
            }
            ArgToken::Dynamic => return None,
        }
    }
    None
}
