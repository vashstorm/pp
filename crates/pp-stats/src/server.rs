use super::{Range, Store, database_path, maintain};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};

mod http;

/// 同步工作端不可用时的固定响应体，HTTP 层与 [`dispatch`] 共用同一份字面量。
pub(super) const DATABASE_UNAVAILABLE: &str = r#"{"error":"statistics database unavailable"}"#;

pub fn run(
    host: std::net::IpAddr,
    port: u16,
    open: bool,
    mut session_titles: impl FnMut() -> Vec<(String, String)>,
) -> Result<()> {
    ensure!(
        !host.is_unspecified(),
        "stats host must be a specific IP address, not an unspecified address"
    );
    let mut store = Store::open(&database_path()?).context("opening statistics database")?;
    let settings_path = pp_config::config::global_dir()?.join("settings.yml");
    // 旧库退役在服务启动时执行；失败只告警，服务照常启动并在下次重试。
    if let Err(error) = maintain(&mut store.conn) {
        tracing::warn!(event = "statistics_migration_failed", error = %error);
    }
    let (transport, mut queries) =
        http::start(std::net::SocketAddr::new(host, port)).context("starting statistics server")?;
    let url = format!("http://{}/#/overview?range=24h", transport.address());
    println!("pp stats: {url}");
    std::io::stdout().flush()?;
    // Auto-open only on interactive terminals; piped, agent, or CI contexts just print the URL.
    if open && std::io::stdout().is_terminal() {
        open_browser(url);
    }
    // 查询串行化在启动线程上：`Store` 只被这里使用，标题回调也只在这里执行。
    while let Some(query) = queries.blocking_recv() {
        if query.reply.is_closed() {
            continue;
        }
        let titles = if query.target.starts_with("/api/stats/traces") {
            session_titles()
        } else {
            Vec::new()
        };
        let response = if query.target == http::STYLE_PATH {
            style_response(&settings_path, query.style)
        } else {
            dispatch(&store, &query.target, &titles)
        };
        let _ = query.reply.send(response);
    }
    transport.join()
}

fn style_response(
    settings_path: &std::path::Path,
    style: Option<pp_config::config::StatsStyle>,
) -> (u16, &'static str, String) {
    let result = if let Some(style) = style {
        style
            .save(settings_path)
            .map(|()| pp_config::config::StatsSettings { style })
    } else {
        pp_config::config::read_settings(settings_path).map(|settings| settings.stats)
    };
    match result {
        Ok(settings) => (200, "application/json", json!(settings).to_string()),
        Err(_) => (
            503,
            "application/json",
            json!({"error":"statistics style settings unavailable"}).to_string(),
        ),
    }
}

fn open_browser(url: String) {
    let result = std::thread::Builder::new()
        .name("stats-browser".into())
        .spawn(move || {
            let program = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            let opened = Command::new(program)
                .arg(&url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if !opened {
                eprintln!("warning: could not open browser; open {url} manually");
            }
        });
    if result.is_err() {
        eprintln!("warning: could not start browser opener; open the printed URL manually");
    }
}

pub(super) fn dispatch(
    store: &Store,
    path: &str,
    titles: &[(String, String)],
) -> (u16, &'static str, String) {
    if let Some((kind, text)) = static_text(path) {
        return (200, kind, text.to_owned());
    }
    match api(store, path, titles) {
        Ok(Some(value)) => (200, "application/json", value.to_string()),
        Ok(None) => (
            404,
            "application/json",
            json!({"error":"request or page not found"}).to_string(),
        ),
        Err(ApiError::Invalid) => (
            400,
            "application/json",
            json!({"error":"invalid statistics query"}).to_string(),
        ),
        Err(ApiError::Database) => {
            tracing::warn!(event = "statistics_query_failed");
            (503, "application/json", DATABASE_UNAVAILABLE.to_owned())
        }
    }
}

/// 内嵌文本资源表：HTTP 层与 [`dispatch`] 共用同一份精确 request-target 匹配。
pub(super) fn static_text(target: &str) -> Option<(&'static str, &'static str)> {
    Some(match target {
        "/" | "/index.html" => ("text/html", include_str!("index.html")),
        "/stats.css" => ("text/css", include_str!("stats.css")),
        "/web/stats-pages.css" => ("text/css", include_str!("web/stats-pages.css")),
        "/ui.css" => ("text/css", include_str!("ui.css")),
        "/web/ui-components.css" => ("text/css", include_str!("web/ui-components.css")),
        "/traces.css" => ("text/css", include_str!("traces.css")),
        "/modern.css" => ("text/css", include_str!("modern.css")),
        "/web/modern-pages.css" => ("text/css", include_str!("web/modern-pages.css")),
        "/stats.js" => ("text/javascript", include_str!("stats.js")),
        "/activity.js" => ("text/javascript", include_str!("activity.js")),
        "/insights.js" => ("text/javascript", include_str!("insights.js")),
        "/traces.js" => ("text/javascript", include_str!("traces.js")),
        "/context.js" => ("text/javascript", include_str!("context.js")),
        "/web/analytics-shared.js" => ("text/javascript", include_str!("web/analytics-shared.js")),
        "/web/charts.js" => ("text/javascript", include_str!("web/charts.js")),
        "/web/costs.js" => ("text/javascript", include_str!("web/costs.js")),
        "/web/format.js" => ("text/javascript", include_str!("web/format.js")),
        "/web/hooks.js" => ("text/javascript", include_str!("web/hooks.js")),
        "/web/models.js" => ("text/javascript", include_str!("web/models.js")),
        "/web/overview.js" => ("text/javascript", include_str!("web/overview.js")),
        "/web/projects.js" => ("text/javascript", include_str!("web/projects.js")),
        "/web/providers.js" => ("text/javascript", include_str!("web/providers.js")),
        "/web/requests.js" => ("text/javascript", include_str!("web/requests.js")),
        "/web/state.js" => ("text/javascript", include_str!("web/state.js")),
        "/web/table.js" => ("text/javascript", include_str!("web/table.js")),
        "/web/tooltip.js" => ("text/javascript", include_str!("web/tooltip.js")),
        "/web/traces-drawer.js" => ("text/javascript", include_str!("web/traces-drawer.js")),
        "/web/traces-scale.js" => ("text/javascript", include_str!("web/traces-scale.js")),
        "/web/traces-state.js" => ("text/javascript", include_str!("web/traces-state.js")),
        "/web/traces-timeline.js" => ("text/javascript", include_str!("web/traces-timeline.js")),
        "/icon.svg" => ("image/svg+xml", include_str!("icon.svg")),
        _ => return None,
    })
}
enum ApiError {
    Invalid,
    Database,
}
fn api(
    store: &Store,
    path: &str,
    session_titles: &[(String, String)],
) -> std::result::Result<Option<Value>, ApiError> {
    let titles = || {
        session_titles
            .iter()
            .map(|(id, title)| (id.as_str(), title.as_str()))
            .collect::<std::collections::HashMap<_, _>>()
    };
    let url =
        reqwest::Url::parse(&format!("http://localhost{path}")).map_err(|_| ApiError::Invalid)?;
    if let Some(id) = url.path().strip_prefix("/api/stats/traces/") {
        if url.query().is_some() {
            return Err(ApiError::Invalid);
        }
        let id = id
            .parse::<i64>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or(ApiError::Invalid)?;
        return store
            .trace(id, chrono::Utc::now().timestamp_millis(), &titles())
            .map_err(|_| ApiError::Database);
    }
    if let Some(id) = url.path().strip_prefix("/api/stats/requests/") {
        let id = id
            .parse::<i64>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or(ApiError::Invalid)?;
        return store.detail(id).map_err(|_| ApiError::Database);
    }
    if !matches!(
        url.path(),
        "/api/stats/overview"
            | "/api/stats/requests"
            | "/api/stats/models"
            | "/api/stats/providers"
            | "/api/stats/costs"
            | "/api/stats/errors"
            | "/api/stats/traces"
            | "/api/stats/tools"
            | "/api/stats/hooks"
            | "/api/stats/projects"
    ) {
        return Ok(None);
    }
    let mut range = Range::Day;
    let errors = url.path() == "/api/stats/errors";
    let mut limit = if errors {
        ERROR_LIMITS[0]
    } else {
        ROW_LIMITS[0]
    };
    let mut seen = std::collections::HashSet::new();
    for (key, value) in url.query_pairs() {
        if !seen.insert(key.to_string()) {
            return Err(ApiError::Invalid);
        }
        match key.as_ref() {
            "range" => range = Range::parse(&value).map_err(|_| ApiError::Invalid)?,
            "limit"
                if matches!(
                    url.path(),
                    "/api/stats/requests" | "/api/stats/errors" | "/api/stats/traces"
                ) =>
            {
                limit = value.parse::<usize>().map_err(|_| ApiError::Invalid)?;
                ensure_limit(errors, limit).map_err(|_| ApiError::Invalid)?;
            }
            _ => return Err(ApiError::Invalid),
        }
    }
    let now = chrono::Utc::now().timestamp_millis();
    if url.path() == "/api/stats/overview" {
        return store
            .overview(range, now)
            .map(Some)
            .map_err(|_| ApiError::Database);
    }
    if matches!(
        url.path(),
        "/api/stats/tools" | "/api/stats/hooks" | "/api/stats/projects"
    ) {
        let result = match url.path() {
            "/api/stats/tools" => store.tools(range, now),
            "/api/stats/hooks" => store.hooks(range, now),
            _ => store.projects(range, now),
        };
        return result.map(Some).map_err(|_| ApiError::Database);
    }
    if url.path() == "/api/stats/errors" {
        return store
            .errors(range, now, limit)
            .map(Some)
            .map_err(|_| ApiError::Database);
    }
    if url.path() == "/api/stats/traces" {
        return store
            .traces(range, now, limit, &titles())
            .map(Some)
            .map_err(|_| ApiError::Database);
    }
    if url.path() != "/api/stats/requests" {
        return store
            .analytics(range, now)
            .map(Some)
            .map_err(|_| ApiError::Database);
    }
    let rows = store
        .latest_with_context(range.since(now), now, limit + 1)
        .map_err(|_| ApiError::Database)?;
    let has_more = rows.len() > limit;
    let rows = rows
        .iter()
        .take(limit)
        .map(|(r, has_context)| {
            let mut value = r.value()?;
            value["has_context"] = json!(has_context);
            Ok(value)
        })
        .collect::<Result<Vec<_>>>()
        .map_err(|_| ApiError::Database)?;
    Ok(Some(
        json!({"requests":rows,"has_more":has_more,"limit":limit,"updated_at":now,"range":range.label()}),
    ))
}
/// 失败列表按页加载的档位：Errors 页先取最新 50 条，再按 200／1000 递增。
const ERROR_LIMITS: [usize; 3] = [50, 200, 1000];
/// 请求日志与 Traces 列表的档位。
const ROW_LIMITS: [usize; 2] = [500, 2000];

fn ensure_limit(errors: bool, limit: usize) -> Result<()> {
    let allowed = if errors {
        ERROR_LIMITS.contains(&limit)
    } else {
        ROW_LIMITS.contains(&limit)
    };
    ensure!(allowed, "invalid limit");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use crate::server;
    use std::collections::{BTreeSet, VecDeque};
    use std::path::Path;

    fn extract_quoted_attrs(html: &str, attr: &str) -> Vec<String> {
        let mut out = Vec::new();
        let needle = format!(r#"{attr}=""#);
        let mut rest = html;
        while let Some(idx) = rest.find(&needle) {
            let after = &rest[idx + needle.len()..];
            if let Some(end) = after.find('"') {
                out.push(after[..end].to_owned());
                rest = &after[end + 1..];
            } else {
                break;
            }
        }
        out
    }

    fn extract_esm_specifiers(source: &str) -> Vec<String> {
        let mut out = Vec::new();
        for line in source.lines() {
            let trimmed = line.trim();
            if !(trimmed.starts_with("import ")
                || trimmed.starts_with("export ")
                || trimmed.starts_with("} from "))
            {
                continue;
            }
            if let Some(idx) = trimmed.find("from \"") {
                let after = &trimmed[idx + 6..];
                if let Some(end) = after.find('"') {
                    out.push(after[..end].to_owned());
                }
            } else if let Some(after) = trimmed.strip_prefix("import \"")
                && let Some(end) = after.find('"')
            {
                out.push(after[..end].to_owned());
            }
        }
        out
    }

    fn resolve_web_path(base: &str, specifier: &str) -> String {
        let mut parts: Vec<&str> = base
            .rsplit_once('/')
            .map_or("", |(dir, _)| dir)
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        for seg in specifier.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                _ => parts.push(seg),
            }
        }
        format!("/{}", parts.join("/"))
    }

    fn collect_disk_assets(dir: &Path, prefix: &str, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let path = entry.path();
            if path.is_dir() {
                if name == "web" {
                    collect_disk_assets(&path, &format!("{prefix}/web"), out);
                }
                continue;
            }
            if name.ends_with(".js") || name.ends_with(".css") {
                out.insert(format!("{prefix}/{name}"));
            }
        }
    }

    #[test]
    fn stats_server_serves_traces_assets() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_store(&dir.path().join("stats.db")).unwrap();
        for (path, kind) in [
            ("/traces.js", "text/javascript"),
            ("/traces.css", "text/css"),
            ("/modern.css", "text/css"),
        ] {
            let (status, content_type, body) = server::dispatch(&store, path, &[]);
            assert_eq!(status, 200, "{path}");
            assert_eq!(content_type, kind, "{path}");
            assert!(!body.is_empty(), "{path}");
        }
    }

    #[test]
    fn stats_server_static_assets_and_esm_graph_are_complete() {
        let (_, html) = server::static_text("/index.html").expect("index.html");
        // CSP `script-src 'self'` 下禁止内联脚本或内联事件属性。
        assert!(!html.contains("onclick="));
        assert!(!html.contains("<style"));
        for chunk in html.split("<script").skip(1) {
            let tag_end = chunk.find('>').unwrap();
            assert!(
                chunk[..tag_end].contains("src="),
                "inline script tag found in index.html"
            );
        }

        let mut linked_css = BTreeSet::new();
        let mut entry_scripts = Vec::new();
        for attr in ["href", "src"] {
            for target in extract_quoted_attrs(html, attr) {
                if !target.starts_with('/') {
                    continue;
                }
                let (kind, body) = server::static_text(&target)
                    .unwrap_or_else(|| panic!("unregistered HTML asset: {target}"));
                assert!(!body.is_empty(), "{target}");
                if target.ends_with(".css") {
                    assert_eq!(kind, "text/css", "{target}");
                    linked_css.insert(target);
                } else if target.ends_with(".js") {
                    assert_eq!(kind, "text/javascript", "{target}");
                    entry_scripts.push(target);
                }
            }
        }
        assert!(
            !entry_scripts.is_empty(),
            "no entry module script in index.html"
        );

        let mut visited_js = BTreeSet::new();
        let mut queue: VecDeque<String> = entry_scripts.into();
        while let Some(module) = queue.pop_front() {
            if !visited_js.insert(module.clone()) {
                continue;
            }
            let (kind, source) = server::static_text(&module)
                .unwrap_or_else(|| panic!("missing ESM target: {module}"));
            assert_eq!(kind, "text/javascript", "{module}");
            for spec in extract_esm_specifiers(source) {
                if !spec.starts_with("./") && !spec.starts_with("../") {
                    continue;
                }
                let resolved = resolve_web_path(&module, &spec);
                assert!(
                    server::static_text(&resolved).is_some(),
                    "{module} imports unregistered asset {spec} -> {resolved}"
                );
                if resolved.ends_with(".js") && !visited_js.contains(&resolved) {
                    queue.push_back(resolved);
                }
            }
        }

        let mut disk_assets = BTreeSet::new();
        collect_disk_assets(
            Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")),
            "",
            &mut disk_assets,
        );
        for asset in disk_assets {
            assert!(
                server::static_text(&asset).is_some(),
                "disk asset missing from static_text: {asset}"
            );
            if asset.ends_with(".css") {
                assert!(
                    linked_css.contains(&asset),
                    "orphan CSS not linked in index.html: {asset}"
                );
            } else if asset.ends_with(".js") {
                assert!(
                    visited_js.contains(&asset),
                    "orphan JS not reachable from index.html: {asset}"
                );
            }
        }
    }
}
