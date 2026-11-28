use super::*;
use pp_config::config::IconMode;
use std::path::Path;

/// 折叠 `TestBackend` 缓冲区在双宽字符后插入的占位空格，还原可读屏幕行。
fn readable_screen(buffer: &Buffer) -> String {
    (buffer.area.y..buffer.area.bottom())
        .map(|y| {
            let mut row = String::new();
            let mut skip_placeholder = false;
            for x in buffer.area.x..buffer.area.right() {
                let symbol = buffer[(x, y)].symbol();
                if skip_placeholder {
                    skip_placeholder = false;
                    if symbol == " " {
                        continue;
                    }
                }
                row.push_str(symbol);
                if Span::raw(symbol).width() > 1 {
                    skip_placeholder = true;
                }
            }
            row
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn user_image_renders_marker_omits_redundant_body_and_aligns_multiple_sources() {
    {
        let mut transcript = Transcript::new().with_cwd(Path::new("/work/project"));
        transcript.append(
            "You",
            "\u{f03e} #1 这个 welcome 如何，打个分\n\
             \u{f03e} #1 Image: /Users/panh/Library/Application Support/CleanShot/media/media_J0ZoDONZu8/CleanShot 2026-11-19 at 15.55.43@2x.png\n\
             Format: image/png\n\
             Dimensions: 1720 x 320\n\
             Bytes: 85170",
        );

        let buffer = render(&mut transcript, 100, 7);
        let text = readable_screen(&buffer);

        assert!(text.contains("这个 welcome 如何，打个分"), "{text}");
        assert!(
            text.contains("CleanShot 2026-11-19 at 15.55.43@2x.png"),
            "{text}"
        );
        assert!(text.contains("PNG · 1720×320 · 83.2 KB"), "{text}");
        assert!(!text.contains("Application Support"), "{text}");
        assert!(!text.contains("Format:"), "{text}");
        assert!(!text.contains("Dimensions:"), "{text}");
        assert!(!text.contains("Bytes:"), "{text}");

        // 顶/底留白与内容行统一保留 user_bg；正文内联标记和左导轨使用 attachment_color。
        assert_eq!(buffer[(0, 0)].bg, user_background());
        assert_eq!(buffer[(2, 1)].bg, user_background());
        assert_eq!(buffer[(2, 1)].fg, palette::attachment_color());
        assert!(buffer[(2, 1)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(2, 3)].symbol(), "▎");
        assert_eq!(buffer[(2, 3)].fg, palette::attachment_color());
        assert_eq!(buffer[(2, 3)].bg, user_background());
        assert_eq!(buffer[(0, 4)].bg, user_background());
    }

    {
        let mut transcript = Transcript::new();
        transcript.append(
            "You",
            "\u{f03e} #1\n\
             \u{f03e} #1 Image: 剪贴板\n\
             Format: image/png\n\
             Dimensions: 1280 x 720\n\
             Bytes: 245760",
        );

        let buffer = render(&mut transcript, 80, 5);
        let text = readable_screen(&buffer);
        let non_empty: Vec<_> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(non_empty.len(), 1, "{text}");
        assert!(non_empty[0].contains("▎ \u{f03e} #1"), "{text}");
        assert!(non_empty[0].contains("剪贴板"), "{text}");
        assert!(non_empty[0].contains("PNG · 1280×720 · 240 KB"), "{text}");
    }

    {
        let mut transcript = Transcript::new().with_cwd(Path::new("/work/project"));
        transcript.append(
            "You",
            "对比 \u{f03e} #1、\u{f03e} #2 与 \u{f03e} #10\n\
             \u{f03e} #1 Image: /work/project/assets/home.png\n\
             Format: image/png\n\
             Dimensions: 100 x 50\n\
             Bytes: 512\n\
             \u{f03e} #2 Image: /tmp/dir_a/shot.png\n\
             Format: image/jpeg\n\
             Dimensions: 800 x 600\n\
             Bytes: 1572864\n\
             \u{f03e} #10 Image: /tmp/dir_b/shot.png\n\
             Format: image/webp\n\
             Dimensions: 400 x 300\n\
             Bytes: 4096",
        );

        let buffer = render(&mut transcript, 100, 8);
        let text = readable_screen(&buffer);
        assert!(text.contains("assets/home.png"), "{text}");
        assert!(text.contains("dir_a/shot.png"), "{text}");
        assert!(text.contains("dir_b/shot.png"), "{text}");
        assert!(text.contains("PNG · 100×50 · 512 B"), "{text}");
        assert!(text.contains("JPEG · 800×600 · 1.5 MB"), "{text}");
        assert!(text.contains("WebP · 400×300 · 4 KB"), "{text}");

        // 多图行中规格起始列对齐。
        let meta_col = |needle: &str| {
            text.lines()
                .find_map(|line| line.find(needle).map(|idx| line[..idx].chars().count()))
                .unwrap()
        };
        assert_eq!(meta_col("PNG ·"), meta_col("JPEG ·"));
        assert_eq!(meta_col("JPEG ·"), meta_col("WebP ·"));
    }
}

#[test]
fn user_image_formats_ascii_wrap_and_plain_text_non_image() {
    let cases = [
        (
            "看 \u{f03e} #1\n\
             \u{f03e} #1 Image: /tmp/anim.gif\n\
             Format: image/png\n\
             Dimensions: 8000 x 4000\n\
             Bytes: 4194304\n\
             GIF: first frame\n\
             Resized from: 10000 x 5000 (max 8000 px per side)",
            &[
                "anim.gif",
                "PNG · 8000×4000 · 4.0 MB · first frame · resized from 10000×5000",
            ][..],
        ),
        (
            "[图片 1] 旧历史\n\
             [图片 1] Image: /tmp/legacy.png\n\
             Format: image/png\n\
             Dimensions: 640 x 480\n\
             Bytes: 2048",
            &["旧历史", "legacy.png", "PNG · 640×480 · 2 KB"][..],
        ),
        (
            "查看 @docs/ui.png\n\
             [图片引用 1] @docs/ui.png\n\
             Image: /work/docs/ui.png\n\
             Format: image/png\n\
             Dimensions: 320 x 240\n\
             Bytes: 1024",
            &["查看 @docs/ui.png", "docs/ui.png", "PNG · 320×240 · 1 KB"][..],
        ),
    ];

    for (raw, expected) in cases {
        let mut transcript = Transcript::new();
        transcript.append("You", raw);
        let text = readable_screen(&render(&mut transcript, 120, 8));
        for needle in expected {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        assert!(!text.contains("Dimensions:"), "{text}");
    }

    {
        let prev = crate::tui::themes::current();
        crate::tui::themes::set_active(prev.0, IconMode::Ascii);

        let mut transcript = Transcript::new();
        transcript.append(
            "You",
            "\u{f03e} #1 检查一下\n\
             \u{f03e} #1 Image: /tmp/CleanShot 2026-11-19 at 15.55.43@2x.png\n\
             Format: image/png\n\
             Dimensions: 1720 x 320\n\
             Bytes: 85170",
        );

        let text = readable_screen(&render(&mut transcript, 54, 8));
        crate::tui::themes::set_active(prev.0, prev.1);

        assert!(!text.contains('\u{f03e}'), "{text}");
        assert!(!text.contains('▎'), "{text}");
        assert!(text.contains("[img #1] 检查一下"), "{text}");
        assert!(text.contains("| [img #1]"), "{text}");
        assert!(text.contains("PNG · 1720x320 · 83.2 KB"), "{text}");
        // 窄屏拆行后每条附件续行仍保留左导轨。
        let rail_lines: Vec<_> = text
            .lines()
            .filter(|line| line.trim_start().starts_with('|'))
            .collect();
        assert!(rail_lines.len() >= 2, "{text}");
    }

    {
        let mut transcript = Transcript::new();
        let raw = "排查日志：\nImage: foo.png\nFormat: not-a-mime\nDimensions: unknown";
        transcript.append("You", raw);

        let text = readable_screen(&render(&mut transcript, 80, 8));
        assert!(text.contains("Image: foo.png"), "{text}");
        assert!(text.contains("Format: not-a-mime"), "{text}");
        assert!(!text.contains('▎'), "{text}");
    }
}
