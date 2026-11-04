use super::*;
use std::io::Cursor;

fn run_text(path: &str, cwd: &Path, limits: ToolLimits, mode: EditMode) -> Value {
    run(
        path,
        cwd,
        limits,
        mode,
        &pp_config::skills::Skills::default().shared(),
        &mut WebCache::default(),
        &mut |_, _| unreachable!(),
        &CancellationToken::new(),
    )
    .unwrap()
    .value
}

#[test]
fn skill_read_full_text_selectors_resources_and_live_enablement() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".pp/agent");
    let root = dir.join("skills/rust");
    std::fs::create_dir_all(root.join("references/empty")).unwrap();
    let body = "---\nname: rust\ndescription: Rust guide\n---\na long single line exceeding eight bytes\nsecond\nBODY-END\n";
    std::fs::write(root.join("SKILL.md"), body).unwrap();
    std::fs::write(root.join("references/check.md"), body).unwrap();
    std::fs::write(root.join(".hidden"), "hidden").unwrap();
    std::os::unix::fs::symlink(root.join("references"), root.join("link")).unwrap();
    ::image::DynamicImage::new_rgb8(2, 3)
        .save(root.join("picture.png"))
        .unwrap();
    let skills = pp_config::skills::Skills::from_dir(&dir).shared();
    // 同名真实路径不能把虚拟技能 URI 或选择器劫持为本地读取。
    std::fs::create_dir_all(home.path().join("skill:")).unwrap();
    std::fs::write(home.path().join("skill:/rust:2-3:raw"), "wrong local file").unwrap();
    let limits = ToolLimits {
        read_max_lines: 2,
        read_max_bytes: 8,
        ..Default::default()
    };
    let read = |path: &str, mode| {
        run(
            path,
            home.path(),
            limits,
            mode,
            &skills,
            &mut WebCache::default(),
            &mut |_, _| panic!("技能读取不得访问网络"),
            &CancellationToken::new(),
        )
    };
    for path in [
        "skill://rust",
        "skill://rust/",
        "skill://rust/references/check.md",
    ] {
        let value = read(path, EditMode::Replace).unwrap().value;
        assert_eq!(value["content"], body);
        assert_eq!(value["source"], path);
        assert_eq!(value["truncated"], false);
        assert_eq!(
            value["base_dir"],
            root.canonicalize().unwrap().to_str().unwrap()
        );
        assert!(value["next_path"].is_null());
    }
    assert_eq!(
        read("skill://rust:2-3:raw", EditMode::Hashline)
            .unwrap()
            .value["content"],
        "name: rust\ndescription: Rust guide\n"
    );
    let value = read("skill://rust:2,6-7", EditMode::Hashline)
        .unwrap()
        .value;
    let lines: Vec<_> = value["content"].as_str().unwrap().lines().collect();
    for (line, expected) in lines.iter().zip(["2:", "6:", "7:"]) {
        assert!(line.starts_with(expected));
    }
    assert_eq!(lines.len(), 3);
    assert!(
        read("skill://rust:-1", EditMode::Hashline).unwrap().value["content"]
            .as_str()
            .unwrap()
            .starts_with("7:")
    );
    std::fs::write(home.path().join("ordinary"), body.lines().nth(4).unwrap()).unwrap();
    assert!(
        read("ordinary", EditMode::Replace)
            .err()
            .unwrap()
            .to_string()
            .contains("exceeds read_max_bytes")
    );
    assert_eq!(
        read("skill://rust/.", EditMode::Hashline).unwrap().value["content"],
        "references/\n.hidden\nSKILL.md\nlink\npicture.png\n"
    );
    assert_eq!(
        read("skill://rust/references", EditMode::Hashline)
            .unwrap()
            .value["content"],
        "empty/\ncheck.md\n"
    );
    assert_eq!(
        read("skill://rust/references/empty", EditMode::Hashline)
            .unwrap()
            .value["content"],
        "(empty directory)"
    );
    assert!(read(root.to_str().unwrap(), EditMode::Replace).is_err());
    let image = read("skill://rust/picture.png", EditMode::Replace).unwrap();
    assert_eq!(image.value["kind"], "image");
    assert_eq!(image.value["source"], "skill://rust/picture.png");
    assert_eq!(image.images.len(), 1);
    assert_eq!(
        read("skill://rust/picture.png:1", EditMode::Replace)
            .err()
            .unwrap()
            .to_string(),
        "image reads do not accept selectors"
    );
    std::fs::write(home.path().join("canary"), "EXTERNAL-CANARY").unwrap();
    std::os::unix::fs::symlink(home.path().join("canary"), root.join("outside")).unwrap();
    std::os::unix::fs::symlink(root.join("references/check.md"), root.join("inside")).unwrap();
    assert_eq!(
        read("skill://rust/inside", EditMode::Replace)
            .unwrap()
            .value["content"],
        body
    );
    std::fs::write(root.join("guide:2?#%.md"), "encoded resource\nlast\n").unwrap();
    assert_eq!(
        read(
            "skill://rust/guide%3A2%3F%23%25.md:2:raw",
            EditMode::Hashline
        )
        .unwrap()
        .value["content"],
        "last\n"
    );
    std::fs::write(root.join("%2e%2e"), "decoded only once\n").unwrap();
    assert_eq!(
        read("skill://rust/%252e%252e", EditMode::Replace)
            .unwrap()
            .value["content"],
        "decoded only once\n"
    );
    for path in [
        "skill://",
        "skill://rust%",
        "skill://%FF",
        "skill://rust/%00",
        "skill://rust/%2e%2e/canary",
        "skill://rust/%2Fetc/passwd",
        "skill://rust/outside",
        "skill://rust/missing",
        "skill://rust?query",
    ] {
        let error = read(path, EditMode::Replace).err().unwrap().to_string();
        assert!(!error.contains("EXTERNAL-CANARY"), "{path}: {error}");
    }
    skills
        .lock()
        .unwrap()
        .save_selection(&[("rust".into(), false)])
        .unwrap();
    assert_eq!(
        read("skill://rust", EditMode::Replace)
            .err()
            .unwrap()
            .to_string(),
        "Skill does not exist or is disabled"
    );
    skills
        .lock()
        .unwrap()
        .save_selection(&[("rust".into(), true)])
        .unwrap();
    std::fs::write(root.join("SKILL.md"), "changed without frontmatter\n").unwrap();
    assert_eq!(
        read("skill://rust", EditMode::Replace).unwrap().value["content"],
        "changed without frontmatter\n"
    );
    std::fs::remove_file(root.join("SKILL.md")).unwrap();
    assert!(read("skill://rust", EditMode::Replace).is_err());
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
            &pp_config::skills::Skills::default().shared(),
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
            &pp_config::skills::Skills::default().shared(),
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
        &pp_config::skills::Skills::default().shared(),
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
        &pp_config::skills::Skills::default().shared(),
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
            &pp_config::skills::Skills::default().shared(),
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
                &pp_config::skills::Skills::default().shared(),
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

use crate::tools::PipeStore;
use std::io::Write as _;
use std::time::{Duration, Instant};

fn wait_stream_bytes(store: &PipeStore, run: &str, stream: u64, want: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(snapshot) = store.read(run, stream, None)
            && snapshot.total_bytes >= want
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "relay did not mirror {want} bytes"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_stream_closed(store: &PipeStore, run: &str, stream: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(snapshot) = store.read(run, stream, None)
            && !snapshot.open
        {
            return;
        }
        assert!(Instant::now() < deadline, "stream did not close");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn pipeline_read_lists_bounded_run_state_and_streams() {
    let dir = tempfile::tempdir().unwrap();
    let store = PipeStore::new();
    let run = store
        .start_run("0123456789abcdef0123456789abcdef", "bash:call")
        .unwrap();
    let observer = run.observer();
    let pipeline = observer.begin(None, 2, false, false).unwrap();
    observer.label(pipeline, 1, "printf hidden").unwrap();
    observer.label(pipeline, 2, "tail -n 1").unwrap();
    let (_consumer, mut producer) = observer
        .pipe(pipeline, 1, "printf hidden", "tail -n 1")
        .unwrap()
        .unwrap();
    let body = b"hidden\nvisible\n";
    producer.write_all(body).unwrap();
    drop(producer);
    wait_stream_bytes(&store, run.id(), 1, body.len() as u64);
    observer.stage_done(pipeline, 1, 7, None).unwrap();
    observer.stage_done(pipeline, 2, 0, None).unwrap();
    observer.finish(pipeline, 0).unwrap();

    let value = read_pipe(
        &format!("pipe://{}", run.id()),
        dir.path(),
        ToolLimits::default(),
        EditMode::Replace,
        &store,
        &CancellationToken::new(),
    )
    .unwrap()
    .value;
    assert_eq!(value["ok"], true);
    assert_eq!(value["kind"], "pipe-run");
    assert_eq!(value["state"], "running");
    assert_eq!(value["nonzero_stages"], 1);
    assert_eq!(
        value["pipelines"][0]["stages"][0]["command"],
        "printf hidden"
    );
    assert_eq!(value["pipelines"][0]["stages"][0]["status"], 7);
    assert_eq!(value["streams"][0]["uri"], format!("pipe://{}/1", run.id()));
}

#[test]
fn pipeline_read_stream_normalizes_display_and_reuses_selectors() {
    let dir = tempfile::tempdir().unwrap();
    let store = PipeStore::new();
    let run = store
        .start_run("fedcba9876543210fedcba9876543210", "bash:call")
        .unwrap();
    let observer = run.observer();
    let pipeline = observer.begin(None, 2, false, false).unwrap();
    let (_consumer, mut producer) = observer.pipe(pipeline, 1, "cat", "cat").unwrap().unwrap();
    // CRLF 折叠为一个换行，ANSI 颜色串不进入正文，保留真实文本。
    let body = b"one\r\n\x1b[31mtwo\x1b[0m\n\xe5\x9b\x9bthree\x07\n";
    producer.write_all(body).unwrap();
    wait_stream_bytes(&store, run.id(), 1, body.len() as u64);

    let read = |suffix: &str, mode| {
        read_pipe(
            &format!("pipe://{}/1{suffix}", run.id()),
            dir.path(),
            ToolLimits::default(),
            mode,
            &store,
            &CancellationToken::new(),
        )
        .unwrap()
        .value
    };
    let full = read("", EditMode::Replace);
    assert_eq!(full["kind"], "pipe-stream");
    assert_eq!(full["content"], "one\ntwo\n四three\n");
    assert_eq!(full["pipe"]["start"], 0);
    assert_eq!(full["pipe"]["end"], body.len() as u64);
    assert_eq!(full["pipe"]["retained_start"], 0);
    assert_eq!(full["pipe"]["retained_end"], body.len() as u64);
    assert_eq!(full["pipe"]["total_bytes"], body.len() as u64);
    assert_eq!(full["pipe"]["open"], true);
    assert_eq!(full["pipe"]["prefix_evicted"], false);
    assert!(full["pipe"]["revision"].as_u64().unwrap() >= 1);
    assert_eq!(read(":2", EditMode::Replace)["content"], "two\n四three\n");
    assert_eq!(read(":2-2", EditMode::Replace)["content"], "two\n");
    assert_eq!(read(":-1", EditMode::Replace)["content"], "四three\n");
    // :raw 只影响选择/锚点，Hashline 模式也不会给临时流签发标签。
    assert_eq!(read(":raw:2", EditMode::Hashline)["content"], "two\n");
    assert!(!full["content"].as_str().unwrap().contains('\u{1b}'));
    assert!(!full["content"].as_str().unwrap().contains('\u{7}'));
    drop(producer);
}

#[test]
fn pipeline_read_empty_and_unavailable_uris_are_distinct() {
    let dir = tempfile::tempdir().unwrap();
    let store = PipeStore::new();
    let run = store
        .start_run("00112233445566778899aabbccddeeff", "bash:call")
        .unwrap();
    let observer = run.observer();
    let pipeline = observer.begin(None, 2, false, false).unwrap();
    let (_consumer, producer) = observer.pipe(pipeline, 1, "true", "cat").unwrap().unwrap();
    drop(producer);
    wait_stream_closed(&store, run.id(), 1);

    let empty = read_pipe(
        &format!("pipe://{}/1", run.id()),
        dir.path(),
        ToolLimits::default(),
        EditMode::Replace,
        &store,
        &CancellationToken::new(),
    )
    .unwrap()
    .value;
    assert_eq!(empty["content"], "");
    assert_eq!(empty["pipe"]["total_bytes"], 0);
    assert_eq!(empty["pipe"]["open"], false);

    for uri in [
        "pipe://ffffffffffffffffffffffffffffffff".to_owned(),
        format!("pipe://{}/9", run.id()),
    ] {
        let error = read_pipe(
            &uri,
            dir.path(),
            ToolLimits::default(),
            EditMode::Replace,
            &store,
            &CancellationToken::new(),
        )
        .err()
        .unwrap();
        assert!(
            error
                .to_string()
                .contains("Temporary pipe output is unavailable"),
            "{uri}: {error}"
        );
    }

    // 同名 POSIX 文件不能让虚拟 pipe 前缀被本地读取劫持。
    std::fs::create_dir_all(dir.path().join("pipe:")).unwrap();
    std::fs::write(dir.path().join("pipe:/abc"), "local file").unwrap();
    let error = read_pipe(
        "pipe://abc",
        dir.path(),
        ToolLimits::default(),
        EditMode::Replace,
        &store,
        &CancellationToken::new(),
    )
    .err()
    .unwrap();
    assert!(
        error
            .to_string()
            .contains("Temporary pipe output is unavailable"),
        "{error}"
    );
}

#[test]
fn pipeline_read_reports_retained_tail_offsets_after_eviction() {
    use std::io::Read as _;

    let dir = tempfile::tempdir().unwrap();
    let store = PipeStore::new();
    let run = store
        .start_run("aaaabbbbccccddddeeeeffff00001111", "bash:call")
        .unwrap();
    let observer = run.observer();
    let pipeline = observer.begin(None, 2, false, false).unwrap();
    let (mut consumer, mut producer) = observer.pipe(pipeline, 1, "yes", "head").unwrap().unwrap();
    let drain = std::thread::spawn(move || {
        let mut sink = Vec::new();
        consumer.read_to_end(&mut sink).unwrap();
        sink.len()
    });
    let mut body = "x\n".repeat(600_000);
    body.push_str("TAIL\n");
    producer.write_all(body.as_bytes()).unwrap();
    drop(producer);
    let written = body.len() as u64;
    wait_stream_bytes(&store, run.id(), 1, written);
    drain.join().unwrap();

    let full = read_pipe(
        &format!("pipe://{}/1", run.id()),
        dir.path(),
        ToolLimits::default(),
        EditMode::Replace,
        &store,
        &CancellationToken::new(),
    )
    .unwrap()
    .value;
    assert_eq!(full["pipe"]["total_bytes"], written);
    assert_eq!(full["pipe"]["end"], written);
    assert!(full["pipe"]["start"].as_u64().unwrap() > 0);
    assert_eq!(full["pipe"]["prefix_evicted"], true);
    assert_eq!(full["pipe"]["retained_end"], written);
    assert_eq!(full["truncated"], true);
    assert!(full["content"].as_str().unwrap().len() <= 65_536);
    assert!(full["next_path"].as_str().unwrap().starts_with("pipe://"));

    let tail = read_pipe(
        &format!("pipe://{}/1:-2", run.id()),
        dir.path(),
        ToolLimits::default(),
        EditMode::Replace,
        &store,
        &CancellationToken::new(),
    )
    .unwrap()
    .value;
    assert_eq!(tail["content"], "x\nTAIL\n");
    assert_eq!(tail["truncated"], false);
}
