use super::*;

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
