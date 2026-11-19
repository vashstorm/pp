use super::*;
use crate::web_search::providers::{Endpoints, duckduckgo, public, startpage};
use crate::web_search::query::{
    GOOGLE_QUERY_SYNTAX, apply_query_constraints, format_scraper_query, parse_search_query,
};
use crate::web_search::{Recency, SearchSource, WebSearchRequest, execute_search_with_endpoints};
use axum::http::header::AUTHORIZATION;
use pp_config::config::{SearxngSettings, WebSearchProvider, WebSearchSettings};
use pp_core::cancellation::CancellationToken;
use pp_test_support::TestServer;
use std::time::Duration;

fn no_proxy_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .unwrap()
}

#[test]
fn query_parser_scraper_downgrade_and_lenient_constraints() {
    let parsed = parse_search_query(
        r#"site:github.com/rust-lang -site:spam.example inurl:releases intitle:"async runtime" filetype:md after:2025-01-01 before:2027-01-01 (tokio OR smol) -legacy"#,
    );
    assert_eq!(parsed.sites, vec!["github.com/rust-lang"]);
    assert_eq!(parsed.excluded_sites, vec!["spam.example"]);
    assert_eq!(parsed.in_url, vec!["releases"]);
    assert_eq!(parsed.in_title, vec!["async runtime"]);
    assert_eq!(parsed.filetypes, vec!["md"]);
    assert_eq!(parsed.after.as_deref(), Some("2025-01-01"));
    assert_eq!(parsed.before.as_deref(), Some("2027-01-01"));
    assert!(parsed.has_constraints);

    let scraper = format_scraper_query(&parsed.raw, &parsed, GOOGLE_QUERY_SYNTAX);
    assert!(!scraper.contains("site:github.com/rust-lang"), "{scraper}");
    assert!(!scraper.contains("inurl:releases"), "{scraper}");
    assert!(scraper.contains("github.com/rust-lang"), "{scraper}");
    assert!(scraper.contains("releases"), "{scraper}");
    assert!(scraper.contains("-site:spam.example"), "{scraper}");

    let sources = vec![
        SearchSource {
            title: "Async Runtime Notes".into(),
            url: "https://github.com/rust-lang/book/releases/notes.md".into(),
            snippet: None,
            published_date: Some("2026-03-10".into()),
            age_seconds: None,
            author: None,
        },
        SearchSource {
            title: "Spam".into(),
            url: "https://spam.example/releases/notes.md".into(),
            snippet: None,
            published_date: Some("2026-03-10".into()),
            age_seconds: None,
            author: None,
        },
        SearchSource {
            title: "Async Runtime Overflow".into(),
            url: "https://github.com/rust-lang/book/releases/overflow.md".into(),
            snippet: None,
            published_date: Some("9999999999999 years ago".into()),
            age_seconds: None,
            author: None,
        },
    ];
    let filtered = apply_query_constraints(&sources, &parsed);
    assert!(filtered.dropped.is_empty(), "{:?}", filtered.dropped);
    assert_eq!(filtered.sources.len(), 2);
    assert_eq!(filtered.sources[0].title, "Async Runtime Notes");

    // 某维约束会清空结果时应放宽并保留其余约束与提示标签。
    let impossible = parse_search_query("site:docs.rs intitle:Async");
    let relaxed = apply_query_constraints(&sources[..1], &impossible);
    assert_eq!(relaxed.sources.len(), 1);
    assert_eq!(relaxed.dropped, vec!["site:docs.rs"]);
}

#[test]
fn startpage_handshake_and_duckduckgo_pagination_unwrap_work() {
    let sp_server = Arc::new(TestServer::start().unwrap());
    let sp_addr = sp_server.addr();
    let sp_worker = {
        let server = Arc::clone(&sp_server);
        std::thread::spawn(move || {
            let home_req = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(home_req.method(), "GET");
            assert_eq!(home_req.target(), "/");
            home_req
                .respond(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(
                            r#"<html><body><form action="/sp/search"><input type="hidden" name="sc" value="tok-123"><input type="hidden" name="cat" value="web"></form></body></html>"#,
                        ))
                        .unwrap(),
                )
                .unwrap();

            let search_req = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(search_req.method(), "POST");
            assert_eq!(search_req.target(), "/sp/search");
            assert!(search_req.headers().get("referer").is_some());
            let form = String::from_utf8_lossy(search_req.body());
            assert!(form.contains("sc=tok-123"), "{form}");
            assert!(form.contains("with_date=w"), "{form}");
            search_req
                .respond(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(
                            r#"<html><body>
                            <div class="result">
                                <div><a class="result-link" href="https://example.com/a"><h2>Alpha Guide</h2></a></div>
                                <p class="description">First snippet</p>
                            </div>
                            <div class="result">
                                <a class="result-link" href="https://www.startpage.com/ad"><h2>Internal Ad</h2></a>
                                <p class="description">Skip me</p>
                            </div>
                            </body></html>"#,
                        ))
                        .unwrap(),
                )
                .unwrap();
        })
    };

    let ddg_server = Arc::new(TestServer::start().unwrap());
    let ddg_addr = ddg_server.addr();
    let ddg_worker = {
        let server = Arc::clone(&ddg_server);
        std::thread::spawn(move || {
            let first = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(first.method(), "POST");
            let form = String::from_utf8_lossy(first.body());
            assert!(form.contains("df=w"), "{form}");
            first
                .respond(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(
                            r#"<html><body>
                            <div class="result">
                                <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa">Alpha from DDG</a>
                                <a class="result__snippet">Much longer snippet from DDG for Alpha</a>
                                <div class="result__extras__url"><span>2026-01-02</span></div>
                            </div>
                            <div class="result">
                                <a class="result__a" href="https://duckduckgo.com/y.js?ad=1">Sponsored</a>
                            </div>
                            <form action="/html/" method="post">
                                <input type="hidden" name="s" value="10">
                                <input type="hidden" name="vqd" value="vqd-next">
                            </form>
                            </body></html>"#,
                        ))
                        .unwrap(),
                )
                .unwrap();

            let second = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(second.method(), "POST");
            let form = String::from_utf8_lossy(second.body());
            assert!(form.contains("vqd=vqd-next"), "{form}");
            second
                .respond(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(
                            r#"<html><body>
                            <div class="result">
                                <a class="result__a" href="https://example.org/b">Beta Page</a>
                                <div class="result__snippet">Second page snippet</div>
                            </div>
                            </body></html>"#,
                        ))
                        .unwrap(),
                )
                .unwrap();
        })
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = no_proxy_client();
    let endpoints = Endpoints {
        startpage_home: format!("http://{sp_addr}/"),
        startpage_search: format!("http://{sp_addr}/sp/search"),
        duckduckgo_html: format!("http://{ddg_addr}/html/"),
        ..Default::default()
    };
    let settings = WebSearchSettings::default();
    let req = WebSearchRequest {
        query: "rust async".into(),
        recency: Some(Recency::Week),
        limit: Some(2),
        provider: Some(WebSearchProvider::Public),
    };
    let res = rt
        .block_on(execute_search_with_endpoints(
            &client,
            &req,
            &settings,
            true,
            &endpoints,
            &CancellationToken::new(),
        ))
        .unwrap();
    sp_worker.join().unwrap();
    ddg_worker.join().unwrap();

    assert_eq!(res.provider, "public");
    assert_eq!(res.sources.len(), 2);
    // https://example.com/a 在两个引擎都命中，排在第一位且保留较长 snippet。
    assert_eq!(res.sources[0].url, "https://example.com/a");
    assert_eq!(
        res.sources[0].snippet.as_deref(),
        Some("Much longer snippet from DDG for Alpha")
    );
    assert_eq!(res.sources[1].url, "https://example.org/b");
    assert!(res.formatted_text.contains("[1] Alpha Guide"));
}

#[test]
fn public_fanout_returns_on_soft_deadline_when_one_engine_is_slow() {
    let fast_sp = Arc::new(TestServer::start().unwrap());
    let sp_addr = fast_sp.addr();
    let sp_worker = {
        let server = Arc::clone(&fast_sp);
        std::thread::spawn(move || {
            let home = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            home.respond(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from("<html/>"))
                    .unwrap(),
            )
            .unwrap();
            let search = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            search
                .respond(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(
                            r#"<div class="result"><a class="result-link" href="https://fast.example/1"><h2>Fast Hit</h2></a><p class="description">Quick</p></div>"#,
                        ))
                        .unwrap(),
                )
                .unwrap();
        })
    };

    let slow_ddg = Arc::new(TestServer::start().unwrap());
    let ddg_addr = slow_ddg.addr();
    let ddg_worker = {
        let server = Arc::clone(&slow_ddg);
        std::thread::spawn(move || {
            if let Ok(Some(req)) = server.recv_timeout(Duration::from_secs(5)) {
                std::thread::sleep(Duration::from_millis(600));
                let _ = req.respond(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(
                            r#"<div class="result"><a class="result__a" href="https://slow.example/2">Slow Hit</a></div>"#,
                        ))
                        .unwrap(),
                );
            }
        })
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = no_proxy_client();
    let endpoints = Endpoints {
        startpage_home: format!("http://{sp_addr}/"),
        startpage_search: format!("http://{sp_addr}/sp/search"),
        duckduckgo_html: format!("http://{ddg_addr}/html/"),
        ..Default::default()
    };
    let parsed = parse_search_query("rust");
    let ctx = crate::web_search::providers::SearchContext {
        client: &client,
        timeout: Duration::from_secs(5),
        searxng: None,
        endpoints: &endpoints,
    };
    let started = std::time::Instant::now();
    let resp = rt
        .block_on(public::search_with_deadlines(
            &ctx,
            "rust",
            &parsed,
            None,
            Some(5),
            Duration::from_millis(80),
            Duration::from_secs(2),
        ))
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(450));
    assert_eq!(resp.sources.len(), 1);
    assert_eq!(resp.sources[0].url, "https://fast.example/1");
    sp_worker.join().unwrap();
    ddg_worker.join().unwrap();

    // 验证 html 解析函数对异常路径与广告的过滤。
    let sp_bad = startpage::parse_html_results(
        "<div class='result'><a class='result-link' href='/relative'><h2>Local</h2></a></div>",
        "https://www.startpage.com/",
    );
    assert!(sp_bad.is_empty());
    let ddg_bad = duckduckgo::parse_html_results(
        r#"<div class="result"><a class="result__a" href="https://duckduckgo.com/y.js?u=1">Ad</a></div>"#,
    );
    assert!(ddg_bad.is_empty());
}

#[test]
fn public_reports_startpage_challenge_when_duckduckgo_has_no_results() {
    let server = TestServer::start().unwrap();
    let base = format!("http://{}", server.addr());
    let endpoints = Endpoints {
        startpage_home: format!("{base}/"),
        startpage_search: format!("{base}/sp/search"),
        duckduckgo_html: format!("{base}/html/"),
        ..Default::default()
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            for _ in 0..3 {
                let req = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let html = match req.target().split('?').next().unwrap() {
                    "/" | "/html/" => "<html></html>",
                    "/sp/search" => {
                        r#"<script id="anubis_challenge" type="application/json">{}</script>"#
                    }
                    target => panic!("unexpected search request: {target}"),
                };
                req.respond(Response::new(Body::from(html))).unwrap();
            }
        });
        runtime.block_on(execute_search_with_endpoints(
            &no_proxy_client(),
            &WebSearchRequest {
                query: "rust".into(),
                recency: None,
                limit: None,
                provider: Some(WebSearchProvider::Public),
            },
            &WebSearchSettings::default(),
            true,
            &endpoints,
            &CancellationToken::new(),
        ))
    });

    let error = result.unwrap_err().to_string();
    assert!(error.contains("Startpage blocked the request"), "{error}");
    assert!(error.contains("DuckDuckGo returned no results"), "{error}");
}

#[test]
fn auto_routes_hackernews_and_searxng_with_fallback_and_auth_redaction() {
    let hn_server = Arc::new(TestServer::start().unwrap());
    let hn_addr = hn_server.addr();
    let hn_worker = {
        let server = Arc::clone(&hn_server);
        std::thread::spawn(move || {
            for _ in 0..2 {
                let req = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let body = if req.target().contains("tags=story") {
                    r#"{"hits":[{"objectID":"101","title":"Rust 1.88","url":"https://blog.rust-lang.org/","points":420,"num_comments":99,"author":"alice","created_at":"2026-01-01T00:00:00Z"}]}"#
                } else {
                    r#"{"hits":[{"objectID":"202","comment_text":"Great <i>release</i>!","story_title":"Rust 1.88","author":"bob","created_at":"2026-01-02T00:00:00Z"}]}"#
                };
                req.respond(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(body))
                        .unwrap(),
                )
                .unwrap();
            }
        })
    };

    let searxng_server = Arc::new(TestServer::start().unwrap());
    let sx_addr = searxng_server.addr();
    let sx_worker = {
        let server = Arc::clone(&searxng_server);
        std::thread::spawn(move || {
            let cfg_req = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(cfg_req.target(), "/config");
            cfg_req
                .respond(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(
                            r#"{"engines":[{"name":"duckduckgo","shortcut":"ddg"}]}"#,
                        ))
                        .unwrap(),
                )
                .unwrap();

            let search_req = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(
                search_req
                    .headers()
                    .get(AUTHORIZATION)
                    .and_then(|v| v.to_str().ok()),
                Some("Basic dXNlcjpzZWNyZXQtdGVzdC1wYXNz")
            );
            assert!(
                search_req.target().contains("engines=duckduckgo"),
                "{}",
                search_req.target()
            );
            assert!(
                !search_req.target().contains("%21%21g"),
                "{}",
                search_req.target()
            );
            search_req
                .respond(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(
                            r#"{"answers":["42"],"results":[{"title":"SearXNG Result","url":"https://docs.rs/tokio","content":"Tokio docs","published_date":"2026-03-10"}]}"#,
                        ))
                        .unwrap(),
                )
                .unwrap();
        })
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = no_proxy_client();
    let endpoints = Endpoints {
        hackernews_search: format!("http://{hn_addr}/api/v1/search"),
        hackernews_item_prefix: "https://news.ycombinator.com/item?id=".into(),
        ..Default::default()
    };

    // 1. HN 专属查询自动优先 Hacker News。
    let hn_req = WebSearchRequest {
        query: "site:news.ycombinator.com rust".into(),
        recency: None,
        limit: Some(2),
        provider: None,
    };
    let hn_res = rt
        .block_on(execute_search_with_endpoints(
            &client,
            &hn_req,
            &WebSearchSettings::default(),
            true,
            &endpoints,
            &CancellationToken::new(),
        ))
        .unwrap();
    hn_worker.join().unwrap();
    assert_eq!(hn_res.provider, "hackernews");
    assert_eq!(hn_res.sources.len(), 2);
    assert_eq!(
        hn_res.sources[0].url,
        "https://news.ycombinator.com/item?id=101"
    );
    assert!(
        hn_res.sources[1]
            .snippet
            .as_deref()
            .is_some_and(|s| s.contains("Great release!")),
        "{:?}",
        hn_res.sources[1]
    );

    // 2. 配置 SearXNG 后优先走 SearXNG（并验证 sandbox 断网时本地 loopback SearXNG 仍放行）。
    let searx_settings = WebSearchSettings {
        searxng: Some(SearxngSettings {
            endpoint: format!("http://{sx_addr}"),
            token: None,
            basic_username: Some("user".into()),
            basic_password: Some("secret-test-pass".into()),
            engines: Some("ddg".into()),
            categories: None,
            language: None,
            safesearch: None,
        }),
        ..Default::default()
    };
    let sx_req = WebSearchRequest {
        query: "!!g tokio docs".into(),
        recency: Some(Recency::Week),
        limit: Some(5),
        provider: None,
    };
    let sx_res = rt
        .block_on(execute_search_with_endpoints(
            &client,
            &sx_req,
            &searx_settings,
            false,
            &endpoints,
            &CancellationToken::new(),
        ))
        .unwrap();
    sx_worker.join().unwrap();
    assert_eq!(sx_res.provider, "searxng");
    assert_eq!(sx_res.answer.as_deref(), Some("42"));
    assert_eq!(sx_res.sources[0].url, "https://docs.rs/tokio");
    assert_eq!(
        sx_res.sources[0].published_date.as_deref(),
        Some("2026-03-10")
    );

    // 3. 当非 loopback SearXNG 遇到断网沙盒时必须拒绝。
    let remote_searx = WebSearchSettings {
        searxng: Some(SearxngSettings {
            endpoint: "https://searx.remote.example".into(),
            token: None,
            basic_username: None,
            basic_password: None,
            engines: None,
            categories: None,
            language: None,
            safesearch: None,
        }),
        ..Default::default()
    };
    let denied = rt
        .block_on(execute_search_with_endpoints(
            &client,
            &sx_req,
            &remote_searx,
            false,
            &endpoints,
            &CancellationToken::new(),
        ))
        .unwrap_err();
    assert!(denied.to_string().contains("sandbox"), "{denied}");
}
