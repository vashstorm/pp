//! 重定向、here-document 与外部脚本字面量的展示。

use brush_core::parser::{
    Parser,
    ast::{self, CommandPrefixOrSuffixItem as Item},
};
use pp_core::shell_display::{ShellSegment, ShellStyle as S};
use std::io::Cursor;

use super::super::{MAX_NODES, Model, segment, words};
use super::Printer;
use super::idioms::argument_language;
use super::{arguments, items};

impl<'a> Printer<'a> {
    pub(super) fn redirections(
        &self,
        redirects: &[ast::IoRedirect],
        node: Option<u32>,
    ) -> Vec<ShellSegment> {
        redirects
            .iter()
            .map(|redirect| {
                let text = match redirect {
                    ast::IoRedirect::File(fd, kind, target) => {
                        let target = match target {
                            ast::IoFileRedirectTarget::Filename(w)
                            | ast::IoFileRedirectTarget::Duplicate(w) => {
                                words::literal(&w.value).unwrap_or_else(|| w.to_string())
                            }
                            _ => target.to_string(),
                        };
                        if target == "/dev/null"
                            && !matches!(
                                kind,
                                ast::IoFileRedirectKind::Read
                                    | ast::IoFileRedirectKind::ReadAndWrite
                                    | ast::IoFileRedirectKind::DuplicateInput
                            )
                        {
                            if *fd == Some(2) {
                                " 󱒼".to_owned()
                            } else {
                                " 󱙍".to_owned()
                            }
                        } else if *fd == Some(2)
                            && matches!(kind, ast::IoFileRedirectKind::DuplicateOutput)
                            && target == "1"
                        {
                            " +err".to_owned()
                        } else {
                            format!(
                                " {}{kind}{target}",
                                fd.map_or_else(String::new, |fd| fd.to_string())
                            )
                        }
                    }
                    ast::IoRedirect::OutputAndError(w, _)
                        if words::literal(&w.value).as_deref() == Some("/dev/null") =>
                    {
                        " 󰖁".to_owned()
                    }
                    ast::IoRedirect::HereDocument(_, document) => {
                        format!(" ⟵ {} ↓", document_language(document))
                    }
                    _ => format!(" {redirect}"),
                };
                segment(text, S::Badge, node)
            })
            .collect()
    }

    pub(super) fn documents(&mut self, command: &ast::Command, depth: u16) {
        let ast::Command::Simple(simple) = command else {
            return;
        };
        let redirects = items(simple)
            .filter_map(|item| {
                if let Item::IoRedirect(r) = item {
                    Some(r.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        self.heredocs(&redirects, depth + 1);
        let args = arguments(simple);
        let name = args
            .first()
            .and_then(|w| words::literal(w))
            .unwrap_or_default();
        for (index, arg) in args.iter().enumerate().skip(1) {
            let Some(language) = argument_language(&name, &args, index) else {
                continue;
            };
            let Some(value) = words::literal(arg) else {
                continue;
            };
            if value.contains('\n') || value.len() > 64 {
                if language == "bash" {
                    let mut reader = Cursor::new(value.as_bytes());
                    if let Ok(program) =
                        Parser::new(&mut reader, &Default::default()).parse_program()
                    {
                        // 外部 shell 字面量只有静态结构，绝不冒充当前解释器的子执行。
                        let mut detached = Model::default();
                        let mut printer = Printer::new(&mut detached, 0, Some(&value), depth + 1);
                        printer.program(&program);
                        if !printer.overflow {
                            for mut row in detached.rows {
                                if self.model.rows.len() >= MAX_NODES {
                                    self.overflow = true;
                                    break;
                                }
                                row.node = None;
                                for segment in &mut row.segments {
                                    segment.node = None;
                                }
                                self.model.rows.push(row);
                            }
                            continue;
                        }
                    }
                }
                self.document(&value, depth + 1, language);
            }
        }
    }

    pub(super) fn heredocs(&mut self, redirects: &[ast::IoRedirect], depth: u16) {
        for redirect in redirects {
            if let ast::IoRedirect::HereDocument(_, document) = redirect {
                let language = document_language(document);
                let delimiter = words::literal(&document.here_end.value)
                    .unwrap_or_else(|| document.here_end.value.clone());
                self.push(
                    depth,
                    None,
                    vec![segment(
                        format!("{language} · heredoc «{delimiter}»"),
                        S::Comment,
                        None,
                    )],
                    true,
                );
                self.document(&document.doc.value, depth, language);
            }
        }
    }

    fn document(&mut self, text: &str, depth: u16, language: &str) {
        for line in text.lines() {
            let mut code = segment(line, S::Text, None);
            code.language = Some(language.to_owned());
            self.push(depth, None, vec![code], true);
        }
    }
}

fn document_language(document: &ast::IoHereDocument) -> &'static str {
    let delimiter = words::literal(&document.here_end.value).unwrap_or_default();
    match delimiter.to_ascii_lowercase().as_str() {
        "py" | "python" => "py",
        "js" | "javascript" => "js",
        "ts" | "typescript" => "ts",
        "sh" | "bash" | "zsh" => "bash",
        "rs" | "rust" => "rust",
        "sql" => "sql",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "html" => "html",
        "css" => "css",
        "xml" => "xml",
        "toml" => "toml",
        _ => "text",
    }
}
