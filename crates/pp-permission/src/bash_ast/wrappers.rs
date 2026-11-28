//! Transparent command wrapper stripping, privilege prefix handling and subcommand extraction for multi-subcommand CLIs.

use super::ArgToken;

/// Transparent command wrappers that can be stripped safely; privilege commands such as `sudo`/`su`/`doas` are never stripped here.
const TRANSPARENT_WRAPPERS: &[&str] = &[
    "env", "command", "builtin", "nohup", "time", "nice", "timeout", "stdbuf",
];

/// Multi-subcommand CLI tools: persistent prefix derivation takes `[program, subcommand]`.
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
                if !arg.starts_with('-') && !looks_like_env_assign(arg) {
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
        } else if base == "doas" {
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
                if matches!(arg.as_str(), "-u" | "-C") {
                    idx += 1;
                }
            }
        } else if matches!(base, "su" | "runuser") {
            let start_idx = idx;
            idx += 1;
            let mut saw_command_script = false;
            while let Some(ArgToken::Literal(arg)) = tokens.get(idx) {
                if arg == "--" {
                    idx += 1;
                    break;
                }
                if arg == "-c" || arg == "--command" || arg == "--session-command" {
                    saw_command_script = true;
                    break;
                }
                if arg.starts_with("--command=") || arg.starts_with("--session-command=") {
                    saw_command_script = true;
                    break;
                }
                if !arg.starts_with('-') {
                    break;
                }
                idx += 1;
                if matches!(arg.as_str(), "-u" | "-g" | "-G" | "-s") {
                    idx += 1;
                }
            }
            if saw_command_script {
                // Keep `su`/`runuser` and `-c` for nested parsing instead of stripping them into a prefix-less script.
                idx = start_idx;
                break;
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
                // Only inspect options right after `command`, up to `--` or the first operand (combined short options like `-pv` included),
                // so the target command's own `-v` argument (e.g. `command rm -rf / -v`) is not mistaken for a query.
                let mut j = idx + 1;
                let mut is_query = false;
                while j < tokens.len() {
                    match &tokens[j] {
                        ArgToken::Literal(arg) => {
                            if arg == "--" {
                                j += 1;
                                break;
                            }
                            if !arg.starts_with('-') || arg == "-" {
                                break;
                            }
                            if !arg.starts_with("--") && arg[1..].contains(['v', 'V']) {
                                is_query = true;
                                break;
                            }
                            if arg == "-p" {
                                j += 1;
                                continue;
                            }
                            dynamic_exec = true;
                            break;
                        }
                        ArgToken::Dynamic => {
                            dynamic_exec = true;
                            break;
                        }
                    }
                }
                if is_query {
                    break;
                }
                j
            }
            "builtin" | "nohup" => {
                let mut j = idx + 1;
                if matches!(tokens.get(j), Some(ArgToken::Literal(s)) if s == "--") {
                    j += 1;
                }
                j
            }
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
                        // After `--` ends the options, the next argument is still the duration; skip it together and then take the real command.
                        j = (j + 2).min(tokens.len());
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

pub(super) enum XargsTarget {
    None,
    Command(usize),
    Dynamic,
}

pub(super) fn xargs_command_target(args: &[ArgToken]) -> XargsTarget {
    let mut idx = 0;
    while idx < args.len() {
        match &args[idx] {
            ArgToken::Literal(arg) => {
                if arg == "--" {
                    return match args.get(idx + 1) {
                        Some(ArgToken::Literal(_)) => XargsTarget::Command(idx + 1),
                        Some(ArgToken::Dynamic) => XargsTarget::Dynamic,
                        None => XargsTarget::None,
                    };
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
                    return XargsTarget::Command(idx);
                }
            }
            ArgToken::Dynamic => return XargsTarget::Dynamic,
        }
    }
    XargsTarget::None
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

/// git value-carrying global options: the hazard scan skips the value and keeps looking for the real subcommand;
/// safe prefix extraction instead treats them as unsafe to remember and refuses.
const GIT_VALUED_GLOBALS: &[&str] = &[
    "-c",
    "--config-env",
    "--exec-path",
    "-C",
    "--git-dir",
    "--work-tree",
    "--namespace",
];

/// The same holds for cargo's value-carrying options such as `--config`/`-Z`.
const CARGO_VALUED_GLOBALS: &[&str] = &["--config", "-Z", "--manifest-path", "-C"];

/// Value-carrying global options of JS package managers: `--prefix`/`--registry`/`--workspace` etc. rewrite the working
/// directory, install source or configuration, so their value must never be taken as a memorable subcommand. pnpm's `-w` is the boolean
/// `--workspace-root`, which differs from npm's value-carrying `-w|--workspace`, so it is not listed for pnpm.
const NPM_VALUED_GLOBALS: &[&str] = &[
    "-w",
    "--workspace",
    "--prefix",
    "--registry",
    "--userconfig",
];
const PNPM_VALUED_GLOBALS: &[&str] = &["-C", "--dir", "--prefix", "--registry", "--filter"];
const YARN_VALUED_GLOBALS: &[&str] = &["--cwd", "--registry", "--use-yarnrc"];
const BUN_VALUED_GLOBALS: &[&str] = &["-F", "--filter", "--cwd", "--registry"];

fn valued_globals(program: &str) -> Option<&'static [&'static str]> {
    match program {
        "git" => Some(GIT_VALUED_GLOBALS),
        "cargo" => Some(CARGO_VALUED_GLOBALS),
        "npm" => Some(NPM_VALUED_GLOBALS),
        "pnpm" => Some(PNPM_VALUED_GLOBALS),
        "yarn" => Some(YARN_VALUED_GLOBALS),
        "bun" => Some(BUN_VALUED_GLOBALS),
        _ => None,
    }
}

/// Whether a token is a value-carrying global option from the table (separate, `=`-attached or short-option-attached form).
fn valued_global_form(arg: &str, valued: &[&str]) -> bool {
    valued.iter().any(|flag| {
        if arg == *flag {
            return true;
        }
        if flag.starts_with("--") {
            return arg.starts_with(&format!("{flag}="));
        }
        arg.starts_with(flag) && arg.len() > flag.len()
    })
}

/// Subcommand scan used only for hazard classification.
///
/// Division of labour with [`extract_safe_subcommand_index`]: the latter serves persistent grant prefix derivation and
/// must return `None` when it meets a configuration override that cannot be remembered safely (`git -c`, `cargo --config`, npm's `--registry`, ...);
/// this function instead skips those options and their values (separate or `=` form) and keeps looking for the
/// real subcommand, so commands like `git -c …=… push` are not silently allowed because of a leading configuration argument.
/// Forms that cannot be proven statically, such as dynamic arguments, still return `None` and the caller handles them as before.
pub(super) fn extract_hazard_subcommand_for(program: &str, args: &[ArgToken]) -> Option<String> {
    extract_hazard_subcommand_index(program, args).map(|(sub, _)| sub)
}

pub(super) fn extract_hazard_subcommand_index(
    program: &str,
    args: &[ArgToken],
) -> Option<(String, usize)> {
    let Some(valued) = valued_globals(program) else {
        return extract_subcommand_index(args, &[]);
    };
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
                // `--opt=value` and short-option-attached forms (e.g. `-Csub`) carry the value in one token;
                // the separate form also needs the value token skipped. cargo's `+toolchain` global argument
                // behaves like a value-carrying option; skip it and keep looking for the real subcommand.
                let attached = if arg.starts_with("--") {
                    arg.contains('=')
                } else {
                    arg.len() > 2
                };
                if program == "cargo" && arg.starts_with('+') {
                    idx += 1;
                } else if valued_global_form(arg, valued) {
                    idx += if attached { 1 } else { 2 };
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

/// Extracts the subcommand and its index position after `args`; refuses unsafe configuration override arguments (e.g. `git -c`, `cargo --config`).
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
                        if arg == "--config"
                            || arg.starts_with("--config=")
                            || arg == "-Z"
                            || arg.starts_with("-Z")
                        {
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
        "npm" | "pnpm" | "yarn" | "bun" => {
            let valued = valued_globals(program).unwrap_or(&[]);
            let mut idx = 0;
            while idx < args.len() {
                match &args[idx] {
                    ArgToken::Literal(arg) => {
                        // A value-carrying global option rewrites the working directory, install source or configuration, so its
                        // value cannot be a memorable subcommand; its presence refuses persistent prefix derivation.
                        if valued_global_form(arg, valued) {
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
                        if arg.starts_with('-') {
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
