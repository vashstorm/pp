//! OMP 18.19 默认 Pseudo 规则的 brush AST 适配；不重新解释命令。

use super::{MAX_DEPTH, MAX_NODES, Model, Node, kind, row, segment, words};
use brush_core::parser::{
    Parser,
    ast::{self, CommandPrefixOrSuffixItem as Item, SourceLocation},
};
use pp_core::shell_display::{ShellSegment, ShellState, ShellStyle as S};
use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    time::Duration,
};

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

    fn list(&mut self, list: &ast::CompoundList, depth: u16) {
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

    fn inline(&mut self, command: &ast::Command, node: Option<u32>) -> Vec<ShellSegment> {
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

    fn function_call(&self, simple: &ast::SimpleCommand, node: Option<u32>) -> Vec<ShellSegment> {
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

    fn redirections(&self, redirects: &[ast::IoRedirect], node: Option<u32>) -> Vec<ShellSegment> {
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

    fn documents(&mut self, command: &ast::Command, depth: u16) {
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

    fn heredocs(&mut self, redirects: &[ast::IoRedirect], depth: u16) {
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

fn tail_count(simple: &ast::SimpleCommand) -> Option<(String, u32)> {
    if simple.prefix.is_some() {
        return None;
    }
    let args = arguments(simple)
        .iter()
        .map(|w| words::literal(w))
        .collect::<Option<Vec<_>>>()?;
    let name = args.first()?;
    if !matches!(name.as_str(), "head" | "tail") {
        return None;
    }
    let count = match args.as_slice() {
        [_] => 10,
        [_, count] if count.starts_with('-') && count[1..].chars().all(|c| c.is_ascii_digit()) => {
            count[1..].parse().ok()?
        }
        [_, flag, count]
            if (flag == "-n" || flag == "--lines")
                && !count.is_empty()
                && count.chars().all(|c| c.is_ascii_digit()) =>
        {
            count.parse().ok()?
        }
        [_, count]
            if count.starts_with("--lines=") && count[8..].chars().all(|c| c.is_ascii_digit()) =>
        {
            count[8..].parse().ok()?
        }
        _ => return None,
    };
    Some((name.clone(), count))
}

fn test(values: &[String], constants: &HashMap<String, String>) -> String {
    let raw = values
        .iter()
        .map(|v| words::literal(v).unwrap_or_else(|| v.clone()))
        .collect::<Vec<_>>();
    let render = |i: usize| words::render(values[i].as_str(), true, constants);
    match raw.as_slice() {
        [op, _] => {
            let function = match op.as_str() {
                "-f" => Some("is_file"),
                "-d" => Some("is_dir"),
                "-e" => Some("exists"),
                "-s" => Some("has_content"),
                "-x" => Some("is_executable"),
                "-L" => Some("is_symlink"),
                _ => None,
            };
            if let Some(function) = function {
                format!("{function}({})", render(1))
            } else if op == "-n" || op == "-z" {
                format!(
                    "{} {} \"\"",
                    render(1),
                    if op == "-n" { "!=" } else { "==" }
                )
            } else if op == "!" {
                format!("not {}", render(1))
            } else {
                values.join(" ")
            }
        }
        [_, op, _] => format!(
            "{} {} {}",
            render(0),
            match op.as_str() {
                "=" | "==" | "-eq" => "==",
                "-ne" => "!=",
                "-lt" => "<",
                "-le" => "<=",
                "-gt" => ">",
                "-ge" => ">=",
                other => other,
            },
            render(2)
        ),
        _ => values.join(" "),
    }
}

fn set_meaning(value: &str) -> Option<&'static str> {
    match value {
        "-e" => Some("stop on first error"),
        "+e" => Some("keep going on errors"),
        "-u" => Some("error on unset vars"),
        "-a" => Some("auto-export assignments"),
        "+a" => Some("stop auto-export"),
        "-m" => Some("job control on"),
        "+m" => Some("job control off"),
        "-x" => Some("trace commands"),
        "-euo" => Some("strict mode"),
        _ => None,
    }
}

fn argument_language(name: &str, args: &[String], index: usize) -> Option<&'static str> {
    if index == 0 {
        return None;
    }
    let previous = args.get(index - 1).map(String::as_str).unwrap_or_default();
    if matches!(name, "bash" | "sh" | "zsh") && previous.starts_with('-') && previous.contains('c')
    {
        Some("bash")
    } else if matches!(name, "python" | "python3") && previous == "-c" {
        Some("python")
    } else if matches!(name, "node" | "bun")
        && matches!(previous, "-e" | "--eval" | "-p" | "--print")
    {
        Some("javascript")
    } else if matches!(name, "grep" | "rg" | "egrep") && regex_argument(args, index) {
        Some("regex")
    } else {
        None
    }
}

fn effect(simple: &ast::SimpleCommand) -> String {
    let args = arguments(simple)
        .iter()
        .filter_map(|w| words::literal(w))
        .collect::<Vec<_>>();
    let name = args.first().map_or("", String::as_str);
    let has = |values: &[&str]| {
        args.iter()
            .skip(1)
            .any(|arg| values.contains(&arg.as_str()))
    };
    let mut out = String::new();
    if matches!(name, "curl" | "wget" | "gh" | "ssh" | "scp" | "rsync")
        || name == "git" && has(&["fetch", "push", "pull", "clone"])
    {
        out.push('⇅');
    }
    if matches!(name, "rm" | "kill" | "pkill")
        || name == "git"
            && (has(&["reset"]) && has(&["--hard"])
                || has(&["branch"]) && has(&["-D", "-d", "--delete"])
                || has(&["worktree"]) && has(&["remove"]))
    {
        out.push('×');
    }
    if matches!(
        name,
        "mkdir" | "touch" | "cp" | "mv" | "ln" | "tee" | "install"
    ) || name == "sed" && args.iter().any(|a| a.starts_with("-i"))
        || name == "git"
            && has(&[
                "add",
                "commit",
                "rebase",
                "checkout",
                "switch",
                "merge",
                "cherry-pick",
                "stash",
                "tag",
                "branch",
                "worktree",
            ])
    {
        out.push('✎');
    }
    if matches!(name, "bun" | "npm" | "pnpm" | "yarn") && has(&["install", "add", "i"]) {
        out.push_str("⇅✎");
    }
    if matches!(name, "sudo" | "su" | "runuser" | "chmod" | "chown") {
        out.push('⇪');
    }
    if name == "docker" && has(&["exec", "run", "compose", "start", "stop", "restart"]) {
        out.push('⇉');
    }
    if name == "security"
        && args
            .iter()
            .any(|arg| arg.starts_with("find-") && arg.ends_with("password"))
        || matches!(name, "source" | ".") && args.iter().any(|arg| arg.contains(".env"))
    {
        out.push('◆');
    }
    let writes = items(simple).any(|item| match item {
        Item::IoRedirect(ast::IoRedirect::File(
            _,
            ast::IoFileRedirectKind::Write
            | ast::IoFileRedirectKind::Append
            | ast::IoFileRedirectKind::Clobber
            | ast::IoFileRedirectKind::ReadAndWrite,
            target,
        )) => !target.to_string().starts_with("/dev/"),
        Item::IoRedirect(ast::IoRedirect::OutputAndError(target, _)) => {
            !target.value.contains("/dev/")
        }
        _ => false,
    });
    if writes && !out.contains('✎') {
        out.push('✎');
    }
    out
}

fn pipe_stderr(command: &ast::Command) -> bool {
    if let ast::Command::Simple(simple) = command {
        items(simple).any(|item| {
            matches!(
                item,
                Item::IoRedirect(ast::IoRedirect::File(
                    Some(2),
                    ast::IoFileRedirectKind::DuplicateOutput,
                    ast::IoFileRedirectTarget::Fd(1)
                ))
            )
        })
    } else {
        false
    }
}

fn timeout_duration(value: &str) -> bool {
    let value = value.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(value);
    !value.is_empty()
        && value.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
        && value
            .parse::<f64>()
            .is_ok_and(|value| value.is_finite() && value >= 0.0)
}

fn lone_simple(list: &ast::CompoundList) -> Option<&ast::SimpleCommand> {
    if list.0.len() != 1 || !list.0[0].0.additional.is_empty() || list.0[0].0.first.seq.len() != 1 {
        return None;
    }
    match &list.0[0].0.first.seq[0] {
        ast::Command::Simple(simple) => Some(simple),
        _ => None,
    }
}

fn function_parameters(source: &str) -> String {
    use brush_core::parser::word::{
        self, Parameter, ParameterExpr, WordPiece, WordPieceWithSource,
    };
    fn count(parts: &[WordPieceWithSource]) -> u32 {
        parts
            .iter()
            .map(|part| match &part.piece {
                WordPiece::ParameterExpansion(ParameterExpr::Parameter {
                    parameter: Parameter::Positional(index),
                    indirect: false,
                }) => *index,
                WordPiece::DoubleQuotedSequence(parts) => count(parts),
                _ => 0,
            })
            .max()
            .unwrap_or(0)
    }
    match word::parse(source, &Default::default()) {
        Ok(parts) => {
            let count = count(&parts);
            if count > 64 {
                return "*args".into();
            }
            (1..=count)
                .map(|index| format!("arg{index}"))
                .collect::<Vec<_>>()
                .join(", ")
        }
        Err(_) => "*args".into(),
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

fn regex_argument(args: &[String], index: usize) -> bool {
    let mut at = 1;
    let mut positional = false;
    let mut explicit = false;
    while at < args.len() {
        let arg = args[at].as_str();
        if arg == "--" {
            return !explicit && at + 1 == index;
        }
        if matches!(arg, "-e" | "--regexp") {
            if at + 1 == index {
                return true;
            }
            explicit = true;
            at += 2;
            continue;
        }
        if matches!(arg, "-f" | "--file") {
            explicit = true;
            at += 2;
            continue;
        }
        if matches!(
            arg,
            "-A" | "-B"
                | "-C"
                | "-m"
                | "-g"
                | "-t"
                | "-T"
                | "-M"
                | "-j"
                | "--after-context"
                | "--before-context"
                | "--context"
                | "--max-count"
                | "--glob"
                | "--iglob"
                | "--type"
                | "--type-not"
                | "--encoding"
                | "--max-columns"
                | "--threads"
                | "--color"
                | "--colors"
        ) {
            at += 2;
            continue;
        }
        if !arg.starts_with('-') {
            if !positional && !explicit && at == index {
                return true;
            }
            positional = true;
        }
        at += 1;
    }
    false
}
