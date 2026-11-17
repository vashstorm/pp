//! OMP 18.19 默认 Pseudo 规则的 brush AST 适配；不重新解释命令。

use super::{MAX_DEPTH, MAX_NODES, Model, Node, kind, row, words};
use brush_core::parser::ast::{self, CommandPrefixOrSuffixItem as Item, SourceLocation};
use pp_core::shell_display::{ShellSegment, ShellState};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

mod documents;
mod idioms;
mod statements;
mod traversal;

pub(super) struct Printer<'a> {
    pub(super) model: &'a mut Model,
    unit: u64,
    source: Option<&'a str>,
    base: u16,
    nesting: u16,
    constants: HashMap<String, String>,
    functions: HashSet<String>,
    pub(super) overflow: bool,
}

impl<'a> Printer<'a> {
    pub(super) fn new(model: &'a mut Model, unit: u64, source: Option<&'a str>, base: u16) -> Self {
        Self {
            model,
            unit,
            source,
            base,
            nesting: 0,
            constants: HashMap::new(),
            functions: HashSet::new(),
            overflow: false,
        }
    }

    pub(super) fn program(&mut self, program: &ast::Program) {
        // 只有平直程序中唯一、先赋值后引用的字面量才能内联。
        self.analyze(program);
        for list in &program.complete_commands {
            self.list(list, self.base);
        }
    }

    fn analyze(&mut self, program: &ast::Program) {
        let mut candidates = HashMap::<String, (usize, String)>::new();
        let mut flat = true;
        for list in &program.complete_commands {
            for item in &list.0 {
                if !item.0.additional.is_empty() || matches!(item.1, ast::SeparatorOperator::Async)
                {
                    flat = false;
                }
                for (_, pipeline) in &item.0 {
                    for command in &pipeline.seq {
                        match command {
                            ast::Command::Function(f) => {
                                self.functions.insert(f.fname.value.clone());
                                flat = false;
                            }
                            ast::Command::Simple(s) => {
                                if s.word_or_name.as_ref().is_some_and(|w| {
                                    matches!(
                                        w.value.as_str(),
                                        "eval"
                                            | "source"
                                            | "."
                                            | "read"
                                            | "unset"
                                            | "export"
                                            | "declare"
                                            | "typeset"
                                            | "local"
                                            | "builtin"
                                            | "command"
                                    )
                                }) {
                                    flat = false;
                                }
                                if arguments(s).iter().any(|value| {
                                    value.contains("$(")
                                        || value.contains('`')
                                        || value.contains("${")
                                }) {
                                    flat = false;
                                }
                                for item in items(s) {
                                    if let Item::AssignmentWord(a, _) = item
                                        && let (
                                            ast::AssignmentName::VariableName(name),
                                            ast::AssignmentValue::Scalar(value),
                                        ) = (&a.name, &a.value)
                                    {
                                        let candidate = candidates.entry(name.clone()).or_default();
                                        candidate.0 += 1;
                                        if !a.append
                                            && s.word_or_name.is_none()
                                            && let Some(value) = words::literal(&value.value)
                                        {
                                            candidate.1 = value;
                                        } else {
                                            candidate.0 += 1;
                                        }
                                    }
                                }
                            }
                            _ => flat = false,
                        }
                    }
                }
            }
        }
        if flat {
            // 候选值暂存在表中；遍历到实际 setter 后才允许后续词引用。
            for (name, (count, value)) in candidates {
                if count == 1 {
                    self.constants.insert(format!("\0{name}"), value);
                }
            }
        }
    }

    fn register(&mut self, command: &ast::Command, guard: bool) -> Option<u32> {
        let span = command.location()?;
        let key = (self.unit, span.start.index, span.end.index, kind(command));
        if let Some(node) = self.model.keys.get(&key) {
            return Some(*node);
        }
        if self.model.nodes.len() >= MAX_NODES {
            self.overflow = true;
            return None;
        }
        let node = self.model.nodes.len() as u32;
        self.model.nodes.push(Node {
            unit: self.unit,
            start: span.start.index,
            end: span.end.index,
            state: ShellState::Pending,
            live: 0,
            runs: 0,
            duration: Duration::ZERO,
            status: None,
            iterations: 0,
            looping: matches!(
                command,
                ast::Command::Compound(
                    ast::CompoundCommand::ForClause(_)
                        | ast::CompoundCommand::ArithmeticForClause(_)
                        | ast::CompoundCommand::WhileClause(_)
                        | ast::CompoundCommand::UntilClause(_),
                    _
                )
            ),
            guard,
            leaf: matches!(
                command,
                ast::Command::Simple(_)
                    | ast::Command::ExtendedTest(..)
                    | ast::Command::Compound(ast::CompoundCommand::Arithmetic(_), _)
            ),
            function_form: Vec::new(),
            external_form: Vec::new(),
            builtin_form: Vec::new(),
        });
        self.model.keys.insert(key, node);
        Some(node)
    }

    fn push(&mut self, depth: u16, node: Option<u32>, segments: Vec<ShellSegment>, block: bool) {
        if depth > MAX_DEPTH || self.model.rows.len() >= MAX_NODES {
            self.overflow = true;
            return;
        }
        if !self.overflow && !segments.is_empty() {
            self.model.rows.push(row(depth, node, segments, block));
        }
    }
}

fn inline_command(command: &ast::Command) -> bool {
    matches!(
        command,
        ast::Command::Simple(_)
            | ast::Command::ExtendedTest(..)
            | ast::Command::Compound(ast::CompoundCommand::Arithmetic(_), _)
    )
}

fn items(simple: &ast::SimpleCommand) -> impl Iterator<Item = &Item> {
    simple
        .prefix
        .iter()
        .flat_map(|p| &p.0)
        .chain(simple.suffix.iter().flat_map(|p| &p.0))
}

fn arguments(simple: &ast::SimpleCommand) -> Vec<String> {
    simple
        .word_or_name
        .iter()
        .map(|w| w.value.clone())
        .chain(
            simple
                .suffix
                .iter()
                .flat_map(|p| &p.0)
                .filter_map(|item| match item {
                    Item::Word(w) | Item::AssignmentWord(_, w) => Some(w.value.clone()),
                    Item::ProcessSubstitution(kind, command) => Some(format!("{kind}({command})")),
                    Item::IoRedirect(_) => None,
                }),
        )
        .collect()
}

fn has_redirections(simple: &ast::SimpleCommand) -> bool {
    items(simple).any(|item| matches!(item, Item::IoRedirect(_)))
}
