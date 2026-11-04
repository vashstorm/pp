//! 诊断编排：只向匹配的 language server 采集诊断，生成可信报告。
//!
//! 报告里的每个状态都来自真实响应：没有匹配服务器、启动失败、没有诊断能力与
//! 未收到新鲜报告各自保留原因；任何一条路径都不会把“未知”渲染成“干净”。
//! 显式 diagnostics 永远返回当前真实问题，不套跨轮去重账本。

use anyhow::{Context, Result, ensure};
use globset::GlobBuilder;
use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::lsp::Registry;
use crate::lsp::adapters;
use crate::lsp::types::Diagnostic;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;
use pp_core::cancellation::CleanupError;
use pp_core::lsp_report::DiagnosticReport;
use pp_core::lsp_report::ReportDiagnostic;
use pp_core::lsp_report::ReportStatus;
use pp_core::lsp_report::ServerReport;

/// 一次 glob diagnostics 最多解析的普通文件数。
pub(crate) const MAX_TARGETS: usize = 20;

/// 自动钩子（保存/编辑后）最多展示的诊断消息数；显式 diagnostics 不受此限。
pub const AUTO_MESSAGE_LIMIT: usize = 50;

/// glob 解析结果；`truncated` 为 true 时调用方必须提示只检查了前 20 个文件。
#[derive(Clone, Debug, Default)]
pub(crate) struct DiagnosticTargets {
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) truncated: bool,
}

/// 采集单个文件的诊断。
///
/// - 查询所有文件类型匹配的真实服务器；旧 CLI adapter 明确报告为不支持。
/// - 单服务器的启动/请求失败写入该服务器的 `ServerReport`，不影响其他服务器；
///   取消与清理失败保留 typed error 向上传播。
/// - `deadline` 是绝对期限，启动、加载与请求共用，不要每层重新起算。
pub(crate) fn collect(
    registry: &Registry,
    path: &Path,
    origin: &str,
    owner: &str,
    _include_adapters: bool,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<DiagnosticReport> {
    pp_core::cancellation::check(cancellation)?;
    ensure!(registry.enabled(), "LSP support is disabled.");
    // 配置错误是显式调用必须看到的原因，不能被“没有匹配服务器”掩盖。
    let _config = registry.config()?;
    // 保存调用方给出的路径用于展示，但读写都基于工作区内的绝对路径。
    let target = if path.is_absolute() {
        path.to_path_buf()
    } else {
        registry.cwd().join(path)
    };
    let mut servers = Vec::new();
    for definition in registry.servers_for_file(&target) {
        pp_core::cancellation::check(cancellation)?;
        if adapters::is_adapter(&definition.name) {
            servers.push(ServerReport {
                server: definition.name,
                status: ReportStatus::Unsupported,
                note: Some("CLI adapters are not run by LSP; use bash.".to_owned()),
                diagnostics: Vec::new(),
            });
            continue;
        }
        servers.push(server_report(
            registry,
            &definition.name,
            &target,
            deadline,
            cancellation,
        )?);
    }
    deduplicate(&mut servers);
    Ok(DiagnosticReport {
        path: path.display().to_string(),
        origin: origin.to_owned(),
        owner: owner.to_owned(),
        servers,
        formatter: None,
        deduplicated: 0,
        omitted: 0,
        at_ms: now_ms(),
    })
}

fn is_control(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<Cancelled>() || cause.is::<CleanupError>())
}

/// 把报告渲染成工具/终端共用的英文分组正文。
///
/// `max_messages` 只给自动钩子使用（最多 50 条）；显式 diagnostics 传 `None`
/// 保留完整报告，由工具输出限额控制正文。任何“没有报告”的状态都会显式出现，
/// 不会读成 OK。
pub fn render(report: &DiagnosticReport, max_messages: Option<usize>) -> String {
    let path = clean_text(&report.path);
    if report.servers.is_empty() {
        return match &report.formatter {
            Some(formatter) => format!(
                "Formatting for {path}\n  {}: {}{}",
                clean_text(&formatter.server),
                formatter.status.label(),
                formatter
                    .note
                    .as_deref()
                    .map(|note| format!(" - {}", note_text(note)))
                    .unwrap_or_default()
            ),
            None => format!("No language server covers {path}; diagnostics were not collected."),
        };
    }
    let mut remaining = max_messages.unwrap_or(usize::MAX);
    let mut omitted = report.omitted;
    let mut lines = vec![format!("Diagnostics for {path}")];
    for server in &report.servers {
        lines.push(status_line(server));
        for diagnostic in &server.diagnostics {
            if remaining == 0 {
                omitted += 1;
                continue;
            }
            remaining -= 1;
            lines.push(format!("    {}", diagnostic_line(&path, diagnostic)));
        }
    }
    if omitted > 0 {
        lines.push(format!("  … {omitted} more diagnostic(s) not shown."));
    }
    if let Some(formatter) = &report.formatter {
        lines.push(format!(
            "Formatter {}: {}{}",
            clean_text(&formatter.server),
            formatter.status.label(),
            formatter
                .note
                .as_deref()
                .map(|note| format!(" - {}", note_text(note)))
                .unwrap_or_default()
        ));
    }
    let mut summary = summary_line(report);
    let flags = status_flags(report);
    if !flags.is_empty() {
        summary.push_str("; ");
        summary.push_str(&flags.join("; "));
    }
    lines.push(format!("Summary: {summary}"));
    lines.join("\n")
}

/// 把 diagnostics 的 `file` 参数解析成有界、排序固定的普通文件列表。
///
/// 无 glob 字符时按单文件处理；其余情况按 cwd 相对 glob 扫描，最多返回
/// [`MAX_TARGETS`] 个路径并置 `truncated`。无匹配返回空列表，由调用方明确
/// 提示“没有匹配”，不能当成 OK。
pub(crate) fn resolve_targets(
    cwd: &Path,
    pattern: &str,
    cancellation: &CancellationToken,
) -> Result<DiagnosticTargets> {
    pp_core::cancellation::check(cancellation)?;
    let pattern = pattern.trim();
    ensure!(!pattern.is_empty(), "the diagnostics pattern is empty");
    if !contains_glob(pattern) {
        return Ok(DiagnosticTargets {
            paths: vec![absolute(cwd, Path::new(pattern))],
            truncated: false,
        });
    }
    let direct = absolute(cwd, Path::new(pattern));
    if std::fs::metadata(&direct).is_ok_and(|metadata| metadata.is_file()) {
        return Ok(DiagnosticTargets {
            paths: vec![direct],
            truncated: false,
        });
    }
    let (root, relative) = glob_scope(cwd, pattern);
    ensure!(
        root != Path::new("/"),
        "refusing to scan the filesystem root for diagnostics"
    );
    let matcher = GlobBuilder::new(&relative)
        .literal_separator(true)
        .build()
        .with_context(|| format!("invalid diagnostics glob `{pattern}`"))?
        .compile_matcher();
    let depth = if relative.contains("**") {
        None
    } else {
        Some(
            relative
                .split('/')
                .filter(|segment| !segment.is_empty())
                .count()
                .max(1),
        )
    };
    let mut matches = BTreeSet::new();
    let mut truncated = false;
    let mut builder = ignore::WalkBuilder::new(&root);
    builder
        .hidden(true)
        .follow_links(false)
        .ignore(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .require_git(false)
        .parents(false)
        .max_depth(depth);
    for entry in builder.build() {
        pp_core::cancellation::check(cancellation)?;
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 || !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let candidate = entry.path().strip_prefix(&root).unwrap_or(entry.path());
        if !matcher.is_match(candidate) {
            continue;
        }
        matches.insert(normalize(entry.path()));
        // 只保留最小的 MAX_TARGETS 个路径；多出来的最大值立即丢弃，扫描期间
        // 的占用因此有界，而不是先收集整个工作区再截断。
        if matches.len() > MAX_TARGETS {
            truncated = true;
            matches.pop_last();
        }
    }
    let paths = matches.into_iter().collect();
    Ok(DiagnosticTargets { paths, truncated })
}

fn server_report(
    registry: &Registry,
    name: &str,
    path: &Path,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<ServerReport> {
    let client = match registry.ensure_client(name, deadline, cancellation) {
        Ok(client) => client,
        Err(error) => return failed_or_control(name, error),
    };
    let outcome = match client.collect_diagnostics(path, deadline, cancellation) {
        Ok(outcome) => outcome,
        Err(error) => return failed_or_control(name, error),
    };
    Ok(ServerReport {
        server: name.to_owned(),
        status: outcome.status,
        note: outcome
            .note
            .map(|note| pp_core::logging::diagnostic_text(&note)),
        diagnostics: outcome
            .diagnostics
            .unwrap_or_default()
            .iter()
            .map(report_diagnostic)
            .collect(),
    })
}

/// 单服务器失败写入报告；取消与清理失败不是“服务器报告”，必须保留类型上抛。
fn failed_or_control(name: &str, error: anyhow::Error) -> Result<ServerReport> {
    if is_control(&error) {
        return Err(error);
    }
    Ok(ServerReport {
        server: name.to_owned(),
        status: ReportStatus::Failed,
        note: Some(pp_core::logging::diagnostic_text(&format!("{error:#}"))),
        diagnostics: Vec::new(),
    })
}

/// LSP 位置是 0-based UTF-16；报告统一保存 1-based，供工具与终端直接展示。
fn report_diagnostic(diagnostic: &Diagnostic) -> ReportDiagnostic {
    ReportDiagnostic {
        line: diagnostic.range.start.line.saturating_add(1),
        column: diagnostic.range.start.character.saturating_add(1),
        end_line: diagnostic.range.end.line.saturating_add(1),
        end_column: diagnostic.range.end.character.saturating_add(1),
        severity: diagnostic
            .severity
            .map(|severity| severity.0)
            .unwrap_or(1)
            .clamp(1, 4),
        message: pp_core::logging::diagnostic_text(&diagnostic.message),
        code: diagnostic.code.as_ref().map(code_text),
        source: diagnostic.source.clone(),
        line_only: false,
    }
}

fn code_text(code: &serde_json::Value) -> String {
    match code {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// 跨服务器按 range+message 去重，重复时保留更高 severity。
///
/// 第一遍只读扫描，用借用消息的键记录每个身份当前的胜者下标；第二遍按原顺序
/// 把胜者移回对应服务器分组，不复制任何诊断内容。同等级重复保留先到的条目，
/// 保证结果稳定。最后每个服务器内部按 severity、位置与消息稳定排序。
fn deduplicate(servers: &mut [ServerReport]) {
    let mut flat: Vec<(usize, ReportDiagnostic)> = Vec::new();
    for (index, server) in servers.iter_mut().enumerate() {
        for item in server.diagnostics.drain(..) {
            flat.push((index, item));
        }
    }
    let kept = {
        let mut winners: HashMap<DiagnosticKey<'_>, usize> = HashMap::new();
        for (index, (_, item)) in flat.iter().enumerate() {
            let key = DiagnosticKey::of(item);
            match winners.get(&key).copied() {
                Some(owner) if item.severity < flat[owner].1.severity => {
                    // 更严重的重复替换胜者：hint 不可能压掉后到的 error。
                    winners.insert(key, index);
                }
                Some(_) => {}
                None => {
                    winners.insert(key, index);
                }
            }
        }
        let mut kept = vec![false; flat.len()];
        for index in winners.into_values() {
            kept[index] = true;
        }
        kept
    };
    for (position, (server_index, item)) in flat.into_iter().enumerate() {
        if kept[position] {
            servers[server_index].diagnostics.push(item);
        }
    }
    for server in servers.iter_mut() {
        server.diagnostics.sort_by(|a, b| {
            a.severity
                .cmp(&b.severity)
                .then(a.line.cmp(&b.line))
                .then(a.column.cmp(&b.column))
                .then(a.end_line.cmp(&b.end_line))
                .then(a.end_column.cmp(&b.end_column))
                .then(a.message.cmp(&b.message))
        });
    }
}

/// 去重身份键：借用诊断自身的消息，避免为扫描复制内容。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct DiagnosticKey<'a> {
    line: u32,
    column: u32,
    end_line: u32,
    end_column: u32,
    message: &'a str,
}

impl<'a> DiagnosticKey<'a> {
    fn of(diagnostic: &'a ReportDiagnostic) -> Self {
        Self {
            line: diagnostic.line,
            column: diagnostic.column,
            end_line: diagnostic.end_line,
            end_column: diagnostic.end_column,
            message: &diagnostic.message,
        }
    }
}

fn status_line(server: &ServerReport) -> String {
    let name = clean_text(&server.server);
    let note = server.note.as_deref().map(note_text);
    match server.status {
        ReportStatus::ReportedClean => format!("  {name}: clean"),
        ReportStatus::ReportedIssues => {
            if server.diagnostics.is_empty() {
                format!("  {name}: issues reported, duplicate messages omitted")
            } else {
                format!(
                    "  {name}: {}",
                    count_label(server.diagnostics.len(), "issue")
                )
            }
        }
        ReportStatus::Unreported => with_note(&name, "unverified", note),
        ReportStatus::Failed => with_note(&name, "failed", note),
        ReportStatus::Unsupported => with_note(&name, "unsupported", note),
    }
}

fn with_note(name: &str, status: &str, note: Option<String>) -> String {
    match note {
        Some(note) => format!("  {name}: {status} - {note}"),
        None => format!("  {name}: {status}"),
    }
}

fn diagnostic_line(path: &str, diagnostic: &ReportDiagnostic) -> String {
    let severity = severity_name(diagnostic.severity);
    let source = diagnostic
        .source
        .as_deref()
        .map(|source| format!("[{}] ", clean_text(source)))
        .unwrap_or_default();
    let code = diagnostic
        .code
        .as_deref()
        .map(|code| format!(" ({})", clean_text(code)))
        .unwrap_or_default();
    let message = one_line(&diagnostic.message);
    if diagnostic.line_only {
        format!(
            "{path}:{} [{severity}] {source}{message}{code}",
            diagnostic.line
        )
    } else {
        format!(
            "{path}:{}:{} [{severity}] {source}{message}{code}",
            diagnostic.line, diagnostic.column
        )
    }
}

fn summary_line(report: &DiagnosticReport) -> String {
    let mut counts = [0usize; 5];
    for server in &report.servers {
        for diagnostic in &server.diagnostics {
            counts[usize::from(diagnostic.severity.clamp(1, 4))] += 1;
        }
    }
    let mut parts = Vec::new();
    for (severity, label) in [(1, "error"), (2, "warning"), (3, "info"), (4, "hint")] {
        if counts[severity] > 0 {
            parts.push(count_label(counts[severity], label));
        }
    }
    if report.deduplicated > 0 || report.omitted > 0 {
        return format!(
            "{}; {} unchanged diagnostic(s) omitted{}",
            count_label(report.total(), "issue"),
            report.deduplicated,
            if parts.is_empty() {
                String::new()
            } else {
                format!("; new: {}", parts.join(", "))
            }
        );
    }
    if !parts.is_empty() {
        return parts.join(", ");
    }
    if report.all_clean() {
        "no issues found".to_owned()
    } else {
        "no issues reported, but diagnostics are incomplete".to_owned()
    }
}

fn status_flags(report: &DiagnosticReport) -> Vec<String> {
    let mut flags = Vec::new();
    for (status, label) in [
        (ReportStatus::Unreported, "unverified"),
        (ReportStatus::Failed, "failed"),
        (ReportStatus::Unsupported, "unsupported"),
    ] {
        let names: Vec<String> = report
            .servers
            .iter()
            .filter(|server| server.status == status)
            .map(|server| clean_text(&server.server))
            .collect();
        if !names.is_empty() {
            flags.push(format!("{label}: {}", names.join(", ")));
        }
    }
    flags
}

fn count_label(count: usize, label: &str) -> String {
    if count == 1 {
        format!("1 {label}")
    } else {
        format!("{count} {label}s")
    }
}

fn severity_name(severity: u8) -> &'static str {
    match severity {
        1 => "error",
        2 => "warning",
        3 => "info",
        4 => "hint",
        _ => "unknown",
    }
}

fn clean_text(text: &str) -> String {
    one_line(&pp_core::logging::diagnostic_text(text))
}

fn note_text(note: &str) -> String {
    let mut text = one_line(&pp_core::logging::diagnostic_text(note));
    if text.chars().count() > 400 {
        text = text.chars().take(400).collect::<String>() + "…";
    }
    text
}

fn one_line(text: &str) -> String {
    let mut flat = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            space = true;
            continue;
        }
        if space && !flat.is_empty() {
            flat.push(' ');
        }
        space = false;
        flat.push(ch);
    }
    flat
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn contains_glob(pattern: &str) -> bool {
    pattern
        .chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | '{'))
}

/// 把 glob 拆成「静态目录根 + 相对模式」；绝对模式只看静态前缀，避免从 `/` 扫描。
fn glob_scope(cwd: &Path, pattern: &str) -> (PathBuf, String) {
    let normalized = pattern.replace('\\', "/");
    let base = if normalized.starts_with('/') {
        PathBuf::from("/")
    } else {
        cwd.to_path_buf()
    };
    let segments: Vec<&str> = normalized
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let first = segments
        .iter()
        .position(|segment| contains_glob(segment))
        .unwrap_or(segments.len());
    let mut root = base;
    for segment in &segments[..first] {
        root.push(segment);
    }
    let relative = segments[first..].join("/");
    (normalize(&root), relative)
}

fn absolute(cwd: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    normalize(&joined)
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::config;
    use pp_config::config::LspSettings;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;

    fn registry(dir: &Path) -> Registry {
        let config = config::Config::load_with(dir, None).unwrap();
        Registry::from_config(
            dir.to_path_buf(),
            LspSettings::default(),
            Ok(Arc::new(config)),
        )
    }

    fn diagnostic(severity: u8, line: u32, message: &str) -> ReportDiagnostic {
        ReportDiagnostic {
            line,
            column: 1,
            end_line: line,
            end_column: 2,
            severity,
            message: message.to_owned(),
            code: None,
            source: None,
            line_only: false,
        }
    }

    #[test]
    fn lsp_diagnostics_partial_failure_keeps_other_reports() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".pp")).unwrap();
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/lsp_server.py");
        let log = dir.path().join("requests.jsonl");
        std::fs::write(
            dir.path().join(".pp/lsp.json"),
            json!({"servers": {
                "fixture": {
                    "command": "python3",
                    "args": [script, "--log", log],
                    "fileTypes": [".fixture"],
                    "rootMarkers": [".pp"],
                },
                "broken": {
                    "command": "/bin/sh",
                    "args": ["-c", "exit 1"],
                    "fileTypes": [".fixture"],
                    "rootMarkers": [".pp"],
                },
            }})
            .to_string(),
        )
        .unwrap();
        let registry = registry(dir.path());
        let mut cleanup = registry.cleanup();
        let path = dir.path().join("sample.fixture");
        std::fs::write(&path, "wrong\n").unwrap();
        let report = collect(
            &registry,
            &path,
            "test",
            "owner",
            true,
            Instant::now() + Duration::from_secs(10),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(report.servers.len(), 2, "{report:?}");
        let fixture = report
            .servers
            .iter()
            .find(|server| server.server == "fixture")
            .expect("fixture report");
        assert_eq!(fixture.status, ReportStatus::ReportedIssues);
        assert_eq!(fixture.diagnostics.len(), 1);
        assert_eq!(fixture.diagnostics[0].line, 1);
        assert_eq!(fixture.diagnostics[0].column, 1);
        let broken = report
            .servers
            .iter()
            .find(|server| server.server == "broken")
            .expect("broken report");
        assert_eq!(broken.status, ReportStatus::Failed);
        assert!(broken.note.is_some(), "{broken:?}");
        assert!(report.has_issues());
        assert!(!report.all_clean());
        let rendered = render(&report, None);
        assert!(rendered.contains("fixture: 1 issue"), "{rendered}");
        assert!(rendered.contains("broken: failed"), "{rendered}");
        cleanup.stop().unwrap();
    }

    #[test]
    fn lsp_diagnostics_cancelled_collect_keeps_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".pp")).unwrap();
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/lsp_server.py");
        let log = dir.path().join("requests.jsonl");
        let barrier = dir.path().join("diagnostics");
        std::fs::write(
            dir.path().join(".pp/lsp.json"),
            json!({"servers": {"fixture": {
                "command": "python3",
                "args": [script, "--log", log, "--barrier-dir", dir.path()],
                "fileTypes": [".fixture"],
                "rootMarkers": [".pp"],
            }}})
            .to_string(),
        )
        .unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&barrier)
                .status()
                .unwrap()
                .success()
        );
        let registry = registry(dir.path());
        let mut cleanup = registry.cleanup();
        let path = dir.path().join("sample.fixture");
        std::fs::write(&path, "wrong\n").unwrap();
        let cancellation = CancellationToken::new();
        let outcome = std::thread::scope(|scope| {
            let worker = {
                let registry = registry.clone();
                let cancellation = cancellation.clone();
                let path = path.clone();
                scope.spawn(move || {
                    collect(
                        &registry,
                        &path,
                        "test",
                        "owner",
                        true,
                        Instant::now() + Duration::from_secs(10),
                        &cancellation,
                    )
                })
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let reached = std::fs::read_to_string(&log)
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                    .any(|entry| entry["barrier"] == "diagnostics");
                if reached {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "fixture never reached the pull barrier"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            cancellation.cancel();
            worker.join().unwrap()
        });
        let error = match outcome {
            Ok(report) => panic!("cancelled collect returned {report:?}"),
            Err(error) => error,
        };
        assert!(error.is::<Cancelled>(), "{error:#}");
        // 让阻塞在 FIFO 上的 fixture 继续走到 shutdown，避免退出清理只能等超时。
        let unblock = barrier.clone();
        std::thread::spawn(move || {
            use std::io::Write;
            if let Ok(mut stream) = std::fs::OpenOptions::new().write(true).open(&unblock) {
                let _ = stream.write_all(b"\n");
            }
        });
        cleanup.stop().unwrap();
    }

    #[test]
    fn lsp_diagnostics_dedup_keeps_highest_severity() {
        let mut servers = vec![
            ServerReport {
                server: "hint".to_owned(),
                status: ReportStatus::ReportedIssues,
                note: None,
                diagnostics: vec![diagnostic(4, 3, "same issue")],
            },
            ServerReport {
                server: "error".to_owned(),
                status: ReportStatus::ReportedIssues,
                note: None,
                diagnostics: vec![
                    diagnostic(1, 3, "same issue"),
                    diagnostic(2, 1, "other issue"),
                ],
            },
        ];
        deduplicate(&mut servers);
        assert!(
            servers[0].diagnostics.is_empty(),
            "the earlier hint must yield to a later error"
        );
        assert_eq!(servers[1].diagnostics.len(), 2);
        assert_eq!(servers[1].diagnostics[0].severity, 1);
        assert_eq!(servers[1].diagnostics[0].line, 3);
        assert_eq!(servers[1].diagnostics[1].severity, 2);
        assert_eq!(servers[1].diagnostics[1].line, 1);

        // 顺序反转时 error 先到，hint 直接丢弃，错误位置不变。
        let mut reversed = vec![
            ServerReport {
                server: "error".to_owned(),
                status: ReportStatus::ReportedIssues,
                note: None,
                diagnostics: vec![diagnostic(1, 3, "same issue")],
            },
            ServerReport {
                server: "hint".to_owned(),
                status: ReportStatus::ReportedIssues,
                note: None,
                diagnostics: vec![diagnostic(4, 3, "same issue")],
            },
        ];
        deduplicate(&mut reversed);
        assert_eq!(reversed[0].diagnostics.len(), 1);
        assert_eq!(reversed[0].diagnostics[0].severity, 1);
        assert!(reversed[1].diagnostics.is_empty());

        // 同等级的重复保留先到条目，后到条目被丢弃。
        let mut equal = vec![
            ServerReport {
                server: "first".to_owned(),
                status: ReportStatus::ReportedIssues,
                note: None,
                diagnostics: vec![diagnostic(2, 5, "twice")],
            },
            ServerReport {
                server: "second".to_owned(),
                status: ReportStatus::ReportedIssues,
                note: None,
                diagnostics: vec![diagnostic(2, 5, "twice")],
            },
        ];
        deduplicate(&mut equal);
        assert_eq!(equal[0].diagnostics.len(), 1);
        assert!(equal[1].diagnostics.is_empty());
    }

    #[test]
    fn lsp_diagnostics_render_never_fakes_clean() {
        let empty = DiagnosticReport {
            path: "src/lib.rs".to_owned(),
            origin: "test".to_owned(),
            owner: "owner".to_owned(),
            ..Default::default()
        };
        let text = render(&empty, None);
        assert!(
            text.contains("No language server covers src/lib.rs"),
            "{text}"
        );
        assert!(!text.contains("no issues found"), "{text}");

        let unverified = DiagnosticReport {
            path: "src/lib.rs".to_owned(),
            servers: vec![ServerReport {
                server: "tsserver".to_owned(),
                status: ReportStatus::Unreported,
                note: Some("No fresh diagnostics arrived before the deadline.".to_owned()),
                diagnostics: Vec::new(),
            }],
            ..Default::default()
        };
        let text = render(&unverified, None);
        assert!(text.contains("tsserver: unverified"), "{text}");
        assert!(text.contains("diagnostics are incomplete"), "{text}");
        assert!(!text.contains("no issues found"), "{text}");

        let clean = DiagnosticReport {
            path: "src/lib.rs".to_owned(),
            servers: vec![ServerReport {
                server: "tsserver".to_owned(),
                status: ReportStatus::ReportedClean,
                note: None,
                diagnostics: Vec::new(),
            }],
            ..Default::default()
        };
        let text = render(&clean, None);
        assert!(text.contains("tsserver: clean"), "{text}");
        assert!(text.contains("Summary: no issues found"), "{text}");

        let issues = DiagnosticReport {
            path: "src/lib.rs".to_owned(),
            servers: vec![ServerReport {
                server: "tsserver".to_owned(),
                status: ReportStatus::ReportedIssues,
                note: None,
                diagnostics: vec![diagnostic(1, 2, "first"), diagnostic(2, 4, "second")],
            }],
            ..Default::default()
        };
        let text = render(&issues, None);
        assert!(text.contains("src/lib.rs:2:1 [error] first"), "{text}");
        assert!(text.contains("src/lib.rs:4:1 [warning] second"), "{text}");

        // 自动调用方最多 50 条；这里用 1 条验证截断提示，显式调用保留全部。
        let limited = render(&issues, Some(1));
        assert!(
            limited.contains("1 more diagnostic(s) not shown"),
            "{limited}"
        );
        assert!(limited.contains("first"), "{limited}");
        assert!(!limited.contains("[warning] second"), "{limited}");
        let full = render(&issues, None);
        assert!(full.contains("[warning] second"), "{full}");
    }

    #[test]
    fn lsp_diagnostics_targets_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("single.rs"), "fn main() {}\n").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        for index in 0..25 {
            std::fs::write(
                dir.path().join("src").join(format!("file{index:02}.rs")),
                "fn main() {}\n",
            )
            .unwrap();
        }
        let cancellation = CancellationToken::new();

        let single = resolve_targets(dir.path(), "single.rs", &cancellation).unwrap();
        assert!(!single.truncated);
        assert_eq!(single.paths, vec![dir.path().join("single.rs")]);

        let all = resolve_targets(dir.path(), "src/*.rs", &cancellation).unwrap();
        assert!(all.truncated);
        assert_eq!(all.paths.len(), MAX_TARGETS);
        assert!(all.paths.iter().all(|path| path.is_absolute()));
        let mut sorted = all.paths.clone();
        sorted.sort();
        assert_eq!(all.paths, sorted);
        assert_eq!(all.paths[0], dir.path().join("src/file00.rs"));
        assert_eq!(all.paths[MAX_TARGETS - 1], dir.path().join("src/file19.rs"));

        let few = resolve_targets(dir.path(), "src/file0*.rs", &cancellation).unwrap();
        assert!(!few.truncated);
        assert_eq!(few.paths.len(), 10);

        let none = resolve_targets(dir.path(), "*.nothing", &cancellation).unwrap();
        assert!(none.paths.is_empty());
        assert!(!none.truncated);

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let error = resolve_targets(dir.path(), "src/*.rs", &cancelled).unwrap_err();
        assert!(error.is::<Cancelled>(), "{error:#}");
    }
}
