use super::*;
use crate::config::{SettingPatch, update_general_setting};

#[test]
fn web_search_settings_defaults_and_strict_validation() {
    let settings: Settings = yaml::parse("{}", "settings").unwrap();
    settings.validate().unwrap();
    assert!(settings.web_search.enabled);
    assert_eq!(settings.web_search.provider, WebSearchProvider::Auto);
    assert!(settings.web_search.keyless);
    assert_eq!(settings.web_search.timeout_seconds, 60);
    assert!(settings.web_search.searxng.is_none());

    let parsed: Settings = yaml::parse(
        r#"
webSearch:
  enabled: true
  provider: searxng
  keyless: false
  timeoutSeconds: 45
  searxng:
    endpoint: https://searx.example.org
    token: secret-token-marker
    engines: duckduckgo, startpage
    categories: general
    language: en-US
    safesearch: 1
"#,
        "settings",
    )
    .unwrap();
    parsed.validate().unwrap();
    let searxng = parsed.web_search.searxng.as_ref().unwrap();
    let debug = format!("{searxng:?}");
    assert!(!debug.contains("secret-token-marker"), "{debug}");
    assert!(debug.contains("[REDACTED]"), "{debug}");
    assert!(!searxng.is_loopback_endpoint());

    for local in [
        "http://127.0.0.1:8080",
        "http://localhost:8888",
        "http://[::1]:8080",
    ] {
        let cfg = SearxngSettings {
            endpoint: local.to_owned(),
            token: None,
            basic_username: None,
            basic_password: None,
            engines: None,
            categories: None,
            language: None,
            safesearch: None,
        };
        cfg.validate().unwrap();
        assert!(cfg.is_loopback_endpoint(), "{local}");
    }

    for invalid in [
        "webSearch: {unknown: true}",
        "webSearch: {provider: brave}",
        "webSearch: {timeoutSeconds: 0}",
        "webSearch: {timeoutSeconds: 301}",
        "webSearch: {searxng: {endpoint: ftp://searx.local}}",
        "webSearch: {searxng: {endpoint: https://user:pass@searx.local}}",
        "webSearch: {searxng: {endpoint: https://searx.local/search?q=1}}",
        "webSearch: {searxng: {endpoint: https://searx.local, basicUsername: alice}}",
        "webSearch: {searxng: {endpoint: https://searx.local, basicUsername: 'a:b', basicPassword: pw}}",
        "webSearch: {searxng: {endpoint: https://searx.local, safesearch: 3}}",
    ] {
        let loaded = yaml::parse::<Settings>(invalid, "settings").and_then(|s| {
            s.validate()?;
            Ok(s)
        });
        assert!(loaded.is_err(), "{invalid}");
    }
}

#[test]
fn web_search_setting_patches_persist_and_validate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.yml");
    fs::write(
        &path,
        "# keep\nwebSearch:\n  searxng:\n    endpoint: http://127.0.0.1:8080\n",
    )
    .unwrap();

    let saved = update_general_setting(
        &path,
        SettingPatch::WebSearchProvider(WebSearchProvider::Searxng),
    )
    .unwrap();
    assert_eq!(saved.web_search.provider, WebSearchProvider::Searxng);
    assert_eq!(
        saved.web_search.searxng.as_ref().unwrap().endpoint,
        "http://127.0.0.1:8080"
    );

    let saved = update_general_setting(&path, SettingPatch::WebSearchEnabled(false)).unwrap();
    assert!(!saved.web_search.enabled);
    let saved = update_general_setting(&path, SettingPatch::WebSearchKeyless(false)).unwrap();
    assert!(!saved.web_search.keyless);
    let saved = update_general_setting(&path, SettingPatch::WebSearchTimeoutSeconds(15)).unwrap();
    assert_eq!(saved.web_search.timeout_seconds, 15);

    assert!(update_general_setting(&path, SettingPatch::WebSearchTimeoutSeconds(0)).is_err());
    assert!(update_general_setting(&path, SettingPatch::WebSearchTimeoutSeconds(301)).is_err());
}
