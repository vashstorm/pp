//! 简单命令、内置与扩展测试子句的语句级渲染。

use brush_core::parser::ast::{self, CommandPrefixOrSuffixItem as Item, SourceLocation};
use pp_core::shell_display::{ShellSegment, ShellStyle as S};

use super::super::{segment, words};
use super::Printer;
use super::idioms::{argument_language, set_meaning, test, timeout_duration};
use super::{arguments, has_redirections, items};

impl<'a> Printer<'a> {
    pub(super) fn inline(
        &mut self,
        command: &ast::Command,
        node: Option<u32>,
    ) -> Vec<ShellSegment> {
        match command {
            ast::Command::Simple(simple) => {
                let fallback = self.simple(simple, node, false);
                let name = simple.word_or_name.as_ref().map(|word| word.value.clone());
                let shadowed = name
                    .as_ref()
                    .is_some_and(|name| self.functions.remove(name));
                let builtin_form = if shadowed {
                    self.simple(simple, node, true)
                } else {
                    Vec::new()
                };
                if shadowed {
                    self.functions.insert(name.clone().expect("有函数名称"));
                }
                let converted = self.simple(simple, node, true);
                if let Some(node) = node {
                    let mut function_form = self.function_call(simple, Some(node));
                    if let Some(env) = fallback.first().filter(|segment| {
                        segment.style == S::Variable && segment.text.starts_with("env(")
                    }) {
                        function_form.insert(0, env.clone());
                    }
                    self.model.nodes[node as usize].function_form = function_form;
                    self.model.nodes[node as usize].builtin_form = builtin_form;
                    if fallback != converted
                        && (shadowed
                            || name.as_deref().is_some_and(|name| {
                                matches!(
                                    name,
                                    "echo"
                                        | "printf"
                                        | "source"
                                        | "."
                                        | "exit"
                                        | "return"
                                        | "break"
                                        | "continue"
                                        | "test"
                                        | "["
                                        | "cd"
                                        | "set"
                                )
                            }))
                    {
                        self.model.nodes[node as usize].external_form = fallback;
                    }
                }
                converted
            }
            ast::Command::ExtendedTest(test, redirects) => {
                let mut segments = vec![segment(self.extended(&test.expr), S::Variable, node)];
                if let Some(redirects) = redirects {
                    segments.extend(self.redirections(&redirects.0, node));
                }
                segments
            }
            ast::Command::Compound(ast::CompoundCommand::Arithmetic(a), redirects) => {
                let mut segments = vec![segment(a.expr.to_string(), S::Variable, node)];
                if let Some(redirects) = redirects {
                    segments.extend(self.redirections(&redirects.0, node));
                }
                segments
            }
            _ => vec![segment(self.original(command), S::Text, node)],
        }
    }

    fn simple(
        &mut self,
        simple: &ast::SimpleCommand,
        node: Option<u32>,
        idioms: bool,
    ) -> Vec<ShellSegment> {
        let args = arguments(simple);
        let name = args
            .first()
            .and_then(|w| words::literal(w))
            .unwrap_or_default();
        let builtin = !self.functions.contains(&name);
        let mut output = Vec::new();
        let assignments = items(simple)
            .filter_map(|item| {
                if let Item::AssignmentWord(a, _) = item {
                    Some(a)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        if !assignments.is_empty() {
            let text = assignments
                .iter()
                .map(|a| {
                    format!(
                        "{}{} = {}",
                        a.name,
                        if a.append { "+" } else { "" },
                        match &a.value {
                            ast::AssignmentValue::Scalar(w) =>
                                words::render(&w.value, true, &self.constants),
                            ast::AssignmentValue::Array(values) => format!(
                                "[{}]",
                                values
                                    .iter()
                                    .map(|(_, w)| words::render(&w.value, true, &self.constants))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            output.push(segment(
                if args.is_empty() {
                    text
                } else {
                    format!("env({text}) ")
                },
                S::Variable,
                node,
            ));
            if args.is_empty() && idioms {
                let mut all_inline = true;
                for a in &assignments {
                    if let ast::AssignmentName::VariableName(name) = &a.name
                        && let Some(value) = self.constants.get(&format!("\0{name}")).cloned()
                    {
                        self.constants.insert(name.clone(), value);
                    } else {
                        all_inline = false;
                    }
                }
                if all_inline && !has_redirections(simple) {
                    return Vec::new();
                }
            }
        }
        let call = self.functions.contains(&name)
            || builtin
                && matches!(
                    name.as_str(),
                    "echo" | "printf" | "source" | "." | "exit" | "return" | "break" | "continue"
                );
        if idioms && self.functions.contains(&name) {
            output.extend(self.function_call(simple, node));
            return output;
        } else if idioms && call {
            output.push(segment(
                format!(
                    "{}(",
                    match name.as_str() {
                        "echo" | "printf" => "print",
                        "." => "source",
                        _ => &name,
                    }
                ),
                S::Command,
                node,
            ));
            let args = args
                .iter()
                .skip(1)
                .enumerate()
                .filter(|(i, raw)| {
                    !(*i == 0
                        && name == "echo"
                        && words::literal(raw).is_some_and(|v| {
                            v.starts_with('-')
                                && v.len() > 1
                                && v[1..].chars().all(|ch| matches!(ch, 'n' | 'e' | 'E'))
                        }))
                })
                .map(|(_, raw)| raw)
                .collect::<Vec<_>>();
            for (index, arg) in args.iter().enumerate() {
                if index > 0 {
                    output.push(segment(", ", S::Operator, node));
                }
                output.push(segment(
                    words::render(arg, true, &self.constants),
                    S::String,
                    node,
                ));
            }
            output.push(segment(")", S::Operator, node));
        } else if idioms && builtin && matches!(name.as_str(), "test" | "[") {
            let mut values = args.iter().skip(1).cloned().collect::<Vec<_>>();
            if name == "[" && values.last().is_some_and(|s| s == "]") {
                values.pop();
            }
            output.push(segment(test(&values, &self.constants), S::Variable, node));
        } else if idioms && builtin && name == "cd" && args.len() == 2 && !has_redirections(simple)
        {
            output.push(segment(
                format!("cwd {}", words::render(&args[1], false, &self.constants)),
                S::Badge,
                node,
            ));
        } else if idioms
            && builtin
            && name == "set"
            && args.len() == 2
            && set_meaning(&args[1]).is_some()
        {
            output.push(segment(
                format!("# {}", set_meaning(&args[1]).unwrap_or_default()),
                S::Comment,
                node,
            ));
        } else if idioms
            && builtin
            && name == "curl"
            && args
                .iter()
                .any(|arg| words::literal(arg).as_deref() == Some("%{http_code}"))
            && args
                .iter()
                .any(|arg| words::literal(arg).as_deref() == Some("/dev/null"))
            && args.iter().any(|arg| {
                words::literal(arg).is_some_and(|value| {
                    value.starts_with("http:")
                        || value.starts_with("https:")
                        || value.starts_with("localhost")
                })
            })
        {
            let url = args
                .iter()
                .find(|arg| {
                    words::literal(arg).is_some_and(|value| {
                        value.starts_with("http:")
                            || value.starts_with("https:")
                            || value.starts_with("localhost")
                    })
                })
                .expect("已检查 URL");
            output.push(segment("http_status(", S::Command, node));
            output.push(segment(
                words::render(url, true, &self.constants),
                S::String,
                node,
            ));
            output.push(segment(")", S::Operator, node));
        } else {
            let start = if idioms
                && builtin
                && name == "timeout"
                && args.len() > 2
                && timeout_duration(&args[1])
            {
                output.push(segment(
                    format!(
                        "⏱ {}{} ",
                        args[1],
                        if args[1].ends_with(|ch: char| ch.is_ascii_digit()) {
                            "s"
                        } else {
                            ""
                        }
                    ),
                    S::Badge,
                    node,
                ));
                2
            } else {
                0
            };
            for (index, arg) in args.iter().enumerate().skip(start) {
                if index > start {
                    output.push(segment(" ", S::Text, node));
                }
                let language = argument_language(&name, &args, index);
                let literal = words::literal(arg);
                let pattern = language == Some("regex");
                if idioms && let (Some(language), Some(value)) = (language, literal) {
                    if value.contains('\n') || value.len() > 64 {
                        output.push(segment(format!("⟦{language} ↓⟧"), S::Badge, node));
                    } else {
                        output.push(segment("‹", S::Text, node));
                        let mut code = segment(value, S::String, node);
                        code.language = Some(language.to_owned());
                        output.push(code);
                        output.push(segment("›", S::Text, node));
                    }
                } else {
                    let style = if index == start {
                        S::Command
                    } else if arg.starts_with('$') {
                        S::Variable
                    } else if arg.chars().all(|c| c.is_ascii_digit()) {
                        S::Number
                    } else if arg.starts_with(['\'', '"']) {
                        S::String
                    } else {
                        S::Text
                    };
                    let text = if pattern {
                        arg.clone()
                    } else {
                        words::render(arg, false, &self.constants)
                    };
                    output.push(segment(text, style, node));
                }
            }
        }
        let redirects = items(simple)
            .filter_map(|item| {
                if let Item::IoRedirect(r) = item {
                    Some(r.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        output.extend(self.redirections(&redirects, node));
        output
    }

    pub(super) fn function_call(
        &self,
        simple: &ast::SimpleCommand,
        node: Option<u32>,
    ) -> Vec<ShellSegment> {
        let args = arguments(simple);
        let Some(name) = args.first() else {
            return Vec::new();
        };
        let mut out = vec![segment(format!("{name}("), S::Command, node)];
        for (index, arg) in args.iter().skip(1).enumerate() {
            if index > 0 {
                out.push(segment(", ", S::Operator, node));
            }
            out.push(segment(
                words::render(arg, true, &self.constants),
                S::String,
                node,
            ));
        }
        out.push(segment(")", S::Operator, node));
        let redirects = items(simple)
            .filter_map(|item| {
                if let Item::IoRedirect(r) = item {
                    Some(r.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        out.extend(self.redirections(&redirects, node));
        out
    }

    fn extended(&self, expression: &ast::ExtendedTestExpr) -> String {
        match expression {
            ast::ExtendedTestExpr::And(a, b) => {
                format!("{} and {}", self.extended(a), self.extended(b))
            }
            ast::ExtendedTestExpr::Or(a, b) => {
                format!("{} or {}", self.extended(a), self.extended(b))
            }
            ast::ExtendedTestExpr::Not(a) => format!("not {}", self.extended(a)),
            ast::ExtendedTestExpr::Parenthesized(a) => format!("({})", self.extended(a)),
            ast::ExtendedTestExpr::UnaryTest(predicate, value) => test(
                &[predicate.to_string(), value.value.clone()],
                &self.constants,
            ),
            ast::ExtendedTestExpr::BinaryTest(predicate, left, right) => test(
                &[
                    left.value.clone(),
                    predicate.to_string(),
                    right.value.clone(),
                ],
                &self.constants,
            ),
        }
    }

    fn original(&self, command: &ast::Command) -> String {
        if let (Some(source), Some(span)) = (self.source, command.location()) {
            source
                .chars()
                .skip(span.start.index)
                .take(span.end.index.saturating_sub(span.start.index))
                .collect()
        } else {
            command.to_string()
        }
    }
}
