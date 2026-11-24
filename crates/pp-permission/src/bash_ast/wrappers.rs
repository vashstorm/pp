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
                // 保留 `su`/`runuser` 与 `-c` 供 nested 解析处理，不剥离为无前缀脚本。
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
                // 只检查紧跟在 `command` 之后、`--` 或首个操作数之前的选项（含合并短选项如 `-pv`），
                // 避免被目标命令自身的 `-v` 参数（如 `command rm -rf / -v`）误判为查询。
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
                        // `--` 结束选项后紧跟的第一个参数仍是 duration，一并跳过再取真实命令。
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

/// git 全局取值选项：危险扫描跳过取值继续向后识别真实子命令；
/// 安全前缀提取则将它们视为不可安全记住而拒绝。
const GIT_VALUED_GLOBALS: &[&str] = &[
    "-c",
    "--config-env",
    "--exec-path",
    "-C",
    "--git-dir",
    "--work-tree",
    "--namespace",
];

/// cargo 的 `--config`/`-Z` 等取值选项同理。
const CARGO_VALUED_GLOBALS: &[&str] = &["--config", "-Z", "--manifest-path", "-C"];

/// JS 包管理器的带值全局选项：`--prefix`/`--registry`/`--workspace` 等会改写执行目录、
/// 安装源或配置，其取值绝不能被当作可记忆的子命令。pnpm 的 `-w` 是布尔型
/// `--workspace-root`，与 npm 的带值 `-w|--workspace` 语义不同，因此不列入 pnpm 表。
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

/// token 是否为表中的带值全局选项（分离、`=` 附加或短选项附加形态）。
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

/// 危险分类专用的子命令扫描。
///
/// 与 [`extract_safe_subcommand_index`] 的分工：后者服务于持久授权前缀推导，
/// 遇到不可安全记住的配置覆盖参数（`git -c`、`cargo --config`、npm 系 `--registry` 等）
/// 必须返回 `None`；本函数则跳过这些选项及其取值（分离或 `=` 形式）继续向后识别
/// 真实子命令，防止 `git -c …=… push` 之类命令因前置配置参数被静默放行。
/// 动态参数等无法静态证明的形态仍返回 `None`，由调用方按既有语义处理。
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
                // `--opt=value` 与短选项附加形态（如 `-Csub`）单 token 自带取值；
                // 分离形态需同时跳过取值 token。cargo 的 `+toolchain` 全局参数
                // 与取值选项同理，跳过后继续识别真实子命令。
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
                        // 带值全局选项改写执行目录、安装源或配置，其取值不可当作
                        // 可记忆的子命令；出现即拒绝持久前缀推导。
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
