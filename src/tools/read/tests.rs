use super::*;
use std::io::Cursor;

fn run_text(path: &str, cwd: &Path, limits: ToolLimits, mode: EditMode) -> Value {
    run(
        path,
        cwd,
        limits,
        mode,
        &mut WebCache::default(),
        &mut |_, _| unreachable!(),
        &CancellationToken::new(),
    )
    .unwrap()
    .value
}

#[test]
fn read_selectors_are_exact_merge_ranges_and_preserve_newlines() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("file"),
        "中\r\ntwo\r\nthree\r\nfour\r\nlast",
    )
    .unwrap();
    for (suffix, expected) in [
        (":2-3", "two\r\nthree\r\n"),
        (":2+2", "two\r\nthree\r\n"),
        (":L2..L3", "two\r\nthree\r\n"),
        (":-2", "four\r\nlast"),
        (":2", "two\r\nthree\r\nfour\r\nlast"),
        (":2:raw", "two\r\n"),
        (":raw:2-3", "two\r\nthree\r\n"),
        (":2-:raw", "two\r\nthree\r\nfour\r\nlast"),
        (":4,2-3,1-2", "中\r\ntwo\r\nthree\r\nfour\r\n"),
        (":1,-2", "中\r\nfour\r\nlast"),
        (":99-100", ""),
    ] {
        let value = run_text(
            &format!("file{suffix}"),
            dir.path(),
            ToolLimits::default(),
            EditMode::Replace,
        );
        assert_eq!(value["content"], expected, "{suffix}");
        assert_eq!(value["truncated"], false, "{suffix}");
    }
    let anchored = run_text(
        "file:2,4",
        dir.path(),
        ToolLimits::default(),
        EditMode::Hashline,
    );
    assert!(anchored["content"].as_str().unwrap().starts_with("2:"));
    assert!(
        anchored["content"]
            .as_str()
            .unwrap()
            .lines()
            .nth(1)
            .unwrap()
            .starts_with("4:")
    );
    assert_eq!(
        run_text(
            "file:2:raw",
            dir.path(),
            ToolLimits::default(),
            EditMode::Hashline
        )["content"],
        "two\r\n"
    );
}

#[test]
fn read_selectors_validate_literals_and_url_queries() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file:20-30"), "literal").unwrap();
    assert_eq!(
        run_text(
            "file:20-30",
            dir.path(),
            ToolLimits::default(),
            EditMode::Replace
        )["content"],
        "literal"
    );
    for suffix in [
        ":0",
        ":-0",
        ":2-1",
        ":1+0",
        ":1,,2",
        ":999999999999999999999999",
        ":2+18446744073709551615",
    ] {
        assert!(
            Target::parse(&format!("missing{suffix}"), dir.path()).is_err(),
            "{suffix}"
        );
    }
    let url = Target::parse(
        "https://example.com:8443/page:2-5?q=text:20#x:3",
        dir.path(),
    )
    .unwrap();
    assert_eq!(url.source, "https://example.com:8443/page?q=text:20#x:3");
    assert_eq!(
        url.next_path(&[(3, 5)]),
        "https://example.com:8443/page:3-5?q=text:20#x:3"
    );
    assert_eq!(
        Target::parse("https://example.com:8443", dir.path())
            .unwrap()
            .source,
        "https://example.com:8443"
    );
}

#[test]
fn read_multirange_next_path_never_loses_pending_ranges() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file"), "a\nb\nc\nd\ne\nf\ng\n").unwrap();
    let limits = ToolLimits {
        read_max_lines: 2,
        read_max_bytes: 4,
        ..Default::default()
    };
    let first = run_text("file:1-3,5-7:raw", dir.path(), limits, EditMode::Hashline);
    assert_eq!(first["content"], "a\nb\n");
    assert_eq!(first["next_path"], "file:3-3,5-7:raw");
    let second = run_text(
        first["next_path"].as_str().unwrap(),
        dir.path(),
        limits,
        EditMode::Hashline,
    );
    assert_eq!(second["content"], "c\ne\n");
    assert_eq!(second["next_path"], "file:6-7:raw");
    let third = run_text(
        second["next_path"].as_str().unwrap(),
        dir.path(),
        limits,
        EditMode::Hashline,
    );
    assert_eq!(third["content"], "f\ng\n");
    assert_eq!(third["next_path"], Value::Null);
    // 第二个区间放不进当前页时，保留整个区间供下一页读取。
    std::fs::write(dir.path().join("file"), "a\nskip\nlong\n").unwrap();
    let first = run_text(
        "file:1,3",
        dir.path(),
        ToolLimits {
            read_max_bytes: 5,
            ..Default::default()
        },
        EditMode::Replace,
    );
    assert_eq!(first["content"], "a\n");
    assert_eq!(first["next_path"], "file:3-3");
    assert_eq!(
        run_text(
            first["next_path"].as_str().unwrap(),
            dir.path(),
            ToolLimits {
                read_max_bytes: 5,
                ..Default::default()
            },
            EditMode::Replace
        )["content"],
        "long\n"
    );
}

#[test]
fn read_routes_local_and_url_images_through_the_same_validation_and_encoding() {
    let dir = tempfile::tempdir().unwrap();
    for format in [
        ::image::ImageFormat::Png,
        ::image::ImageFormat::Jpeg,
        ::image::ImageFormat::WebP,
        ::image::ImageFormat::Gif,
    ] {
        let mut bytes = Cursor::new(Vec::new());
        ::image::DynamicImage::new_rgb8(2, 3)
            .write_to(&mut bytes, format)
            .unwrap();
        let bytes = bytes.into_inner();
        // 不依赖扩展名或 HTTP MIME，真实图片内容仍应进入图片处理流程。
        std::fs::write(dir.path().join("payload.dat"), &bytes).unwrap();
        let local = run(
            "payload.dat",
            dir.path(),
            ToolLimits::default(),
            EditMode::Hashline,
            &mut WebCache::default(),
            &mut |_, _| panic!("本地读取不应抓取 URL"),
            &CancellationToken::new(),
        )
        .unwrap();
        let remote = run(
            "https://example.com/payload.dat",
            dir.path(),
            ToolLimits::default(),
            EditMode::Hashline,
            &mut WebCache::default(),
            &mut |_, _| {
                Ok(HttpResponse {
                    bytes: bytes.clone(),
                    final_url: "https://example.com/final".into(),
                    content_type: "application/octet-stream".into(),
                })
            },
            &CancellationToken::new(),
        )
        .unwrap();
        for result in [&local, &remote] {
            assert_eq!(result.value["kind"], "image");
            assert_eq!(result.value["width"], 2);
            assert_eq!(result.value["height"], 3);
            assert_eq!(result.value["bytes"], bytes.len());
            assert_eq!(result.images.len(), 1);
            assert!(!result.value.to_string().contains("base64"));
        }
        assert_eq!(local.value["mime_type"], remote.value["mime_type"]);
        assert_eq!(local.images[0].data_url, remote.images[0].data_url);
        if format == ::image::ImageFormat::Gif {
            assert_eq!(local.value["mime_type"], "image/png");
        }
    }
}

#[test]
fn read_routes_text_and_rejects_invalid_image_hints_without_text_fallback() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("source.txt"), "one\r\ntwo\r\n").unwrap();
    let local = run(
        "source.txt:raw",
        dir.path(),
        ToolLimits::default(),
        EditMode::Hashline,
        &mut WebCache::default(),
        &mut |_, _| panic!("本地读取不应抓取 URL"),
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(local.value["content"], "one\r\ntwo\r\n");
    assert!(local.images.is_empty());
    let mut fetch = |_: &str, _: &CancellationToken| {
        Ok(HttpResponse {
            bytes: b"one\r\ntwo\r\n".to_vec(),
            final_url: "https://example.com/final.txt".into(),
            content_type: "text/plain".into(),
        })
    };
    let remote = run(
        "https://example.com/source.txt",
        dir.path(),
        ToolLimits::default(),
        EditMode::Hashline,
        &mut WebCache::default(),
        &mut fetch,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(remote.value["content"], local.value["content"]);
    assert_eq!(remote.value["kind"], "url");
    assert!(remote.images.is_empty());

    std::fs::write(dir.path().join("invalid.PNG"), "plain text").unwrap();
    for path in ["invalid.PNG", "https://example.com/image"] {
        let error = run(
            path,
            dir.path(),
            ToolLimits::default(),
            EditMode::Replace,
            &mut WebCache::default(),
            &mut |_, _| {
                Ok(HttpResponse {
                    bytes: b"plain text".to_vec(),
                    final_url: "https://example.com/image".into(),
                    content_type: "image/png".into(),
                })
            },
            &CancellationToken::new(),
        )
        .err()
        .unwrap();
        assert_eq!(error.to_string(), "unsupported image format");
    }
}

#[test]
fn read_rejects_image_selectors_for_both_local_and_url_targets() {
    let dir = tempfile::tempdir().unwrap();
    // 即使图片损坏，选择器错误仍先于解码错误返回。
    std::fs::write(dir.path().join("image.png"), "invalid image").unwrap();
    for source in ["image.png", "https://example.com/image"] {
        for suffix in [":1", ":raw", ":-1", ":1-2:raw"] {
            let error = run(
                &format!("{source}{suffix}"),
                dir.path(),
                ToolLimits::default(),
                EditMode::Replace,
                &mut WebCache::default(),
                &mut |_, _| {
                    Ok(HttpResponse {
                        bytes: b"invalid image".to_vec(),
                        final_url: "https://example.com/image".into(),
                        content_type: "image/png".into(),
                    })
                },
                &CancellationToken::new(),
            )
            .err()
            .unwrap();
            assert_eq!(error.to_string(), "image reads do not accept selectors");
        }
    }
}
