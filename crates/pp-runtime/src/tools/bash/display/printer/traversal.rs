//! 复合语句、管道与条件列表的遍历。

use brush_core::parser::ast;
use pp_core::shell_display::{ShellSegment, ShellStyle as S};
use std::collections::HashSet;

use super::super::{MAX_DEPTH, segment, words};
use super::Printer;
use super::idioms::{effect, function_parameters, lone_simple, pipe_stderr, tail_count};
use super::{arguments, has_redirections, inline_command};

impl<'a> Printer<'a> {
    pub(super) fn list(&mut self, list: &ast::CompoundList, depth: u16) {
        if depth > MAX_DEPTH {
            self.overflow = true;
            return;
        }
        for item in &list.0 {
            if self.overflow {
                break;
            }
            for (index, (op, pipeline)) in (&item.0).into_iter().enumerate() {
                let lead = if index == 0 {
                    if matches!(item.1, ast::SeparatorOperator::Async) {
                        "spawn "
                    } else {
                        ""
                    }
                } else if matches!(op, ast::PipelineOperator::And) {
                    "and "
                } else {
                    "or "
                };
                if index > 0
                    && index == item.0.additional.len()
                    && matches!(op, ast::PipelineOperator::Or)
                    && !pipeline.bang
                    && pipeline.timed.is_none()
                    && pipeline.seq.len() == 1
                    && let ast::Command::Simple(simple) = &pipeline.seq[0]
                    && !has_redirections(simple)
                    && simple.prefix.is_none()
                    && matches!(arguments(simple).as_slice(), [name] if matches!(name.as_str(), "true" | ":") && !self.functions.contains(name))
                {
                    let node = self.register(&pipeline.seq[0], true);
                    if let Some(previous) = self.model.rows.last_mut() {
                        previous.segments.push(segment(" may fail", S::Badge, node));
                        if let Some(node) = previous.node {
                            self.model.nodes[node as usize].guard = true;
                        }
                    }
                } else {
                    self.pipeline(pipeline, depth + u16::from(index > 0), lead);
                }
            }
        }
    }

    fn pipeline(&mut self, pipeline: &ast::Pipeline, depth: u16, lead: &str) {
        if pipeline.seq.iter().all(inline_command) {
            let mut segments = Vec::new();
            if !lead.is_empty() {
                segments.push(segment(lead, S::Operator, None));
            }
            if pipeline.bang {
                segments.push(segment("not ", S::Keyword, None));
            }
            if pipeline.timed.is_some() {
                segments.push(segment("timed ", S::Badge, None));
            }
            let mut first = None;
            for (index, command) in pipeline.seq.iter().enumerate() {
                let node = self.register(command, false);
                if index == 0 {
                    first = node;
                }
                if let ast::Command::Simple(simple) = command
                    && index > 0
                    && index + 1 == pipeline.seq.len()
                    && !has_redirections(simple)
                    && !simple
                        .word_or_name
                        .as_ref()
                        .is_some_and(|w| self.functions.contains(&w.value))
                    && let Some((name, count)) = tail_count(simple)
                {
                    let mut alternative = vec![segment(" → ", S::Operator, node)];
                    alternative.extend(self.function_call(simple, node));
                    if let Some(node) = node {
                        self.model.nodes[node as usize].function_form = alternative;
                    }
                    segments.push(segment(
                        format!(
                            " {} {count}",
                            if name == "head" {
                                "⇡ first"
                            } else {
                                "⇣ last"
                            }
                        ),
                        S::Badge,
                        node,
                    ));
                    continue;
                }
                if index > 0 {
                    segments.push(segment(
                        if pipe_stderr(&pipeline.seq[index - 1]) {
                            " →& "
                        } else {
                            " → "
                        },
                        S::Operator,
                        None,
                    ));
                }
                segments.extend(self.inline(command, node));
            }
            let mut effects = pipeline
                .seq
                .iter()
                .filter_map(|c| {
                    if let ast::Command::Simple(s) = c {
                        Some(effect(s))
                    } else {
                        None
                    }
                })
                .collect::<HashSet<_>>()
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            effects.sort_unstable();
            let row_index = self.model.rows.len();
            self.push(depth, first, segments, false);
            if let Some(row) = self.model.rows.get_mut(row_index) {
                row.effects = effects.join("");
            }
            for command in &pipeline.seq {
                self.documents(command, depth);
            }
        } else {
            for (index, command) in pipeline.seq.iter().enumerate() {
                let start = self.model.rows.len();
                self.command(command, depth + u16::from(index > 0));
                let prefix = if index == 0 { lead } else { "→ " };
                if !prefix.is_empty()
                    && let Some(row) = self.model.rows.get_mut(start)
                {
                    row.segments.insert(0, segment(prefix, S::Operator, None));
                }
            }
        }
    }

    fn command(&mut self, command: &ast::Command, depth: u16) {
        if depth > MAX_DEPTH || self.nesting >= MAX_DEPTH {
            self.overflow = true;
            return;
        }
        self.nesting += 1;
        let node = self.register(command, false);
        match command {
            ast::Command::Compound(compound, redirects) => {
                self.compound(compound, depth, node);
                if let Some(redirects) = redirects {
                    let redirs = self.redirections(&redirects.0, node);
                    if let Some(row) = self
                        .model
                        .rows
                        .iter_mut()
                        .rev()
                        .find(|row| row.node == node)
                    {
                        row.segments.extend(redirs);
                    }
                    self.heredocs(&redirects.0, depth + 1);
                }
            }
            ast::Command::Function(function) => {
                self.functions.insert(function.fname.value.clone());
                self.push(
                    depth,
                    node,
                    vec![segment(
                        format!(
                            "def {}({}):",
                            function.fname.value,
                            function_parameters(&function.body.to_string())
                        ),
                        S::Command,
                        node,
                    )],
                    true,
                );
                self.compound(&function.body.0, depth + 1, None);
            }
            _ => {
                let segments = self.inline(command, node);
                self.push(depth, node, segments, false);
                self.documents(command, depth);
            }
        }
        self.nesting -= 1;
    }

    fn compound(&mut self, compound: &ast::CompoundCommand, depth: u16, node: Option<u32>) {
        if depth > MAX_DEPTH {
            self.overflow = true;
            return;
        }
        match compound {
            ast::CompoundCommand::BraceGroup(group) => self.list(&group.list, depth),
            ast::CompoundCommand::Subshell(subshell) => {
                self.push(
                    depth,
                    node,
                    vec![segment("subshell:", S::Keyword, node)],
                    true,
                );
                self.list(&subshell.list, depth + 1);
            }
            ast::CompoundCommand::IfClause(condition) => {
                let mut segments = vec![segment("if ", S::Keyword, None)];
                let (guard, inline) = self.condition(&condition.condition);
                segments.extend(inline);
                segments.push(segment(":", S::Operator, None));
                self.push(depth, guard.or(node), segments, true);
                self.list(&condition.then, depth + 1);
                for clause in condition.elses.iter().flatten() {
                    let (guard, mut segments) = if let Some(condition) = &clause.condition {
                        let (guard, mut body) = self.condition(condition);
                        body.insert(0, segment("elif ", S::Keyword, None));
                        (guard, body)
                    } else {
                        (None, vec![segment("else", S::Keyword, None)])
                    };
                    segments.push(segment(":", S::Operator, None));
                    self.push(depth, guard, segments, true);
                    self.list(&clause.body, depth + 1);
                }
            }
            ast::CompoundCommand::WhileClause(clause)
            | ast::CompoundCommand::UntilClause(clause) => {
                let (guard, mut body) = self.condition(&clause.0);
                body.insert(
                    0,
                    segment(
                        if matches!(compound, ast::CompoundCommand::WhileClause(_)) {
                            "while "
                        } else {
                            "while not "
                        },
                        S::Keyword,
                        node,
                    ),
                );
                body.push(segment(":", S::Operator, None));
                if let Some(simple) = lone_simple(&clause.0)
                    && simple
                        .word_or_name
                        .as_ref()
                        .is_some_and(|w| w.value == "read")
                {
                    let variables = arguments(simple)
                        .into_iter()
                        .skip(1)
                        .filter(|arg| !arg.starts_with('-'))
                        .collect::<Vec<_>>()
                        .join(", ");
                    body = vec![segment(
                        format!("for {variables} in lines:"),
                        S::Keyword,
                        guard,
                    )];
                }
                self.push(depth, node.or(guard), body, true);
                self.list(&clause.1.list, depth + 1);
            }
            ast::CompoundCommand::ForClause(clause) => {
                let values = clause.values.as_ref().map_or_else(
                    || "args".to_owned(),
                    |values| {
                        format!(
                            "[{}]",
                            values
                                .iter()
                                .map(|w| words::render(&w.value, true, &self.constants))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    },
                );
                self.push(
                    depth,
                    node,
                    vec![
                        segment("for ", S::Keyword, node),
                        segment(&clause.variable_name, S::Variable, node),
                        segment(" in ", S::Keyword, node),
                        segment(format!("{values}:"), S::String, node),
                    ],
                    true,
                );
                self.list(&clause.body.list, depth + 1);
            }
            ast::CompoundCommand::ArithmeticForClause(clause) => {
                let value = |part: &Option<ast::UnexpandedArithmeticExpr>, fallback: &str| {
                    part.as_ref()
                        .map_or_else(|| fallback.to_owned(), ToString::to_string)
                };
                self.push(depth, node, vec![segment(format!("for _ in arithmetic_loop(init=lambda: {}, condition=lambda: {}, step=lambda: {}):", value(&clause.initializer, "None"), value(&clause.condition, "True"), value(&clause.updater, "None")), S::Keyword, node)], true);
                self.list(&clause.body.list, depth + 1);
            }
            ast::CompoundCommand::CaseClause(clause) => {
                self.push(
                    depth,
                    node,
                    vec![segment(
                        format!(
                            "match {}:",
                            words::render(&clause.value.value, true, &self.constants)
                        ),
                        S::Keyword,
                        node,
                    )],
                    true,
                );
                for case in &clause.cases {
                    self.push(
                        depth + 1,
                        None,
                        vec![segment(
                            format!(
                                "case {}:",
                                case.patterns
                                    .iter()
                                    .map(|w| words::render(&w.value, true, &self.constants))
                                    .collect::<Vec<_>>()
                                    .join(" | ")
                            ),
                            S::Keyword,
                            None,
                        )],
                        true,
                    );
                    if let Some(body) = &case.cmd {
                        self.list(body, depth + 2);
                    }
                    match case.post_action {
                        ast::CaseItemPostAction::ExitCase => {}
                        ast::CaseItemPostAction::UnconditionallyExecuteNextCaseItem => self.push(
                            depth + 2,
                            None,
                            vec![segment("fallthrough", S::Keyword, None)],
                            true,
                        ),
                        ast::CaseItemPostAction::ContinueEvaluatingCases => self.push(
                            depth + 2,
                            None,
                            vec![segment("continue matching", S::Keyword, None)],
                            true,
                        ),
                    }
                }
            }
            ast::CompoundCommand::Coprocess(clause) => {
                self.push(
                    depth,
                    node,
                    vec![segment(
                        format!(
                            "coprocess {}:",
                            clause
                                .name
                                .as_ref()
                                .map_or("COPROC", |name| name.value.as_str())
                        ),
                        S::Keyword,
                        node,
                    )],
                    true,
                );
                self.command(&clause.body, depth + 1);
            }
            ast::CompoundCommand::Arithmetic(arithmetic) => self.push(
                depth,
                node,
                vec![segment(arithmetic.expr.to_string(), S::Variable, node)],
                false,
            ),
        }
    }

    fn condition(&mut self, list: &ast::CompoundList) -> (Option<u32>, Vec<ShellSegment>) {
        let mut first = None;
        let mut segments = Vec::new();
        for item in &list.0 {
            for (index, (op, pipeline)) in (&item.0).into_iter().enumerate() {
                if !segments.is_empty() {
                    segments.push(segment(
                        if index == 0 {
                            "; "
                        } else if matches!(op, ast::PipelineOperator::And) {
                            " and "
                        } else {
                            " or "
                        },
                        S::Operator,
                        None,
                    ));
                }
                if pipeline.bang {
                    segments.push(segment("not ", S::Keyword, None));
                }
                for (i, command) in pipeline.seq.iter().enumerate() {
                    if i > 0 {
                        segments.push(segment(" → ", S::Operator, None));
                    }
                    let node = self.register(command, true);
                    first = node.or(first);
                    segments.extend(self.inline(command, node));
                }
            }
        }
        (first, segments)
    }
}
