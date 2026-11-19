use super::*;
use std::fs;

#[test]
fn stats_slash_starts_reuses_and_stops_server_without_models() {
    use crate::streaming::{LiveCli, Pipe};
    let home = tempfile::tempdir().unwrap();
    let bin = stats_browser_fixture(home.path());
    let mut cli = LiveCli::new(Cli::start_env(
        home.path(),
        home.path(),
        &[],
        &[("PATH", bin.to_str().unwrap())],
    ));
    cli.wait_for(Pipe::Stderr, "Configuration unavailable");
    cli.input("/stats --port 65536 --no-open\n");
    cli.wait_for(Pipe::Stderr, "could not start pp stats");
    cli.input("/stats --port 0\n");
    cli.wait_for(Pipe::Stderr, "/#/overview?range=24h");
    let text = cli.text(Pipe::Stderr);
    let url = format!(
        "http://{}",
        text.split("pp stats: http://")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
    );
    let address = url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    let (status, body) = http(address, "/api/stats/overview?range=24h", address);
    assert_eq!(status, 200);
    assert!(serde_json::from_str::<Value>(&body).is_ok());
    assert_eq!(http(address, "/", address).0, 200);
    cli.input("/stats --port 0\n/help\n");
    cli.wait_for(Pipe::Stderr, "Open the local statistics page");
    assert_eq!(cli.text(Pipe::Stderr).matches(&url).count(), 2);
    assert!(!home.path().join(".pp/agent/sessions").exists());
    assert!(!home.path().join("opened-url").exists());
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(std::net::TcpStream::connect(address).is_err());
}

#[test]
fn stats_startup_command_preserves_options() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stats_browser_fixture(dir.path());
    let output = Cli::spawn(
        dir.path(),
        &["/stats", "--host", "127.0.0.1", "--port", "0", "--no-open"],
        &[("PATH", bin.to_str().unwrap())],
    )
    .finish();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "startup command failed: {stderr}");
    assert!(
        stderr.contains("pp stats: http://127.0.0.1:"),
        "missing stats banner: {stderr}"
    );
    assert!(
        stderr.contains("/#/overview?range=24h"),
        "missing statistics page URL: {stderr}"
    );
    assert!(output.stdout.is_empty());
    assert!(!dir.path().join(".pp/agent/sessions").exists());
}

#[test]
#[cfg(unix)]
fn stats_server_opens_actual_url_and_survives_browser_failure() {
    for exit in ["0", "23"] {
        let dir = tempfile::tempdir().unwrap();
        let bin = stats_browser_fixture(dir.path());
        let mut pty = spawn_stats_server_pty(dir.path(), &bin, exit);
        let mut captured = Vec::new();
        pty_wait_for(
            &mut captured,
            &pty.output,
            "pp stats: http://",
            "stats banner",
        );
        let address = pty_stats_address(&captured);
        let expected = format!("1\nhttp://{address}/#/overview?range=24h\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if std::fs::read_to_string(dir.path().join("opened-url"))
                .ok()
                .as_deref()
                == Some(expected.as_str())
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "browser did not receive {expected}"
            );
            thread::sleep(Duration::from_millis(10));
        }
        if exit != "0" {
            pty_wait_for(
                &mut captured,
                &pty.output,
                "could not open browser",
                "browser failure warning",
            );
            let text = String::from_utf8_lossy(&captured);
            assert!(text.contains(&format!("http://{address}/#/overview?range=24h")));
        }
        assert_eq!(http(&address, "/", &address).0, 200);
        let _ = pty.child.kill();
    }
}

#[test]
#[cfg(unix)]
fn stats_server_without_terminal_skips_browser() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stats_browser_fixture(dir.path());
    let (_cli, address) = stats_endpoint(Cli::spawn(
        dir.path(),
        &["stats", "--port", "0"],
        &[("PATH", bin.to_str().unwrap())],
    ));
    assert_eq!(http(&address, "/", &address).0, 200);
    assert!(!dir.path().join("opened-url").exists());
}

#[test]
#[cfg(unix)]
fn stats_server_no_open_skips_browser() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stats_browser_fixture(dir.path());
    let (_cli, address) = stats_endpoint(Cli::spawn(
        dir.path(),
        &["stats", "--port", "0", "--no-open"],
        &[("PATH", bin.to_str().unwrap())],
    ));
    assert_eq!(http(&address, "/", &address).0, 200);
    assert!(!dir.path().join("opened-url").exists());
}

fn routed_private_ipv4() -> Option<String> {
    let socket = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // UDP connect only selects a route and local interface; it sends no packets.
    socket
        .connect((std::net::Ipv4Addr::new(192, 0, 2, 1), 80))
        .ok()?;
    let std::net::IpAddr::V4(ip) = socket.local_addr().ok()?.ip() else {
        return None;
    };
    ip.is_private().then(|| ip.to_string())
}

#[test]
fn stats_server_binds_explicit_ip_and_checks_its_origin() {
    let Some(host) = routed_private_ipv4() else {
        eprintln!("no routed private IPv4 interface; skipping explicit LAN bind test");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (_cli, address) = stats_endpoint(Cli::spawn(
        dir.path(),
        &["stats", "--host", &host, "--port", "0", "--no-open"],
        &[],
    ));
    assert!(address.starts_with(&format!("{host}:")));
    assert_eq!(http(&address, "/", &address).0, 200);

    let port = address.rsplit_once(':').unwrap().1;
    assert_eq!(http(&address, "/", &format!("127.0.0.1:{port}")).0, 403);
    assert_eq!(
        http_with_origin(&address, "/", &address, Some("http://untrusted.example")).0,
        403
    );
    assert_eq!(
        http_with_origin(&address, "/", &address, Some(&format!("http://{address}"))).0,
        200
    );
}

#[test]
fn stats_server_rejects_non_ip_and_unspecified_hosts() {
    let dir = tempfile::tempdir().unwrap();
    for host in ["localhost", "0.0.0.0", "::"] {
        let output = Cli::spawn(dir.path(), &["stats", "--host", host], &[]).finish();
        assert!(!output.status.success());
    }
}

#[test]
fn stats_server_starts_without_models_and_rejects_foreign_hosts() {
    let dir = tempfile::tempdir().unwrap();
    let (_cli, address) = start_stats_server(dir.path());
    let address = address.as_str();
    let (status, body) = http(address, "/api/stats/overview?range=24h", address);
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["summary"]["requests"],
        0
    );
    assert_eq!(http(address, "/", address).0, 200);
    assert_eq!(
        http(address, "/api/stats/overview?range=bad", address).0,
        400
    );
    assert_eq!(http(address, "/api/stats/requests/999", address).0, 404);
    assert_eq!(
        http(
            address,
            "/api/stats/requests?range=all",
            "untrusted.example"
        )
        .0,
        403
    );
    let bad = Cli::spawn(dir.path(), &["-p", "invalid", "stats"], &[]).finish();
    assert!(!bad.status.success());
}

#[test]
fn stats_style_persists_across_server_restart_without_models() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join(".pp/agent/settings.yml");
    let (cli, address) = start_stats_server(dir.path());
    let path = "/api/stats/style";
    assert_eq!(
        http(&address, path, &address),
        (200, r#"{"style":"default"}"#.into())
    );
    assert!(!settings.exists(), "读取默认风格不应创建配置");
    let origin = format!("http://{address}");
    let saved = raw_request_body(
        &address,
        "POST",
        path,
        &address,
        Some(&origin),
        Some(("application/json", br#"{"style":"modern"}"#)),
    );
    assert_eq!(saved.status, 200);
    stats_assert_security(&saved, "application/json");
    assert_eq!(
        pp_config::config::read_settings(&settings)
            .unwrap()
            .stats
            .style,
        pp_config::config::StatsStyle::Modern
    );
    assert!(!settings.with_file_name("models.yml").exists());
    drop(cli);
    let (_cli, address) = start_stats_server(dir.path());
    assert_eq!(
        http(&address, path, &address),
        (200, r#"{"style":"modern"}"#.into())
    );
    let original = "# 保留配置\ndebug: true # 调试\nstats:\n  style: modern\n";
    fs::write(&settings, original).unwrap();
    let origin = format!("http://{address}");
    let saved = raw_request_body(
        &address,
        "POST",
        path,
        &address,
        Some(&origin),
        Some(("application/json; charset=utf-8", br#"{"style":"default"}"#)),
    );
    assert_eq!(saved.status, 200);
    assert!(
        fs::read_to_string(&settings)
            .unwrap()
            .starts_with("# 保留配置\ndebug: true # 调试\n")
    );
    assert_eq!(
        http(&address, path, &address),
        (200, r#"{"style":"default"}"#.into())
    );
}

#[test]
fn stats_style_rejects_invalid_writes_and_preserves_broken_settings() {
    let dir = tempfile::tempdir().unwrap();
    let (_cli, address) = start_stats_server(dir.path());
    let path = "/api/stats/style";
    let origin = format!("http://{address}");
    let settings = dir.path().join(".pp/agent/settings.yml");
    let body = br#"{"style":"modern"}"#;
    for (host, source, kind, expected) in [
        (address.as_str(), None, "application/json", 403),
        (
            address.as_str(),
            Some("http://untrusted.example"),
            "application/json",
            403,
        ),
        (
            "untrusted.example",
            Some(origin.as_str()),
            "application/json",
            403,
        ),
        (address.as_str(), Some(origin.as_str()), "text/plain", 415),
        (
            address.as_str(),
            Some(origin.as_str()),
            "application/x-www-form-urlencoded",
            415,
        ),
    ] {
        let reply = raw_request_body(&address, "POST", path, host, source, Some((kind, body)));
        assert_eq!(reply.status, expected);
    }
    for body in [
        br#"{"style":"unknown"}"#.as_slice(),
        br#"{"style":null}"#,
        br#"{"style":42}"#,
        br#"{"style":"modern","debug":true}"#,
        br#"{"style":"default","style":"modern"}"#,
        b"{}",
        b"[]",
        b"invalid JSON",
    ] {
        let reply = raw_request_body(
            &address,
            "POST",
            path,
            &address,
            Some(&origin),
            Some(("application/json", body)),
        );
        assert_eq!(reply.status, 400);
    }
    let oversized = vec![b' '; 1025];
    assert_eq!(
        raw_request_body(
            &address,
            "POST",
            path,
            &address,
            Some(&origin),
            Some(("application/json", &oversized))
        )
        .status,
        413
    );
    assert_eq!(
        raw_request_body(
            &address,
            "POST",
            "/api/stats/style?style=modern",
            &address,
            Some(&origin),
            Some(("application/json", body))
        )
        .status,
        405
    );
    assert_eq!(
        http(&address, "/api/stats/style?style=modern", &address).0,
        404
    );
    assert!(!settings.exists());
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let broken = "unknown: secret-marker";
    fs::write(&settings, broken).unwrap();
    let reply = raw_request_body(
        &address,
        "POST",
        path,
        &address,
        Some(&origin),
        Some(("application/json", body)),
    );
    assert_eq!(reply.status, 503);
    assert!(
        !String::from_utf8(reply.body)
            .unwrap()
            .contains("secret-marker")
    );
    assert_eq!(fs::read_to_string(&settings).unwrap(), broken);
    assert_eq!(http(&address, path, &address).0, 503);
    assert_eq!(
        http(&address, "/api/stats/overview?range=24h", &address).0,
        200
    );
}

#[test]
fn stats_server_preserves_method_security_and_headers() {
    let dir = tempfile::tempdir().unwrap();
    let (_cli, address) = start_stats_server(dir.path());
    let host = address.as_str();

    // 未知合法 GET 仍是原来的 JSON 404。
    let missing = raw_request(host, "GET", "/api/stats/missing", host, None);
    assert_eq!(missing.status, 404);
    assert_eq!(missing.body, br#"{"error":"request or page not found"}"#);
    stats_assert_security(&missing, "application/json");

    // 合法 Host 的 POST 与 HEAD 都按既有策略返回 405；HEAD 按 HTTP 语义不带正文。
    for method in ["POST", "HEAD"] {
        let reply = raw_request(host, method, "/api/stats/overview?range=24h", host, None);
        assert_eq!(reply.status, 405, "{method}");
        if method == "POST" {
            assert_eq!(reply.body, br#"{"error":"GET required"}"#);
        }
        stats_assert_security(&reply, "application/json");
    }

    // Host 检查先于方法：外来 Host 的 POST 仍是 403。
    let foreign = raw_request(
        host,
        "POST",
        "/api/stats/overview",
        "untrusted.example",
        None,
    );
    assert_eq!(foreign.status, 403);
    assert_eq!(foreign.body, br#"{"error":"allowed origin required"}"#);
    stats_assert_security(&foreign, "application/json");

    // 所有内嵌文本资源可访问，响应头保持一致。
    for (path, kind) in [
        ("/", "text/html"),
        ("/index.html", "text/html"),
        ("/stats.css", "text/css"),
        ("/ui.css", "text/css"),
        ("/traces.css", "text/css"),
        ("/modern.css", "text/css"),
        ("/stats.js", "text/javascript"),
        ("/activity.js", "text/javascript"),
        ("/insights.js", "text/javascript"),
        ("/traces.js", "text/javascript"),
        ("/context.js", "text/javascript"),
        ("/icon.svg", "image/svg+xml"),
    ] {
        let reply = raw_request(host, "GET", path, host, None);
        assert_eq!(reply.status, 200, "{path}");
        assert!(!reply.body.is_empty(), "{path}");
        stats_assert_security(&reply, kind);
    }

    // 字体按原始二进制返回，不做 UTF-8 转码。
    let font = raw_request(host, "GET", "/fonts/InterVariable.woff2", host, None);
    assert_eq!(font.status, 200);
    assert_eq!(
        font.body,
        include_bytes!("../../../crates/pp-stats/src/fonts/InterVariable.woff2")
    );
    stats_assert_security(&font, "font/woff2");

    // 静态资源仍是精确 request-target 匹配。
    assert_eq!(
        raw_request(host, "GET", "/stats.js?x=1", host, None).status,
        404
    );
}
