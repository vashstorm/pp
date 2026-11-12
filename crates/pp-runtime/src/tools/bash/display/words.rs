//! 词级转换只使用解析语法与可证明常量，不读取宿主环境、不做展开执行。

use brush_core::parser::word::{self, Parameter, ParameterExpr, WordPiece, WordPieceWithSource};
use std::collections::HashMap;

pub(super) fn literal(raw: &str) -> Option<String> {
    let pieces = word::parse(raw, &Default::default()).ok()?;
    literal_pieces(&pieces)
}

fn literal_pieces(pieces: &[WordPieceWithSource]) -> Option<String> {
    let mut output = String::new();
    for piece in pieces {
        match &piece.piece {
            WordPiece::Text(s) | WordPiece::SingleQuotedText(s) => output.push_str(s),
            WordPiece::EscapeSequence(s) => output.push_str(s.strip_prefix('\\').unwrap_or(s)),
            WordPiece::DoubleQuotedSequence(pieces) => output.push_str(&literal_pieces(pieces)?),
            // 保留 ANSI-C 及 gettext 原文；不猜测 Bash 特定的解码语义。
            _ => return None,
        }
    }
    Some(output)
}

pub(super) fn quote(value: &str) -> String {
    serde_json::to_string(value).expect("字符串总能编码为 JSON")
}

pub(super) fn render(raw: &str, string: bool, constants: &HashMap<String, String>) -> String {
    if let Some(value) = literal(raw) {
        return if string || raw.starts_with(['\'', '"']) || value.chars().any(char::is_whitespace) {
            quote(&value)
        } else {
            value
        };
    }
    let Ok(pieces) = word::parse(raw, &Default::default()) else {
        return raw.to_owned();
    };
    let mut output = String::new();
    let mut dynamic = false;
    if !render_pieces(&pieces, constants, &mut output, &mut dynamic) {
        return raw.to_owned();
    }
    if dynamic {
        if pieces.len() == 1
            && matches!(
                &pieces[0].piece,
                WordPiece::ParameterExpansion(ParameterExpr::Parameter { .. })
            )
            && !string
        {
            return output
                .trim_start_matches('{')
                .trim_end_matches('}')
                .to_owned();
        }
        format!("f{}", quote(&output))
    } else if string || raw.starts_with('"') {
        quote(&output)
    } else {
        output
    }
}

fn render_pieces(
    pieces: &[WordPieceWithSource],
    constants: &HashMap<String, String>,
    output: &mut String,
    dynamic: &mut bool,
) -> bool {
    for piece in pieces {
        match &piece.piece {
            WordPiece::Text(s) | WordPiece::SingleQuotedText(s) => output.push_str(s),
            WordPiece::EscapeSequence(s) => output.push_str(s.strip_prefix('\\').unwrap_or(s)),
            WordPiece::DoubleQuotedSequence(parts) => {
                if !render_pieces(parts, constants, output, dynamic) {
                    return false;
                }
            }
            WordPiece::ParameterExpansion(ParameterExpr::Parameter {
                parameter,
                indirect: false,
            }) => {
                let name = match parameter {
                    Parameter::Named(name) => name.clone(),
                    Parameter::Positional(index) => format!("arg{index}"),
                    _ => return false,
                };
                if let Some(value) = constants.get(&name) {
                    *dynamic = true;
                    output.push_str("{‹");
                    output.push_str(value);
                    output.push_str("›}");
                } else {
                    *dynamic = true;
                    output.push('{');
                    output.push_str(&name);
                    output.push('}');
                }
            }
            WordPiece::CommandSubstitution(source)
            | WordPiece::BackquotedCommandSubstitution(source) => {
                *dynamic = true;
                output.push_str("{capture(");
                output.push_str(&quote(source));
                output.push_str(")}");
            }
            WordPiece::ArithmeticExpression(expression) => {
                *dynamic = true;
                output.push('{');
                output.push_str(&expression.to_string());
                output.push('}');
            }
            _ => return false,
        }
    }
    true
}
