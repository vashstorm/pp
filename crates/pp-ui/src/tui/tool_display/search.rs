//! rg／glob／plan_search 的标题与结果正文布局。

use super::*;

pub(in crate::tui) fn rg_header(view: &View<'_>, width: u16) -> Vec<Line<'static>> {
    let text = Style::default().fg(palette::RG_TEXT).bg(palette::RG_BG);
    let meta = text.fg(palette::RG_META);
    let pattern = view.arguments["pattern"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            view.arguments["args"]
                .as_array()
                .map(|args| serde_json::to_string(args).expect("JSON values serialize"))
                .unwrap_or_default()
        });
    let mut spans = vec![
        Span::styled(
            format!("{} rg: ", palette::tool_icon("rg")),
            text.fg(palette::tool_accent("rg")),
        ),
        Span::styled(pattern, text),
    ];
    let details = view.result.map(|result| &result["details"]);
    let output_mode = details
        .and_then(|d| d["outputMode"].as_str())
        .or_else(|| view.arguments["output_mode"].as_str())
        .unwrap_or("content");
    if output_mode != "content" {
        spans.push(Span::styled(format!(" · {output_mode}"), meta));
    }
    if view.failed {
        spans.push(Span::styled(" · failed", text.fg(palette::REMOVED)));
    } else if output_mode == "files_with_matches"
        && let Some(files) = details.and_then(|d| d["fileCount"].as_u64())
    {
        spans.push(Span::styled(
            format!(
                " · {files} {} · in ",
                if files == 1 { "file" } else { "files" }
            ),
            meta,
        ));
    } else if let Some((matches, files)) = details.and_then(|d| {
        Some((
            if output_mode == "count" {
                d["totalMatchCount"].as_u64()?
            } else {
                d["matchCount"].as_u64()?
            },
            d["fileCount"].as_u64()?,
        ))
    }) {
        let partial =
            if output_mode == "count" && details.is_some_and(|d| d["countComplete"] == false) {
                " (partial)"
            } else {
                ""
            };
        spans.push(Span::styled(
            format!(
                " {matches} {}{partial} · {files} {} · in ",
                if matches == 1 { "match" } else { "matches" },
                if files == 1 { "file" } else { "files" }
            ),
            meta,
        ));
    } else {
        spans.push(Span::styled(
            if view.running {
                " · running · in "
            } else {
                " · in "
            },
            meta,
        ));
    }
    let scope = details
        .and_then(|d| {
            (d["fileCount"] == 1)
                .then(|| d["fileMatches"][0]["path"].as_str())
                .flatten()
        })
        .or_else(|| details.and_then(|d| d["scopePath"].as_str()))
        .or_else(|| view.arguments["path"].as_str())
        .unwrap_or(".");
    let mut header = Line::from(spans);
    if !view.failed
        && header.width() + Line::raw(pp_core::text::display_text(scope).as_ref()).width()
            > usize::from(width)
    {
        let mut rows = wrap_block(header, width, "");
        rows.extend(wrap([scope.to_owned()], width, meta));
        rows
    } else {
        if !view.failed {
            header.spans.push(Span::styled(scope.to_owned(), meta));
        }
        wrap_block(header, width, "")
    }
}

pub(in crate::tui) fn glob_header(view: &View<'_>, width: u16) -> Vec<Line<'static>> {
    let style = Style::default().fg(palette::RG_TEXT).bg(palette::RG_BG);
    let (status, color) = if view.failed {
        (" · failed", palette::REMOVED)
    } else if view.running {
        (" · running", palette::ACCENT)
    } else {
        ("", palette::RG_META)
    };
    wrap_block(
        Line::from(vec![
            Span::styled(
                format!("{} Glob: ", palette::tool_icon("glob")),
                style.fg(palette::tool_accent("glob")),
            ),
            Span::styled(
                match view.arguments["pattern"].as_str() {
                    Some(pattern) => format!(
                        "{pattern} · {}",
                        view.arguments["path"].as_str().unwrap_or(".")
                    ),
                    None => view.arguments["path"].as_str().unwrap_or(".").to_owned(),
                },
                style,
            ),
            Span::styled(status, style.fg(color)),
        ]),
        width,
        "  ",
    )
}

fn search_row(line: &str, view: &View<'_>) -> Line<'static> {
    let style = Style::default().fg(palette::RG_TEXT).bg(palette::RG_BG);
    let meta = style.fg(palette::RG_META);
    if view.failed {
        return Line::styled(line.to_owned(), style.fg(palette::REMOVED));
    }
    if line.starts_with('…') || line.starts_with('[') || line.starts_with("No ") {
        return Line::styled(line.to_owned(), meta);
    }
    if view.name == "rg" {
        let content_mode = view
            .result
            .and_then(|r| r["details"]["outputMode"].as_str())
            .or_else(|| view.arguments["output_mode"].as_str())
            .unwrap_or("content")
            == "content";
        if content_mode
            && let Some((gutter, text)) = line.split_once('|')
            && gutter
                .trim_start_matches(['└', '─', ' ', '*'])
                .bytes()
                .all(|b| b.is_ascii_digit())
            && gutter.bytes().any(|b| b.is_ascii_digit())
        {
            let matched = gutter.contains('*');
            return Line::from(vec![
                Span::styled(
                    gutter.to_owned(),
                    if matched {
                        style.fg(palette::RG_TITLE)
                    } else {
                        meta
                    },
                ),
                Span::styled("│", meta),
                Span::styled(text.to_owned(), if matched { style } else { meta }),
            ])
            .style(style);
        }
        if view.result.is_some_and(|r| {
            r["details"]["fileMatches"]
                .as_array()
                .is_some_and(|files| files.iter().any(|file| file["path"].as_str() == Some(line)))
        }) {
            return Line::styled(line.to_owned(), style.fg(palette::READ_SUMMARY_PATH));
        }
    } else if view.name == "glob" {
        if line.starts_with("Warning:") {
            return Line::styled(line.to_owned(), style.fg(palette::ACCENT));
        }
        let split = line.trim_end_matches('/').rfind('/').map_or(0, |i| i + 1);
        return Line::from(vec![
            Span::styled(line[..split].to_owned(), meta),
            Span::styled(
                line[split..].to_owned(),
                style.fg(if line.ends_with('/') {
                    palette::RG_TITLE
                } else {
                    palette::READ_SUMMARY_PATH
                }),
            ),
        ])
        .style(style);
    }
    Line::styled(line.to_owned(), style)
}

fn search_rows(rows: Vec<Line<'static>>, view: &View<'_>) -> Vec<Line<'static>> {
    rows.into_iter()
        .map(|row| search_row(&row.to_string(), view))
        .collect()
}

pub(super) fn glob_body(view: &View<'_>, width: u16) -> Vec<Line<'static>> {
    let style = Style::default().fg(palette::RG_TEXT).bg(palette::RG_BG);
    let mut depth = 0;
    let grouped = view.result.is_some_and(|r| r["content"].is_string()) && !view.failed;
    let lines = logical(view.text)
        .into_iter()
        .map(|line| {
            if grouped {
                let level = line.bytes().take_while(|b| *b == b'#').count();
                if level > 0 && line[level..].starts_with(' ') && line.ends_with('/') {
                    depth = level;
                    return format!(
                        "{}{}",
                        "  ".repeat(level.saturating_sub(1)),
                        &line[level + 1..]
                    );
                }
                return format!("{}{line}", "  ".repeat(depth));
            }
            line.to_owned()
        })
        .collect::<Vec<_>>();
    let mut rows = if view.context.full || view.failed {
        wrap(lines, width, style)
    } else {
        head(
            &lines.join("\n"),
            if view.expanded {
                usize::MAX
            } else {
                FIND_ITEMS
            },
            false,
            true,
            width,
            style,
            MoreHint::Expand("items"),
        )
    };
    append_notices(&mut rows, view.result, width, style);
    search_rows(rows, view)
}

pub(super) fn rg_body(view: &View<'_>, width: u16) -> Vec<Line<'static>> {
    let style = Style::default()
        .fg(if view.failed {
            palette::REMOVED
        } else {
            palette::RG_TEXT
        })
        .bg(palette::RG_BG);
    let structured = view.result.is_some_and(|r| {
        r["content"].is_string()
            && r["details"]["outputMode"].as_str().unwrap_or("content") == "content"
    });
    let details = view.result.map(|r| &r["details"]);
    let single = details.is_some_and(|d| d["fileCount"] == 1);
    let files = details.and_then(|d| d["fileMatches"].as_array());
    let mut file_index = 0;
    let mut first = true;
    let mut lines = Vec::new();
    for line in logical(view.text) {
        if structured {
            // 路径已在标题或文件分组中展示；hash anchor 仍保留在原始工具结果。
            if single
                && files.is_some()
                && line.starts_with('[')
                && line.ends_with(']')
                && line.contains('#')
            {
                continue;
            }
            if files.is_some()
                && let Some(heading) = line.strip_prefix('#')
                && let Some((_, path)) = heading.trim_start_matches('#').split_once(' ')
            {
                if !path.ends_with('/') {
                    let path = files
                        .and_then(|files| files.get(file_index))
                        .and_then(|file| file["path"].as_str())
                        .unwrap_or(path);
                    if !single {
                        lines.push(path.to_owned());
                    }
                    file_index += 1;
                    first = true;
                }
                continue;
            }
            if let Some(i) = line.find([':', '|'])
                && i > 1
                && matches!(line.as_bytes()[0], b'*' | b' ')
                && line[1..i].bytes().all(|c| c.is_ascii_digit())
            {
                let branch = if first { "└─ " } else { "   " };
                lines.push(format!("{branch}{}|{}", &line[..i], &line[i + 1..]));
                first = false;
                continue;
            }
        }
        lines.push(line.to_owned());
    }
    let display = lines.join("\n");
    let rows = if view.context.full {
        wrap(lines, width, style)
    } else {
        head(
            &display,
            if view.expanded { usize::MAX } else { RG_LINES },
            true,
            true,
            width,
            style,
            MoreHint::Expand("items"),
        )
    };
    search_rows(rows, view)
}

pub(super) fn plan_search(view: &View<'_>, width: u16, style: Style) -> Vec<Line<'static>> {
    let Some(matches) = view.result.and_then(|r| r["matches"].as_array()) else {
        return head(
            view.text,
            if view.expanded {
                SEARCH_EXPANDED_LINES
            } else {
                RG_LINES
            },
            true,
            true,
            width,
            style,
            if view.expanded {
                MoreHint::Plain("lines")
            } else {
                MoreHint::Expand("lines")
            },
        );
    };
    let mut lines = Vec::new();
    let mut previous = "";
    for item in matches {
        let path = item["path"].as_str().unwrap_or_default();
        if path != previous {
            lines.push(path.to_owned());
            previous = path;
        }
        lines.push(format!(
            "  {}│{}",
            item["line"],
            item["text"].as_str().unwrap_or_default()
        ));
    }
    let mut rows = head(
        &lines.join("\n"),
        if view.expanded {
            SEARCH_EXPANDED_LINES
        } else {
            RG_LINES
        },
        true,
        true,
        width,
        style,
        if view.expanded {
            MoreHint::Plain("lines")
        } else {
            MoreHint::Expand("lines")
        },
    );
    append_notices(&mut rows, view.result, width, style);
    rows
}
