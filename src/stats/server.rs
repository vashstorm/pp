use super::{Range, Store, database_path};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};
use tiny_http::{Header, Method, Response, Server, StatusCode};

pub(crate) fn run(port: u16, open: bool) -> Result<()> {
    let store = Store::open(&database_path()?).context("opening statistics database")?;
    let server = Server::http((std::net::Ipv4Addr::LOCALHOST, port))
        .map_err(|e| anyhow::anyhow!("starting statistics server: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .context("statistics listener unavailable")?
        .port();
    let url = format!("http://127.0.0.1:{port}/#/overview?range=24h");
    println!("pp stats: {url}");
    std::io::stdout().flush()?;
    // Auto-open only on interactive terminals; piped, agent, or CI contexts just print the URL.
    if open && std::io::stdout().is_terminal() {
        open_browser(url);
    }
    for request in server.incoming_requests() {
        let host = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("Host"))
            .map(|h| h.value.as_str())
            .unwrap_or_default();
        let origin = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("Origin"))
            .map(|h| h.value.as_str());
        let allowed = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
        let valid_host = allowed.iter().any(|s| s == host);
        let valid_origin =
            origin.is_none_or(|o| allowed.iter().any(|s| o == format!("http://{s}")));
        let (status, kind, body) = if !valid_host || !valid_origin {
            (
                403,
                "application/json",
                json!({"error":"local origin required"}).to_string(),
            )
        } else if request.method() != &Method::Get {
            (
                405,
                "application/json",
                json!({"error":"GET required"}).to_string(),
            )
        } else if request.url() == "/fonts/InterVariable.woff2" {
            (200, "font/woff2", String::new())
        } else {
            dispatch(&store, request.url())
        };
        let body = if kind == "font/woff2" {
            include_bytes!("fonts/InterVariable.woff2").to_vec()
        } else {
            body.into_bytes()
        };
        let mut response = Response::from_data(body).with_status_code(StatusCode(status));
        for (name,value) in [("Content-Type",format!("{kind}; charset=utf-8")),("Cache-Control","no-store".into()),("X-Content-Type-Options","nosniff".into()),("Content-Security-Policy","default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'none'".into())] {
            if let Ok(header)=Header::from_bytes(name,value){response.add_header(header);}
        }
        let _ = request.respond(response);
    }
    Ok(())
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

pub(super) fn dispatch(store: &Store, path: &str) -> (u16, &'static str, String) {
    match path {
        "/" | "/index.html" => return (200, "text/html", include_str!("index.html").into()),
        "/stats.css" => return (200, "text/css", include_str!("stats.css").into()),
        "/stats.js" => return (200, "text/javascript", include_str!("stats.js").into()),
        "/activity.js" => return (200, "text/javascript", include_str!("activity.js").into()),
        "/insights.js" => return (200, "text/javascript", include_str!("insights.js").into()),
        "/context.js" => return (200, "text/javascript", include_str!("context.js").into()),
        "/icon.svg" => return (200, "image/svg+xml", include_str!("icon.svg").into()),
        _ => {}
    }
    match api(store, path) {
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
            (
                503,
                "application/json",
                json!({"error":"statistics database unavailable"}).to_string(),
            )
        }
    }
}
enum ApiError {
    Invalid,
    Database,
}
fn api(store: &Store, path: &str) -> std::result::Result<Option<Value>, ApiError> {
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
            .trace(
                id,
                chrono::Utc::now().timestamp_millis(),
                &super::analytics::Pricing::load(store),
            )
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
            | "/api/stats/projects"
    ) {
        return Ok(None);
    }
    let mut range = Range::Day;
    let mut limit = 500;
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
                ensure_limit(limit).map_err(|_| ApiError::Invalid)?;
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
    let pricing = super::analytics::Pricing::load(store);
    if matches!(url.path(), "/api/stats/tools" | "/api/stats/projects") {
        let result = if url.path() == "/api/stats/tools" {
            store.tools(range, now, &pricing)
        } else {
            store.projects(range, now, &pricing)
        };
        return result.map(Some).map_err(|_| ApiError::Database);
    }
    if url.path() == "/api/stats/errors" {
        return store
            .errors(range, now, limit, &pricing)
            .map(Some)
            .map_err(|_| ApiError::Database);
    }
    if url.path() == "/api/stats/traces" {
        return store
            .traces(range, now, limit, &pricing)
            .map(Some)
            .map_err(|_| ApiError::Database);
    }
    if url.path() != "/api/stats/requests" {
        return store
            .analytics(range, now, &pricing)
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
            let mut value = pricing.request(r)?;
            value["has_context"] = json!(has_context);
            Ok(value)
        })
        .collect::<Result<Vec<_>>>()
        .map_err(|_| ApiError::Database)?;
    Ok(Some(
        json!({"requests":rows,"has_more":has_more,"limit":limit,"updated_at":now,"range":range.label(),"pricing_error":pricing.error}),
    ))
}
fn ensure_limit(limit: usize) -> Result<()> {
    ensure!(matches!(limit, 500 | 2000), "invalid limit");
    Ok(())
}
